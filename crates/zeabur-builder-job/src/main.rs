//! `zeabur-builder-job`: the job image's entry point, one subcommand for each step of a run.
//!
//! ```text
//! zeabur-builder-job fetch archive [--plain-http] <repository> <digest> <dir>
//! zeabur-builder-job plan restore [--plain-http] <repository> <digest> <dir>
//! zeabur-builder-job build <plan file>
//! ```
//!
//! `fetch archive` is the `fetch` container of an archive source: it pulls the archive
//! ([`registry`]) and unpacks it into `<dir>` ([`unpack`]). It writes errors to stderr, which the
//! controller shows as the run's message.
//!
//! `plan restore` is the `plan` container of a run that reuses a saved plan
//! ([`zeabur_builder_core::plan`]): it pulls the plan the same way into `<dir>`. When the plan has
//! the agent's `info.json`, it writes it to `/dev/termination-log` as reused. An error fails the
//! run.
//!
//! `build` is the `build` container. Its one argument is a file with a [`BuildPlan`] as JSON,
//! written by `zeabur-builder-jobspec`. It runs each build in order, prints their `rawjson`
//! progress for people, counts cached steps, records the steps, reads each digest from its metadata
//! file, and writes the result to `/dev/termination-log`, where the controller reads it from the
//! Pod's status ([`zeabur_builder_core::result`]). The same JSON is the last line on stdout. The
//! image is the last build's digest and tags.
//!
//! After a success it reads the pushed image's config from the registry (over HTTP when the plan
//! says `plainHttp`) and reports its `ExposedPorts`; failing to read them does not fail the
//! build.
//!
//! `secretsHashEnv` names variables whose values are hashed into
//! `--opt build-arg:secrets-hash=…` on the first build, so Railpack's frontend invalidates the
//! steps that use a secret when its value changes, without the value entering the cache key.
//!
//! `nonRoot` makes a Railpack image run as a non-root user ([`zeabur_builder_core::railpack`]):
//! it writes the plan folder's `railpack-plan.json`, rewritten, and the second build's
//! `Dockerfile` into `out`, and leaves the plan folder as the planner wrote it. The second build
//! builds on the first.
//!
//! `savePlan` pushes the planner's folder, `/workspace/plan`, after a success, as the plan
//! artifact ([`zeabur_builder_core::plan`]), and the result reports its digest as
//! `planDigest`. Failing to push it does not fail the build; the `Build` keeps the plan it saved
//! before.

mod progress;
mod registry;
mod unpack;

use std::collections::BTreeMap;
use std::io::{self, BufReader, Write as _};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};
use tokio::io::AsyncBufReadExt as _;
use zeabur_builder_core::archive::{self, MAX_UNPACKED_BYTES};
use zeabur_builder_core::build_plan::{Build, BuildPlan, Image};
use zeabur_builder_core::plan::{self, Info, Source};
use zeabur_builder_core::progress::StepLog;
use zeabur_builder_core::railpack;
use zeabur_builder_core::refusal::Refusal;
use zeabur_builder_core::result::{BuildResult, FailureReason, Outcome, PlanSource};

use crate::registry::RegistryError;
use crate::unpack::UnpackError;

const TERMINATION_LOG: &str = "/dev/termination-log";
const WORKSPACE: &str = "/workspace";
const OUT: &str = "/workspace/out";
const PLAN: &str = "/workspace/plan";
/// The registry's CA, PEM (`zeabur_builder_jobspec::REGISTRY_CA_ENV`).
const REGISTRY_CA_ENV: &str = "ZB_REGISTRY_CA";

/// A command line the job cannot use.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
enum UsageError {
    #[error("usage: zeabur-builder-job fetch archive|plan restore|build …")]
    Job,
    #[error("usage: fetch archive|plan restore [--plain-http] <repository> <digest> <dir>")]
    Fetch,
    #[error("usage: build <plan file>")]
    Build,
    #[error("cannot read the build plan {path}: {message}")]
    Plan { path: String, message: String },
    #[error("the build plan has no build")]
    NoBuild,
}

