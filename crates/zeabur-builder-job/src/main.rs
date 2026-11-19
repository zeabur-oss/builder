//! `zeabur-builder-job`: the job image's entry point, one subcommand for each step of a run.
//!
//! ```text
//! zeabur-builder-job fetch archive [--plain-http] <repository> <digest> <dir>
//! zeabur-builder-job plan restore [--plain-http] <repository> <digest> <dir>
//! zeabur-builder-job build [--plain-http] [--secrets-hash-env NAME]… [--non-root PLAN OUT]
//!                          [--save-plan <repository>:<tag>] --build buildctl … [--build buildctl …]
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
//! `build` is the `build` container. It runs each `--build` command (built by
//! `zeabur-builder-jobspec`) in order, prints their `rawjson` progress for people, counts cached
//! steps, records the steps, reads each digest from its `--metadata-file`, and writes the result to
//! `/dev/termination-log`, where the controller reads it from the Pod's status
//! ([`zeabur_builder_core::result`]). The same JSON is the last line on stdout. The image is the
//! last command's digest and tags; `{digest}` in a command stands for the digest of the one before
//! it.
//!
//! After a success it reads the pushed image's config from the registry (`--plain-http` if the
//! registry speaks http) and reports its `ExposedPorts`; failing to read them does not fail the
//! build.
//!
//! `--secrets-hash-env` names variables whose values are hashed into
//! `--opt build-arg:secrets-hash=…` on the first command, so Railpack's frontend invalidates the
//! steps that use a secret when its value changes, without the value entering the cache key.
//!
//! `--non-root PLAN OUT` makes a Railpack image run as a non-root user
//! ([`zeabur_builder_core::railpack`]): it writes `PLAN/railpack-plan.json`, rewritten, and the
//! second build's `Dockerfile` into `OUT`, and leaves the plan folder as the planner wrote it. The
//! second `--build` builds on the first.
//!
//! `--save-plan` pushes the planner's folder, `/workspace/plan`, after a success, as the plan
//! artifact ([`zeabur_builder_core::plan`]) tagged `<tag>`, and the result reports its digest as
//! `planDigest`. Failing to push it does not fail the build; the `Build` keeps the plan it saved
//! before.

mod progress;
mod registry;
mod unpack;

use std::collections::BTreeMap;
use std::io::{BufReader, Write as _};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};
use tokio::io::AsyncBufReadExt as _;
use zeabur_builder_core::archive::{self, MAX_UNPACKED_BYTES};
use zeabur_builder_core::plan::{self, Info, Source};
use zeabur_builder_core::progress::{CacheCounter, StepLog};
use zeabur_builder_core::railpack;
use zeabur_builder_core::result::{BuildResult, FailureReason, Outcome, PlanSource};

const TERMINATION_LOG: &str = "/dev/termination-log";
const WORKSPACE: &str = "/workspace";
const OUT: &str = "/workspace/out";
const PLAN: &str = "/workspace/plan";
/// The registry's CA, PEM (`zeabur_builder_jobspec::REGISTRY_CA_ENV`).
const REGISTRY_CA_ENV: &str = "ZB_REGISTRY_CA";
/// In a `--build` command after the first, the digest of the one before it.
const DIGEST: &str = "{digest}";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    plain_http: bool,
    secrets_hash_env: Vec<String>,
    non_root: Option<(String, String)>,
    save_plan: Option<String>,
    commands: Vec<Vec<String>>,
}

/// Parses the arguments of `build`.
fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut secrets_hash_env = Vec::new();
    let mut non_root = None;
    let mut plain_http = false;
    let mut save_plan = None;
    let mut rest = args.iter();
    loop {
        match rest.next().map(String::as_str) {
            Some("--secrets-hash-env") => {
                let name = rest.next().ok_or("--secrets-hash-env needs a name")?;
                secrets_hash_env.push(name.clone());
            }
            Some("--non-root") => {
                let (Some(plan), Some(out)) = (rest.next(), rest.next()) else {
                    return Err("--non-root needs the plan folder and an output folder".to_owned());
                };
                non_root = Some((plan.clone(), out.clone()));
            }
            Some("--plain-http") => plain_http = true,
            Some("--save-plan") => {
                let name = rest.next().ok_or("--save-plan needs <repository>:<tag>")?;
                save_plan = Some(name.clone());
            }
            Some("--build") => break,
            Some(other) => return Err(format!("unexpected argument `{other}`")),
            None => return Err("missing `--build buildctl …`".to_owned()),
        }
    }
    let rest: Vec<String> = rest.cloned().collect();
    let commands: Vec<Vec<String>> = rest
        .split(|arg| arg == "--build")
        .map(<[String]>::to_vec)
        .collect();
    if commands.iter().any(Vec::is_empty) {
        return Err("missing a command after `--build`".to_owned());
    }
    Ok(Args {
        plain_http,
        secrets_hash_env,
        non_root,
        save_plan,
        commands,
    })
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
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, b| {
            for nibble in [b >> 4, b & 0xF] {
                hex.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
            }
            hex
        })
}

