//! `zeabur-builder-job`: the build container's entry point.
//!
//! ```text
//! zeabur-builder-job [--plain-http] [--secrets-hash-env NAME]… [--non-root DIR] -- buildctl … build … [--then buildctl … build …]
//! zeabur-builder-job fetch-archive [--plain-http] <repository> <digest> <dir>
//! ```
//!
//! `fetch-archive` is the `fetch` container of an archive source: it pulls the archive
//! ([`registry`]) and unpacks it into `<dir>` ([`unpack`]). It writes errors to stderr, which the
//! controller shows as the run's message.
//!
//! Runs the `buildctl` commands it is given (built by `zeabur-builder-jobspec`) one after the
//! other, prints their `rawjson` progress for people, counts cached steps, reads each digest from
//! its `--metadata-file`, and writes the result to `/dev/termination-log`, where the controller
//! reads it from the Pod's status ([`zeabur_builder_core::result`]). The same JSON is the last
//! line on stdout. The result is the last command's digest and tags; `{digest}` in a command
//! stands for the digest of the one before it.
//!
//! After a success it reads the pushed image's config from the registry (`--plain-http` if the
//! registry speaks http) and reports its `ExposedPorts`; failing to read them does not fail the
//! build.
//!
//! `--secrets-hash-env` names variables whose values are hashed into
//! `--opt build-arg:secrets-hash=…` on the first command, so Railpack's frontend invalidates the
//! steps that use a secret when its value changes, without the value entering the cache key.
//!
//! `--non-root DIR` makes a Railpack image run as a non-root user
//! ([`zeabur_builder_core::railpack`]): it rewrites `DIR/railpack-plan.json` and writes the second
//! build's `DIR/non-root/Dockerfile`.

mod progress;
mod registry;
mod unpack;

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use sha2::{Digest as _, Sha256};
use zeabur_builder_core::archive::MAX_UNPACKED_BYTES;
use zeabur_builder_core::progress::CacheCounter;
use zeabur_builder_core::railpack;
use zeabur_builder_core::result::{BuildResult, FailureReason, Outcome, PlanSource};