/// Why a step of the job stopped. `fetch` and `plan restore` print the message to stderr, and
/// the controller shows it as the run's message.
#[derive(Debug, thiserror::Error)]
enum StepError {
    #[error(transparent)]
    Usage(#[from] UsageError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Unpack(#[from] UnpackError),
    #[error("cannot read {path}: {source}")]
    Read { path: String, source: io::Error },
    #[error("cannot parse {path}: {source}")]
    ParsePlan {
        path: String,
        source: serde_json::Error,
    },
    #[error("cannot parse {path}: {message}")]
    ParseInfo { path: String, message: String },
    #[error("cannot create {path}: {source}")]
    Create { path: String, source: io::Error },
    #[error("cannot write {path}: {source}")]
    Write { path: String, source: io::Error },
    #[error("cannot write {path}: {source}")]
    EncodePlan {
        path: String,
        source: serde_json::Error,
    },
    #[error("cannot open {path}: {source}")]
    Open { path: String, source: io::Error },
    #[error("cannot pack {dir}: {source}")]
    Pack { dir: String, source: io::Error },
    #[error("{dir} packs to {size} bytes; the limit is {limit}")]
    PackTooLarge {
        dir: String,
        size: usize,
        limit: u64,
    },
    #[error("cannot remove the empty {dir}: {source}")]
    RemoveEmpty { dir: String, source: io::Error },
    #[error("no answer in {0} s")]
    NoAnswer(u64),
    #[error(
        "cannot read the saved plan {digest}: {source}. Set the Build's annotation \
         builder.zeabur.com/plan to fresh to plan again."
    )]
    SavedPlan { digest: String, source: Box<Self> },
}

impl StepError {
    fn read(path: &str) -> impl FnOnce(io::Error) -> Self {
        let path = path.to_owned();
        |source| Self::Read { path, source }
    }

    fn create(path: &str) -> impl FnOnce(io::Error) -> Self {
        let path = path.to_owned();
        |source| Self::Create { path, source }
    }

    fn write(path: &str) -> impl FnOnce(io::Error) -> Self {
        let path = path.to_owned();
        |source| Self::Write { path, source }
    }
}

/// Reads the plan file `build` names.
fn read_plan(args: &[String]) -> Result<BuildPlan, UsageError> {
    let [path] = args else {
        return Err(UsageError::Build);
    };
    let failed = |message: String| UsageError::Plan {
        path: path.clone(),
        message,
    };
    let text = std::fs::read_to_string(path).map_err(|e| failed(e.to_string()))?;
    let plan: BuildPlan = serde_json::from_str(&text).map_err(|e| failed(e.to_string()))?;
    if plan.builds.is_empty() {
        return Err(UsageError::NoBuild);
    }
    Ok(plan)
}

/// SHA-256 over `name NUL value NUL` of each variable, in the order given.
fn secrets_hash(vars: &[(String, String)]) -> String {
    let mut hasher = Sha256::new();
    for (name, value) in vars {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(value.as_bytes());
        hasher.update([0]);
    }
    archive::hex(&hasher.finalize())
}

/// The seconds `fetch` and `plan` recorded in `/workspace/out/<phase>.json`.
fn phase_seconds(phases: &mut BTreeMap<String, f64>) {
    for phase in ["fetch", "plan"] {
        let Ok(text) = std::fs::read_to_string(format!("{OUT}/{phase}.json")) else {
            continue;
        };
        if let Ok(map) = serde_json::from_str::<BTreeMap<String, f64>>(&text) {
            phases.extend(map);
        }
    }
}

/// The file `buildctl --metadata-file` writes.
#[derive(serde::Deserialize)]
struct BuildMetadata {
    #[serde(rename = "containerimage.digest")]
    digest: Option<String>,
}

fn digest(metadata_file: &str) -> Option<String> {
    let text = std::fs::read_to_string(metadata_file).ok()?;
    serde_json::from_str::<BuildMetadata>(&text).ok()?.digest
}

/// How long reading the pushed image's ports may take; the ports are only a hint.
const IMAGE_PORTS_TIMEOUT: Duration = Duration::from_secs(60);

/// How long pushing the plan may take; the plan only saves planning on the next run.
const SAVE_PLAN_TIMEOUT: Duration = Duration::from_secs(60);

/// How long `plan restore` waits for the saved plan.
const RESTORE_PLAN_TIMEOUT: Duration = Duration::from_secs(60);

/// The ports the pushed image declares, read back from the registry the build pushed to.
async fn image_ports(
    repository: &str,
    plain_http: bool,
    digest: &str,
) -> Result<Vec<String>, StepError> {
    let mut repository = open_repository(repository, plain_http)?;
    Ok(repository
        .exposed_ports(digest, IMAGE_PORTS_TIMEOUT)
        .await?)
}