/// The value after `flag` in a command line.
fn flag_value<'a>(command: &'a [String], flag: &str) -> Option<&'a str> {
    command
        .iter()
        .position(|arg| arg == flag)
        .and_then(|i| command.get(i + 1))
        .map(String::as_str)
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

fn digest(metadata_file: Option<&str>) -> Option<String> {
    let text = std::fs::read_to_string(metadata_file?).ok()?;
    let metadata: serde_json::Value = serde_json::from_str(&text).ok()?;
    metadata
        .get("containerimage.digest")?
        .as_str()
        .map(str::to_owned)
}

/// Splits a CSV record as `buildctl` reads `--output` (`encoding/csv`).
fn csv_fields(record: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = record.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut field)),
            c => field.push(c),
        }
    }
    fields.push(field);
    fields
}

/// The tags in `--output type=image,"name=<image>:<tag>,…"`.
fn tags(command: &[String]) -> Vec<String> {
    let Some(output) = flag_value(command, "--output") else {
        return Vec::new();
    };
    csv_fields(output)
        .iter()
        .filter_map(|field| field.strip_prefix("name="))
        .flat_map(|names| names.split(','))
        .filter_map(|name| name.rsplit_once(':').map(|(_, tag)| tag.to_owned()))
        .filter(|tag| !tag.contains('/'))
        .collect()
}

/// The repository (`<host>/<name>`) of the first image name in `--output`.
fn pushed_repository(command: &[String]) -> Option<String> {
    let output = flag_value(command, "--output")?;
    let name = csv_fields(output)
        .iter()
        .find_map(|field| field.strip_prefix("name=").map(str::to_owned))?
        .split(',')
        .next()?
        .to_owned();
    let repository = match name.rsplit_once(':') {
        Some((repository, tag)) if !tag.contains('/') => repository,
        _ => &name,
    };
    Some(repository.to_owned())
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
) -> Result<Vec<String>, String> {
    let mut repository = open_repository(repository, plain_http)?;
    repository.exposed_ports(digest, IMAGE_PORTS_TIMEOUT).await
}

/// A client for `repository` with the credentials in `$DOCKER_CONFIG/config.json`.
fn open_repository(repository: &str, plain_http: bool) -> Result<registry::Repository, String> {
    let host = repository.split('/').next().unwrap_or_default();
    let config_path = format!(
        "{}/config.json",
        std::env::var("DOCKER_CONFIG").unwrap_or_default()
    );
    let credentials = match std::fs::read(&config_path) {
        Ok(config) => registry::credentials(&config, host)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("cannot read {config_path}: {e}")),
    };
    let ca = std::env::var(REGISTRY_CA_ENV).ok();
    let client = registry::client(ca.as_deref())?;
    registry::Repository::new(client, repository, plain_http, credentials)
}