const TERMINATION_LOG: &str = "/dev/termination-log";
const WORKSPACE: &str = "/workspace";
const OUT: &str = "/workspace/out";
const PLAN: &str = "/workspace/plan";
/// The registry's CA, PEM (`zeabur_builder_jobspec::REGISTRY_CA_ENV`).
const REGISTRY_CA_ENV: &str = "ZB_REGISTRY_CA";
/// In a command after `--then`, the digest of the command before it.
const DIGEST: &str = "{digest}";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    plain_http: bool,
    secrets_hash_env: Vec<String>,
    non_root: Option<String>,
    commands: Vec<Vec<String>>,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut secrets_hash_env = Vec::new();
    let mut non_root = None;
    let mut plain_http = false;
    let mut rest = args.iter();
    loop {
        match rest.next().map(String::as_str) {
            Some("--secrets-hash-env") => {
                let name = rest.next().ok_or("--secrets-hash-env needs a name")?;
                secrets_hash_env.push(name.clone());
            }
            Some("--non-root") => {
                non_root = Some(rest.next().ok_or("--non-root needs a directory")?.clone());
            }
            Some("--plain-http") => plain_http = true,
            Some("--") => break,
            Some(other) => return Err(format!("unexpected argument `{other}`")),
            None => return Err("missing `-- buildctl …`".to_owned()),
        }
    }
    let rest: Vec<String> = rest.cloned().collect();
    let commands: Vec<Vec<String>> = rest
        .split(|arg| arg == "--then")
        .map(<[String]>::to_vec)
        .collect();
    if commands.iter().any(Vec::is_empty) {
        return Err("missing a command after `--` or `--then`".to_owned());
    }
    Ok(Args {
        plain_http,
        secrets_hash_env,
        non_root,
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

/// The ports the pushed image declares, read back from the registry the build pushed to.
fn image_ports(repository: &str, plain_http: bool, digest: &str) -> Result<Vec<String>, String> {
    let mut repository = open_repository(repository, plain_http)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(repository.exposed_ports(digest))
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

/// Rewrites the Railpack plan in `dir` and writes the second build's Dockerfile.
fn prepare_non_root(dir: &str) -> Result<(), String> {
    let path = format!("{dir}/railpack-plan.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let mut plan: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("cannot parse {path}: {e}"))?;
    railpack::own_deploy_layers(&mut plan);
    let text = serde_json::to_string(&plan).map_err(|e| format!("cannot write {path}: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("cannot write {path}: {e}"))?;
    let non_root = format!("{dir}/non-root");
    std::fs::create_dir_all(&non_root).map_err(|e| format!("cannot create {non_root}: {e}"))?;
    std::fs::write(format!("{non_root}/Dockerfile"), railpack::dockerfile())
        .map_err(|e| format!("cannot write {non_root}/Dockerfile: {e}"))
}

/// Runs one `buildctl` command and returns the digest it reports.
fn build(
    command: &[String],
    renderer: &mut progress::Renderer,
    counter: &mut CacheCounter,
) -> Result<String, (FailureReason, String)> {
    let Some((program, rest)) = command.split_first() else {
        return Err((FailureReason::BuildFailed, String::from("no command")));
    };
    let child = Command::new(program)
        .args(rest)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = child.map_err(|e| {
        (
            FailureReason::BuildFailed,
            format!("cannot run {program}: {e}"),
        )
    })?;
    if let Some(stderr) = child.stderr.take() {
        let mut out = std::io::stdout().lock();
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            counter.line(&line);
            for text in renderer.line(&line) {
                if writeln!(out, "{text}").is_err() {
                    // stdout gone: keep reading so buildctl is not blocked.
                }
            }
        }
    }
    let status = child.wait();
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

fn run(args: &Args) -> BuildResult {
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
    if let Some(dir) = &args.non_root
        && let Err(message) = prepare_non_root(dir)
    {
        result.reason = Some(FailureReason::BuildFailed);
        result.message = message;
        return result;
    }

    let start = Instant::now();
    let mut renderer = progress::Renderer::default();
    let mut counter = CacheCounter::default();
    let mut built: Option<String> = None;
    let mut outcome = Ok(());
    for command in &commands {
        let command = with_digest(command, built.as_deref());
        match build(&command, &mut renderer, &mut counter) {
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
    match (outcome, built) {
        (Ok(()), Some(digest)) => {
            result.result = Outcome::Succeeded;
            result.digest = Some(digest.clone());
            result.tags = commands.last().map_or_default(|c| tags(c));
            if let Some(repository) = commands.last().and_then(|c| pushed_repository(c)) {
                match image_ports(&repository, args.plain_http, &digest) {
                    Ok(ports) => result.exposed_ports = ports,
                    Err(e) => drop(writeln!(
                        std::io::stderr().lock(),
                        "cannot read the image's exposed ports: {e}"
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

/// The arguments of `fetch-archive`.
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
        return Err("usage: fetch-archive [--plain-http] <repository> <digest> <dir>".to_owned());
    };
    Ok(FetchArgs {
        plain_http,
        repository: repository.clone(),
        digest: digest.clone(),
        dir: dir.clone(),
    })
}

/// Pulls the archive, checks it, and unpacks it into `args.dir`.
fn fetch_archive(args: &FetchArgs) -> Result<(), String> {
    let start = Instant::now();
    for dir in [OUT, PLAN] {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {dir}: {e}"))?;
    }
    let mut repository = open_repository(&args.repository, args.plain_http)?;
    let file = std::path::Path::new(WORKSPACE).join("archive.tar.gz");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let layer = repository.layer(&args.digest).await?;
        repository.download(&layer, &file).await
    })?;
    let archive =
        std::fs::File::open(&file).map_err(|e| format!("cannot open {}: {e}", file.display()))?;
    unpack::unpack(
        BufReader::new(archive),
        std::path::Path::new(&args.dir),
        MAX_UNPACKED_BYTES,
    )?;
    drop(std::fs::remove_file(&file));
    std::fs::write(
        format!("{OUT}/fetch.json"),
        format!("{{\"fetch\":{}}}", start.elapsed().as_secs()),
    )
    .map_err(|e| format!("cannot write {OUT}/fetch.json: {e}"))
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if let Some((first, rest)) = raw.split_first()
        && first == "fetch-archive"
    {
        return match parse_fetch(rest).and_then(|args| fetch_archive(&args)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                drop(writeln!(std::io::stderr().lock(), "{message}"));
                ExitCode::FAILURE
            }
        };
    }
    let result = match parse_args(&raw) {
        Ok(args) => run(&args),
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
                "--",
                "buildctl",
                "build"
            ])),
            Ok(Args {
                plain_http: false,
                secrets_hash_env: strings(&["ZB_SECRET_A"]),
                non_root: None,
                commands: vec![strings(&["buildctl", "build"])],
            })
        );
        assert_eq!(
            parse_args(&strings(&[
                "--non-root",
                "/workspace/plan",
                "--",
                "buildctl",
                "build",
                "--then",
                "buildctl",
                "build",
                "--opt",
                "context:app=docker-image://r/a@{digest}",
            ])),
            Ok(Args {
                plain_http: false,
                secrets_hash_env: Vec::new(),
                non_root: Some("/workspace/plan".to_owned()),
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
        assert!(parse_args(&strings(&["--"])).is_err());
        assert!(parse_args(&strings(&["--secrets-hash-env"])).is_err());
        assert!(parse_args(&strings(&["--non-root"])).is_err());
        assert!(parse_args(&strings(&["--", "buildctl", "--then"])).is_err());
        assert!(parse_args(&strings(&["--", "--then", "buildctl"])).is_err());
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
        prepare_non_root(dir.to_str().unwrap()).unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("railpack-plan.json")).unwrap())
                .unwrap();
        assert_eq!(
            written["steps"][0]["commands"][1]["cmd"],
            railpack::OWN_COMMAND
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("non-root/Dockerfile")).unwrap(),
            railpack::dockerfile()
        );
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(prepare_non_root(dir.to_str().unwrap()).is_err());
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
        let args = parse_args(&strings(&["--plain-http", "--", "buildctl"])).unwrap();
        assert!(args.plain_http);
    }
}