/// A client for `repository` with the credentials in `$DOCKER_CONFIG/config.json`.
fn open_repository(repository: &str, plain_http: bool) -> Result<registry::Repository, StepError> {
    let host = repository.split('/').next().unwrap_or_default();
    let config_path = format!(
        "{}/config.json",
        std::env::var("DOCKER_CONFIG").unwrap_or_default()
    );
    let credentials = match std::fs::read(&config_path) {
        Ok(config) => registry::credentials(&config, host)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(StepError::read(&config_path)(e)),
    };
    let ca = std::env::var(REGISTRY_CA_ENV).ok();
    let client = registry::client(ca.as_deref())?;
    Ok(registry::Repository::new(
        client,
        repository,
        plain_http,
        credentials,
    )?)
}

/// Writes the Railpack plan of `plan`, rewritten, and the second build's Dockerfile into `out`.
fn prepare_non_root(plan: &str, out: &str) -> Result<(), StepError> {
    let path = format!("{plan}/railpack-plan.json");
    let text = std::fs::read_to_string(&path).map_err(StepError::read(&path))?;
    let mut value: serde_json::Value =
        serde_json::from_str(&text).map_err(|source| StepError::ParsePlan {
            path: path.clone(),
            source,
        })?;
    railpack::own_deploy_layers(&mut value);
    std::fs::create_dir_all(out).map_err(StepError::create(out))?;
    let path = format!("{out}/railpack-plan.json");
    let text = serde_json::to_string(&value).map_err(|source| StepError::EncodePlan {
        path: path.clone(),
        source,
    })?;
    std::fs::write(&path, text).map_err(StepError::write(&path))?;
    let dockerfile = format!("{out}/Dockerfile");
    std::fs::write(&dockerfile, railpack::dockerfile()).map_err(StepError::write(&dockerfile))
}

/// Runs one build and returns the digest it reports. `previous` is the digest of the build
/// before.
async fn build(
    build: &Build,
    previous: Option<&str>,
    renderer: &mut progress::Renderer,
    steps: &mut StepLog,
) -> Result<String, Refusal<FailureReason>> {
    let command = build.args(previous);
    let Some((program, rest)) = command.split_first() else {
        return Err(Refusal::new(FailureReason::BuildFailed, "no command"));
    };
    let child = tokio::process::Command::new(program)
        .args(rest)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    renderer.next_run();
    steps.next_run();
    let mut child = child.map_err(|e| {
        Refusal::new(
            FailureReason::BuildFailed,
            format!("cannot run {program}: {e}"),
        )
    })?;
    if let Some(stderr) = child.stderr.take() {
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        let mut out = io::stdout();
        while let Ok(Some(line)) = lines.next_line().await {
            steps.line(&line);
            for text in renderer.line(&line) {
                if writeln!(out, "{text}").is_err() {
                    // stdout gone: keep reading so buildctl is not blocked.
                }
            }
        }
    }
    let status = child.wait().await;
    if !status.as_ref().is_ok_and(std::process::ExitStatus::success) {
        let message = renderer
            .last_error
            .take()
            .unwrap_or_else(|| format!("buildctl failed: {status:?}"));
        return Err(Refusal::new(FailureReason::BuildFailed, message));
    }
    digest(&build.metadata_file).ok_or_else(|| {
        Refusal::new(
            FailureReason::BuildFailed,
            "buildctl succeeded but reported no image digest",
        )
    })
}