/// Writes the Railpack plan of `plan`, rewritten, and the second build's Dockerfile into `out`.
fn prepare_non_root(plan: &str, out: &str) -> Result<(), String> {
    let path = format!("{plan}/railpack-plan.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let mut value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("cannot parse {path}: {e}"))?;
    railpack::own_deploy_layers(&mut value);
    std::fs::create_dir_all(out).map_err(|e| format!("cannot create {out}: {e}"))?;
    let path = format!("{out}/railpack-plan.json");
    let text = serde_json::to_string(&value).map_err(|e| format!("cannot write {path}: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("cannot write {path}: {e}"))?;
    std::fs::write(format!("{out}/Dockerfile"), railpack::dockerfile())
        .map_err(|e| format!("cannot write {out}/Dockerfile: {e}"))
}

/// Runs one `buildctl` command and returns the digest it reports.
async fn build(
    command: &[String],
    renderer: &mut progress::Renderer,
    counter: &mut CacheCounter,
    steps: &mut StepLog,
) -> Result<String, (FailureReason, String)> {
    let Some((program, rest)) = command.split_first() else {
        return Err((FailureReason::BuildFailed, String::from("no command")));
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
        (
            FailureReason::BuildFailed,
            format!("cannot run {program}: {e}"),
        )
    })?;
    if let Some(stderr) = child.stderr.take() {
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        let mut out = std::io::stdout();
        while let Ok(Some(line)) = lines.next_line().await {
            counter.line(&line);
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
        return Err((FailureReason::BuildFailed, message));
    }
    digest(flag_value(command, "--metadata-file")).ok_or_else(|| {
        (
            FailureReason::BuildFailed,
            String::from("buildctl succeeded but reported no image digest"),
        )
    })
}

async fn run(args: &Args) -> BuildResult {
    let mut commands = args.commands.clone();
    if !args.secrets_hash_env.is_empty()
        && let Some(first) = commands.first_mut()
    {
        let vars: Vec<(String, String)> = args
            .secrets_hash_env
            .iter()
            .map(|name| (name.clone(), std::env::var(name).unwrap_or_default()))
            .collect();
        first.push("--opt".to_owned());
        first.push(format!("build-arg:secrets-hash={}", secrets_hash(&vars)));
    }
    let mut result = BuildResult {
        plan_source: Some(PlanSource::None),
        ..BuildResult::default()
    };
    phase_seconds(&mut result.phases);
    if result.phases.contains_key("plan") {
        result.plan_source = Some(PlanSource::Generated);
    }
    if let Some((plan, out)) = &args.non_root
        && let Err(message) = prepare_non_root(plan, out)
    {
        result.reason = Some(FailureReason::BuildFailed);
        result.message = message;
        return result;
    }

    let start = Instant::now();
    let mut renderer = progress::Renderer::default();
    let mut counter = CacheCounter::default();
    let mut steps = StepLog::default();
    let mut built: Option<String> = None;
    let mut outcome = Ok(());
    for command in &commands {
        let command = with_digest(command, built.as_deref());
        match build(&command, &mut renderer, &mut counter, &mut steps).await {
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
    result.cache = Some(counter.stats());
    (result.steps, result.failed_step) = steps.finish();
    match (outcome, built) {
        (Ok(()), Some(digest)) => {
            result.result = Outcome::Succeeded;
            result.digest = Some(digest.clone());
            result.tags = commands.last().map_or_default(|c| tags(c));
            if let Some(repository) = commands.last().and_then(|c| pushed_repository(c)) {
                match image_ports(&repository, args.plain_http, &digest).await {
                    Ok(ports) => result.exposed_ports = ports,
                    Err(e) => drop(writeln!(
                        std::io::stderr().lock(),
                        "cannot read the image's exposed ports: {e}"
                    )),
                }
            }
            if let Some(name) = &args.save_plan {
                match tokio::time::timeout(SAVE_PLAN_TIMEOUT, save_plan(name, args.plain_http))
                    .await
                {
                    Ok(Ok(digest)) => result.plan_digest = Some(digest),
                    Ok(Err(e)) => drop(writeln!(
                        std::io::stderr().lock(),
                        "cannot push the plan: {e}"
                    )),
                    Err(_elapsed) => drop(writeln!(
                        std::io::stderr().lock(),
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
        (Err((reason, message)), _) => {
            result.reason = Some(reason);
            result.message = message;
        }
    }
    result
}

/// Pushes the planner's folder as the plan artifact `name` (`<repository>:<tag>`) and returns
/// its digest.
async fn save_plan(name: &str, plain_http: bool) -> Result<String, String> {
    let (repository, tag) = name
        .rsplit_once(':')
        .filter(|(_, tag)| !tag.contains('/'))
        .ok_or_else(|| format!("`{name}` is not <repository>:<tag>"))?;
    let layer = pack(PLAN)?;
    let mut repository = open_repository(repository, plain_http)?;
    repository
        .push_artifact(tag, plan::ARTIFACT_TYPE, layer.into())
        .await
}

/// `dir` as a gzip tar, with its entries below `.`, links kept as links, and no owners or times.
fn pack(dir: &str) -> Result<Vec<u8>, String> {
    let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(gzip);
    tar.mode(tar::HeaderMode::Deterministic);
    tar.follow_symlinks(false);
    tar.append_dir_all(".", dir)
        .map_err(|e| format!("cannot pack {dir}: {e}"))?;
    let packed = tar
        .into_inner()
        .and_then(flate2::write::GzEncoder::finish)
        .map_err(|e| format!("cannot pack {dir}: {e}"))?;
    if u64::try_from(packed.len()).unwrap_or(u64::MAX) > archive::MAX_LAYER_BYTES {
        return Err(format!(
            "{dir} packs to {} bytes; the limit is {}",
            packed.len(),
            archive::MAX_LAYER_BYTES
        ));
    }
    Ok(packed)
}

/// `command` with [`DIGEST`] replaced by the previous command's digest.
fn with_digest(command: &[String], previous: Option<&str>) -> Vec<String> {
    previous.map_or_else(
        || command.to_vec(),
        |digest| {
            command
                .iter()
                .map(|arg| arg.replace(DIGEST, digest))
                .collect()
        },
    )
}

/// The arguments of `fetch archive` and `plan restore`.
#[derive(Debug, PartialEq, Eq)]
struct FetchArgs {
    plain_http: bool,
    repository: String,
    digest: String,
    dir: String,
}

fn parse_fetch(args: &[String]) -> Result<FetchArgs, String> {
    let (plain_http, rest) = match args.split_first() {
        Some((first, rest)) if first == "--plain-http" => (true, rest),
        _ => (false, args),
    };
    let [repository, digest, dir] = rest else {
        return Err(
            "usage: fetch archive|plan restore [--plain-http] <repository> <digest> <dir>"
                .to_owned(),
        );
    };
    Ok(FetchArgs {
        plain_http,
        repository: repository.clone(),
        digest: digest.clone(),
        dir: dir.clone(),
    })
}

/// Pulls the archive, checks it, and unpacks it into `args.dir`.
async fn fetch_archive(args: &FetchArgs) -> Result<(), String> {
    let start = Instant::now();
    for dir in [OUT, PLAN] {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {dir}: {e}"))?;
    }
    pull(args, archive::ARTIFACT_TYPE, "archive.tar.gz").await?;
    std::fs::write(
        format!("{OUT}/fetch.json"),
        format!("{{\"fetch\":{}}}", start.elapsed().as_secs()),
    )
    .map_err(|e| format!("cannot write {OUT}/fetch.json: {e}"))
}

/// Pulls the one layer of the artifact `args.digest` of `artifact_type` into the workspace file
/// `file`, checks it, and unpacks it into `args.dir`.
async fn pull(args: &FetchArgs, artifact_type: &str, file: &str) -> Result<(), String> {
    let mut repository = open_repository(&args.repository, args.plain_http)?;
    let file = std::path::Path::new(WORKSPACE).join(file);
    let layer = repository.layer(&args.digest, artifact_type).await?;
    repository.download(&layer, &file).await?;
    let archive =
        std::fs::File::open(&file).map_err(|e| format!("cannot open {}: {e}", file.display()))?;
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
fn remove_empty(dir: &str) -> Result<(), String> {
    match std::fs::remove_dir(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("cannot remove the empty {dir}: {e}"))
        }
        Ok(()) | Err(_) => Ok(()),
    }
}

/// Pulls the saved plan into `args.dir` and, when it has the agent's `info.json`, writes it to the
/// termination message as reused, for `BuildRun.status.plan`.
async fn restore_plan(args: &FetchArgs) -> Result<(), String> {
    let start = Instant::now();
    remove_empty(&args.dir)?;
    tokio::time::timeout(
        RESTORE_PLAN_TIMEOUT,
        pull(args, plan::ARTIFACT_TYPE, "plan.tar.gz"),
    )
    .await
    .unwrap_or_else(|_elapsed| Err(format!("no answer in {} s", RESTORE_PLAN_TIMEOUT.as_secs())))
    .map_err(|e| {
        format!(
            "cannot read the saved plan {}: {e}. Set the Build's annotation \
             builder.zeabur.com/plan to fresh to plan again.",
            args.digest
        )
    })?;
    let info = std::path::Path::new(&args.dir).join("info.json");
    if let Ok(text) = std::fs::read_to_string(&info) {
        let mut info =
            Info::decode(&text).map_err(|e| format!("cannot parse {}: {e}", info.display()))?;
        info.source = Some(Source::Previous);
        info.model_called = false;
        info.input_tokens = 0;
        info.output_tokens = 0;
        std::fs::write(TERMINATION_LOG, info.encode())
            .map_err(|e| format!("cannot write {TERMINATION_LOG}: {e}"))?;
    }
    std::fs::write(
        format!("{OUT}/plan.json"),
        format!("{{\"plan\":{}}}", start.elapsed().as_secs()),
    )
    .map_err(|e| format!("cannot write {OUT}/plan.json: {e}"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let pulled = match raw.as_slice() {
        [step, what, rest @ ..] if step == "fetch" && what == "archive" => {
            Some(match parse_fetch(rest) {
                Ok(args) => fetch_archive(&args).await,
                Err(message) => Err(message),
            })
        }
        [step, what, rest @ ..] if step == "plan" && what == "restore" => {
            Some(match parse_fetch(rest) {
                Ok(args) => restore_plan(&args).await,
                Err(message) => Err(message),
            })
        }
        [step, ..] if step == "build" => None,
        _ => Some(Err(
            "usage: zeabur-builder-job fetch archive|plan restore|build …".to_owned(),
        )),
    };
    if let Some(pulled) = pulled {
        return match pulled {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                drop(writeln!(std::io::stderr().lock(), "{message}"));
                ExitCode::FAILURE
            }
        };
    }
    let result = match parse_args(raw.get(1..).unwrap_or_default()) {
        Ok(args) => run(&args).await,
        Err(message) => BuildResult {
            reason: Some(FailureReason::BuildFailed),
            message: format!("zeabur-builder-job: {message}"),
            ..BuildResult::default()
        },
    };
    let encoded = result.encode();
    let written = std::fs::write(TERMINATION_LOG, &encoded);
    let mut out = std::io::stdout().lock();
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
    fn arguments() {
        assert_eq!(
            parse_args(&strings(&[
                "--secrets-hash-env",
                "ZB_SECRET_A",
                "--build",
                "buildctl",
                "build"
            ])),
            Ok(Args {
                plain_http: false,
                secrets_hash_env: strings(&["ZB_SECRET_A"]),
                non_root: None,
                save_plan: None,
                commands: vec![strings(&["buildctl", "build"])],
            })
        );
        assert_eq!(
            parse_args(&strings(&[
                "--non-root",
                "/workspace/plan",
                "/workspace/out/non-root",
                "--save-plan",
                "r/a:zeabur-plan-1",
                "--build",
                "buildctl",
                "build",
                "--build",
                "buildctl",
                "build",
                "--opt",
                "context:app=docker-image://r/a@{digest}",
            ])),
            Ok(Args {
                plain_http: false,
                secrets_hash_env: Vec::new(),
                non_root: Some((
                    "/workspace/plan".to_owned(),
                    "/workspace/out/non-root".to_owned()
                )),
                save_plan: Some("r/a:zeabur-plan-1".to_owned()),
                commands: vec![
                    strings(&["buildctl", "build"]),
                    strings(&[
                        "buildctl",
                        "build",
                        "--opt",
                        "context:app=docker-image://r/a@{digest}"
                    ]),
                ],
            })
        );
        assert!(parse_args(&strings(&["buildctl"])).is_err());
        assert!(parse_args(&strings(&["--build"])).is_err());
        assert!(parse_args(&strings(&["--save-plan"])).is_err());
        assert!(parse_args(&strings(&["--secrets-hash-env"])).is_err());
        assert!(parse_args(&strings(&["--non-root"])).is_err());
        assert!(parse_args(&strings(&["--build", "buildctl", "--build"])).is_err());
        assert!(parse_args(&strings(&["--build", "--build", "buildctl"])).is_err());
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
    fn a_later_command_names_the_image_the_one_before_built() {
        let command = strings(&["--opt", "context:app=docker-image://r/a@{digest}"]);
        assert_eq!(with_digest(&command, None), command);
        assert_eq!(
            with_digest(&command, Some("sha256:abc")),
            strings(&["--opt", "context:app=docker-image://r/a@sha256:abc"])
        );
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

    #[test]
    fn tags_from_the_output() {
        let quoted = strings(&[
            "--output",
            "type=image,\"name=r.local:5000/a/api:main-1-abc,r.local:5000/a/api:latest\",push=true",
        ]);
        assert_eq!(tags(&quoted), ["main-1-abc", "latest"]);
        let single = strings(&[
            "--output",
            "type=image,name=r.local:5000/a/api:v1,push=true",
        ]);
        assert_eq!(tags(&single), ["v1"]);
        assert_eq!(tags(&strings(&["build"])), Vec::<String>::new());
    }

    #[test]
    fn pushed_repository_from_the_output() {
        let quoted = strings(&[
            "--output",
            "type=image,\"name=r.local:5000/a/api:main-1-abc,r.local:5000/a/api:latest\",push=true",
        ]);
        assert_eq!(
            pushed_repository(&quoted).as_deref(),
            Some("r.local:5000/a/api")
        );
        let untagged = strings(&["--output", "type=image,name=r.local:5000/a/api,push=true"]);
        assert_eq!(
            pushed_repository(&untagged).as_deref(),
            Some("r.local:5000/a/api")
        );
        assert_eq!(pushed_repository(&strings(&["build"])), None);
    }

    #[test]
    fn plain_http_flag() {
        let args = parse_args(&strings(&["--plain-http", "--build", "buildctl"])).unwrap();
        assert!(args.plain_http);
    }
}