async fn run(plan: &BuildPlan) -> BuildResult {
    let mut builds = plan.builds.clone();
    if !plan.secrets_hash_env.is_empty()
        && let Some(first) = builds.first_mut()
    {
        let vars: Vec<(String, String)> = plan
            .secrets_hash_env
            .iter()
            .map(|name| (name.clone(), std::env::var(name).unwrap_or_default()))
            .collect();
        first.command.push("--opt".to_owned());
        first
            .command
            .push(format!("build-arg:secrets-hash={}", secrets_hash(&vars)));
    }
    let mut result = BuildResult {
        plan_source: Some(PlanSource::None),
        ..BuildResult::default()
    };
    phase_seconds(&mut result.phases);
    if result.phases.contains_key("plan") {
        result.plan_source = Some(PlanSource::Generated);
    }
    if let Some(non_root) = &plan.non_root
        && let Err(message) = prepare_non_root(&non_root.plan, &non_root.out)
    {
        result.reason = Some(FailureReason::BuildFailed);
        result.message = message.to_string();
        return result;
    }

    let start = Instant::now();
    let mut renderer = progress::Renderer::default();
    let mut steps = StepLog::default();
    let mut built: Option<String> = None;
    let mut outcome = Ok(());
    for each in &builds {
        match build(each, built.as_deref(), &mut renderer, &mut steps).await {
            Ok(digest) => built = Some(digest),
            Err(failure) => {
                outcome = Err(failure);
                break;
            }
        }
    }
    result
        .phases
        .insert("build".to_owned(), start.elapsed().as_secs_f64());
    result.cache = Some(steps.cache_stats());
    (result.steps, result.failed_step) = steps.finish();
    match (outcome, built) {
        (Ok(()), Some(digest)) => {
            result.result = Outcome::Succeeded;
            result.digest = Some(digest.clone());
            if let Some(last) = builds.last() {
                result.tags.clone_from(&last.output.tags);
                match image_ports(&last.output.repository, plan.plain_http, &digest).await {
                    Ok(ports) => result.exposed_ports = ports,
                    Err(e) => drop(writeln!(
                        io::stderr().lock(),
                        "cannot read the image's exposed ports: {e}"
                    )),
                }
            }
            if let Some(image) = &plan.save_plan {
                match tokio::time::timeout(SAVE_PLAN_TIMEOUT, save_plan(image, plan.plain_http))
                    .await
                {
                    Ok(Ok(digest)) => result.plan_digest = Some(digest),
                    Ok(Err(e)) => drop(writeln!(io::stderr().lock(), "cannot push the plan: {e}")),
                    Err(_elapsed) => drop(writeln!(
                        io::stderr().lock(),
                        "cannot push the plan: no answer in {} s",
                        SAVE_PLAN_TIMEOUT.as_secs()
                    )),
                }
            }
        }
        (Ok(()), None) => {
            result.reason = Some(FailureReason::BuildFailed);
            result.message = String::from("no command");
        }
        (Err(refusal), _) => {
            result.reason = Some(refusal.reason);
            result.message = refusal.message;
        }
    }
    result
}

/// Pushes the planner's folder as the plan artifact `image` and returns its digest.
async fn save_plan(image: &Image, plain_http: bool) -> Result<String, StepError> {
    let layer = pack(PLAN)?;
    let mut repository = open_repository(&image.repository, plain_http)?;
    Ok(repository
        .push_artifact(&image.tag, plan::ARTIFACT_TYPE, layer.into())
        .await?)
}

/// `dir` as a gzip tar, with its entries below `.`, links kept as links, and no owners or times.
fn pack(dir: &str) -> Result<Vec<u8>, StepError> {
    let failed = |source| StepError::Pack {
        dir: dir.to_owned(),
        source,
    };
    let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(gzip);
    tar.mode(tar::HeaderMode::Deterministic);
    tar.follow_symlinks(false);
    tar.append_dir_all(".", dir).map_err(failed)?;
    let packed = tar
        .into_inner()
        .and_then(flate2::write::GzEncoder::finish)
        .map_err(failed)?;
    if u64::try_from(packed.len()).unwrap_or(u64::MAX) > archive::MAX_LAYER_BYTES {
        return Err(StepError::PackTooLarge {
            dir: dir.to_owned(),
            size: packed.len(),
            limit: archive::MAX_LAYER_BYTES,
        });
    }
    Ok(packed)
}

/// The arguments of `fetch archive` and `plan restore`.
#[derive(Debug, PartialEq, Eq)]
struct FetchArgs {
    plain_http: bool,
    repository: String,
    digest: String,
    dir: String,
}

fn parse_fetch(args: &[String]) -> Result<FetchArgs, UsageError> {
    let (plain_http, rest) = match args.split_first() {
        Some((first, rest)) if first == "--plain-http" => (true, rest),
        _ => (false, args),
    };
    let [repository, digest, dir] = rest else {
        return Err(UsageError::Fetch);
    };
    Ok(FetchArgs {
        plain_http,
        repository: repository.clone(),
        digest: digest.clone(),
        dir: dir.clone(),
    })
}

/// Pulls the archive, checks it, and unpacks it into `args.dir`.
async fn fetch_archive(args: &FetchArgs) -> Result<(), StepError> {
    let start = Instant::now();
    for dir in [OUT, PLAN] {
        std::fs::create_dir_all(dir).map_err(StepError::create(dir))?;
    }
    pull(args, archive::ARTIFACT_TYPE, "archive.tar.gz").await?;
    std::fs::write(
        format!("{OUT}/fetch.json"),
        format!("{{\"fetch\":{}}}", start.elapsed().as_secs()),
    )
    .map_err(StepError::write(&format!("{OUT}/fetch.json")))
}

/// Pulls the one layer of the artifact `args.digest` of `artifact_type` into the workspace file
/// `file`, checks it, and unpacks it into `args.dir`.
async fn pull(args: &FetchArgs, artifact_type: &str, file: &str) -> Result<(), StepError> {
    let mut repository = open_repository(&args.repository, args.plain_http)?;
    let file = std::path::Path::new(WORKSPACE).join(file);
    let layer = repository.layer(&args.digest, artifact_type).await?;
    repository.download(&layer, &file).await?;
    let archive = std::fs::File::open(&file).map_err(|source| StepError::Open {
        path: file.display().to_string(),
        source,
    })?;
    unpack::unpack(
        BufReader::new(archive),
        std::path::Path::new(&args.dir),
        MAX_UNPACKED_BYTES,
    )?;
    drop(std::fs::remove_file(&file));
    Ok(())
}

/// Removes `dir` if it is empty, because the unpack makes its own folder. The fetch container
/// makes an empty plan folder for planners. A folder with files is an error.
fn remove_empty(dir: &str) -> Result<(), StepError> {
    match std::fs::remove_dir(dir) {
        Err(source) if source.kind() != io::ErrorKind::NotFound => Err(StepError::RemoveEmpty {
            dir: dir.to_owned(),
            source,
        }),
        Ok(()) | Err(_) => Ok(()),
    }
}

/// Pulls the saved plan into `args.dir` and, when it has the agent's `info.json`, writes it to the
/// termination message as reused, for `BuildRun.status.plan`.
async fn restore_plan(args: &FetchArgs) -> Result<(), StepError> {
    let start = Instant::now();
    remove_empty(&args.dir)?;
    tokio::time::timeout(
        RESTORE_PLAN_TIMEOUT,
        pull(args, plan::ARTIFACT_TYPE, "plan.tar.gz"),
    )
    .await
    .unwrap_or_else(|_elapsed| Err(StepError::NoAnswer(RESTORE_PLAN_TIMEOUT.as_secs())))
    .map_err(|source| StepError::SavedPlan {
        digest: args.digest.clone(),
        source: Box::new(source),
    })?;
    let info = std::path::Path::new(&args.dir).join("info.json");
    if let Ok(text) = std::fs::read_to_string(&info) {
        let mut info = Info::decode(&text).map_err(|message| StepError::ParseInfo {
            path: info.display().to_string(),
            message,
        })?;
        info.source = Some(Source::Previous);
        info.model_called = false;
        info.input_tokens = 0;
        info.output_tokens = 0;
        std::fs::write(TERMINATION_LOG, info.encode())
            .map_err(StepError::write(TERMINATION_LOG))?;
    }
    std::fs::write(
        format!("{OUT}/plan.json"),
        format!("{{\"plan\":{}}}", start.elapsed().as_secs()),
    )
    .map_err(StepError::write(&format!("{OUT}/plan.json")))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let pulled = match raw.as_slice() {
        [step, what, rest @ ..] if step == "fetch" && what == "archive" => {
            Some(match parse_fetch(rest) {
                Ok(args) => fetch_archive(&args).await,
                Err(usage) => Err(usage.into()),
            })
        }
        [step, what, rest @ ..] if step == "plan" && what == "restore" => {
            Some(match parse_fetch(rest) {
                Ok(args) => restore_plan(&args).await,
                Err(usage) => Err(usage.into()),
            })
        }
        [step, ..] if step == "build" => None,
        _ => Some(Err(UsageError::Job.into())),
    };
    if let Some(pulled) = pulled {
        return match pulled {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                drop(writeln!(io::stderr().lock(), "{error}"));
                ExitCode::FAILURE
            }
        };
    }
    let result = match read_plan(raw.get(1..).unwrap_or_default()) {
        Ok(plan) => run(&plan).await,
        Err(usage) => BuildResult {
            reason: Some(FailureReason::BuildFailed),
            message: format!("zeabur-builder-job: {usage}"),
            ..BuildResult::default()
        },
    };
    let encoded = result.encode();
    let written = std::fs::write(TERMINATION_LOG, &encoded);
    let mut out = io::stdout().lock();
    let printed = writeln!(out, "{encoded}");
    if written.is_err() || printed.is_err() || result.result != Outcome::Succeeded {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|&s| s.to_owned()).collect()
    }

    #[test]
    fn reads_the_plan_file() {
        let dir =
            std::env::temp_dir().join(format!("zeabur-builder-plan-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let read = |name: &str, json: &str| {
            let path = dir.join(name);
            std::fs::write(&path, json).unwrap();
            read_plan(&[path.display().to_string()])
        };
        let plan = read(
            "plan.json",
            r#"{"plainHttp":true,"builds":[{"command":["buildctl","build"],"metadataFile":"/m.json","output":{"repository":"r/a"}}]}"#,
        )
        .unwrap();
        assert!(plan.plain_http);
        assert_eq!(plan.builds[0].command, strings(&["buildctl", "build"]));
        assert_eq!(
            read("empty.json", r#"{"builds":[]}"#),
            Err(UsageError::NoBuild)
        );
        assert!(matches!(
            read("old.json", r#"{"builds":[],"commands":[]}"#),
            Err(UsageError::Plan { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(read_plan(&[]), Err(UsageError::Build));
        assert!(matches!(
            read_plan(&strings(&["/nonexistent/plan.json"])),
            Err(UsageError::Plan { .. })
        ));
    }

    #[test]
    fn a_packed_plan_unpacks_to_the_same_files() {
        let dir = std::env::temp_dir().join(format!("zeabur-builder-pack-{}", std::process::id()));
        let plan = dir.join("plan");
        std::fs::create_dir_all(plan.join("sub")).unwrap();
        std::fs::write(plan.join("Dockerfile"), "FROM scratch").unwrap();
        std::fs::write(plan.join("sub/info.json"), "{}").unwrap();
        let packed = pack(plan.to_str().unwrap()).unwrap();
        assert_eq!(packed, pack(plan.to_str().unwrap()).unwrap());

        let out = dir.join("out");
        unpack::unpack(packed.as_slice(), &out, MAX_UNPACKED_BYTES).unwrap();
        let read = |p: &str| std::fs::read_to_string(out.join(p)).unwrap();
        assert_eq!(read("Dockerfile"), "FROM scratch");
        assert_eq!(read("sub/info.json"), "{}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fetch_arguments() {
        assert_eq!(
            parse_fetch(&strings(&[
                "--plain-http",
                "r/a/b/source",
                "sha256:00",
                "/w/src"
            ])),
            Ok(FetchArgs {
                plain_http: true,
                repository: "r/a/b/source".to_owned(),
                digest: "sha256:00".to_owned(),
                dir: "/w/src".to_owned(),
            })
        );
        assert!(parse_fetch(&strings(&["r/a/b/source", "sha256:00"])).is_err());
        assert!(parse_fetch(&strings(&["--plain-http"])).is_err());
    }

    #[test]
    fn non_root_rewrites_the_plan_and_writes_the_dockerfile() {
        let dir = std::env::temp_dir().join(format!("zeabur-builder-job-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plan = serde_json::json!({
            "steps": [{"name": "build", "commands": [{"cmd": "npm ci"}]}],
            "deploy": {"inputs": [{"step": "build", "include": ["."]}]}
        });
        std::fs::write(dir.join("railpack-plan.json"), plan.to_string()).unwrap();
        let out = dir.join("out");
        let (plan_dir, out_dir) = (dir.to_str().unwrap(), out.to_str().unwrap());
        prepare_non_root(plan_dir, out_dir).unwrap();
        let read = |path: std::path::PathBuf| std::fs::read_to_string(path).unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&read(out.join("railpack-plan.json"))).unwrap();
        assert_eq!(
            written["steps"][0]["commands"][1]["cmd"],
            railpack::OWN_COMMAND
        );
        assert_eq!(read(out.join("Dockerfile")), railpack::dockerfile());
        assert_eq!(read(dir.join("railpack-plan.json")), plan.to_string());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(prepare_non_root(plan_dir, out_dir).is_err());
    }

    #[test]
    fn a_restore_takes_the_empty_plan_folder_but_not_one_with_files() {
        let dir = std::env::temp_dir().join(format!("zeabur-builder-plan-{}", std::process::id()));
        let path = dir.to_str().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        remove_empty(path).unwrap();
        assert!(!dir.exists());
        remove_empty(path).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Dockerfile"), "FROM scratch").unwrap();
        assert!(remove_empty(path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn secrets_hash_changes_with_any_value() {
        let a = secrets_hash(&[("A".to_owned(), "1".to_owned())]);
        let b = secrets_hash(&[("A".to_owned(), "2".to_owned())]);
        let c = secrets_hash(&[("A1".to_owned(), String::new())]);
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert_ne!(a, c);
    }
}
