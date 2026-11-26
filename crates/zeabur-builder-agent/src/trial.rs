//! Trial builds: `buildctl build` of a Dockerfile against the repository. The image goes to the
//! agent as an OCI tarball on `buildctl`'s stdout, and the agent reads only its config: a build
//! whose image does not start under `runAsNonRoot` fails ([`crate::image`]).
//!
//! BuildKit enforces what a build may do, not this crate:
//!
//! - `--source-policy-file` denies every source, then allows `local://context`,
//!   `local://dockerfile` and images of the allowed registries. BuildKit evaluates the rules in
//!   order and the last match wins, against every source the frontend resolves (`FROM`, `COPY
//!   --from`, `RUN --mount=from=`, `ADD` of a URL or Git repository).
//! - `--opt cmdline=dockerfile.v0` makes the bundled Dockerfile frontend skip a `# syntax`
//!   directive or `BUILDKIT_SYNTAX`, so no other frontend image runs
//!   ([`frontend/dockerfile/builder/build.go`]).
//! - No `--allow`: without entitlements BuildKit refuses `RUN --network=host` and
//!   `--security=insecure`.
//!
//! The Dockerfile's `.dockerignore` is written next to it as `Dockerfile.dockerignore`, which the
//! frontend reads before the context's own ([`frontend/dockerui/config.go`]), and ends with the
//! credential rules of [`dockerignore`].
//!
//! [`frontend/dockerfile/builder/build.go`]: https://github.com/moby/buildkit/blob/d1e95fa/frontend/dockerfile/builder/build.go
//! [`frontend/dockerui/config.go`]: https://github.com/moby/buildkit/blob/d1e95fa/frontend/dockerui/config.go

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;
use zeabur_builder_core::progress::SolveStatus;

use crate::image;
use crate::repo::dockerignore;

/// The largest Dockerfile a build accepts.
pub(crate) const MAX_DOCKERFILE: usize = 64 * 1024;
/// The output of the failed step the summary keeps.
const SUMMARY_OUTPUT: usize = 4 * 1024;
/// The full log kept for `read_build_log`.
const MAX_LOG: usize = 4 << 20;

/// How one trial build ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Attempt {
    /// It built.
    Built,
    /// It failed: a summary for the model, and the full log to page through.
    Failed {
        /// The failing step, its error and its last output.
        summary: String,
        /// Every step's output.
        log: String,
    },
}

/// Something that trial-builds a Dockerfile.
pub(crate) trait TrialBuild: Send + Sync {
    /// Builds `dockerfile` with the `.dockerignore` rules `dockerignore`.
    fn build(&self, dockerfile: &str, dockerignore: &str) -> impl Future<Output = Attempt> + Send;

    /// The names of the build variables each build gets as secrets.
    fn variables(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Trial builds on a buildkitd.
#[derive(Debug, Clone)]
pub(crate) struct Buildctl {
    /// `--addr`, such as `tcp://buildkitd:1234`.
    pub(crate) addr: String,
    /// `--tlsdir`, with `ca.pem`, `cert.pem` and `key.pem`.
    pub(crate) tlsdir: Option<PathBuf>,
    /// The repository.
    pub(crate) context: PathBuf,
    /// Where each attempt's Dockerfile and policy are written.
    pub(crate) work: PathBuf,
    /// Registries images may come from, such as `docker.io`.
    pub(crate) registries: Vec<String>,
    /// The longest one build may take.
    pub(crate) timeout: Duration,
    /// The build variables, passed as secrets.
    pub(crate) secrets: Vec<Secret>,
}

/// A build variable, which a build reads with `RUN --mount=type=secret,id=<name>`.
#[derive(Debug, Clone)]
pub(crate) struct Secret {
    pub(crate) name: String,
    pub(crate) value: String,
}

/// The prefix of the variables that hold the build variables' values, in the Job and for
/// `buildctl`.
pub(crate) const SECRET_ENV_PREFIX: &str = "ZB_SECRET_";

impl Buildctl {
    /// The `buildctl` arguments of one attempt.
    fn args(&self) -> Vec<String> {
        let mut args = vec!["--addr".to_owned(), self.addr.clone()];
        if let Some(tlsdir) = &self.tlsdir {
            args.extend(["--tlsdir".to_owned(), tlsdir.display().to_string()]);
        }
        args.extend(
            [
                "build",
                "--progress",
                "rawjson",
                "--frontend",
                "dockerfile.v0",
                "--opt",
                "cmdline=dockerfile.v0",
                "--opt",
                "filename=Dockerfile",
                "--output",
                "type=oci,dest=-",
            ]
            .map(str::to_owned),
        );
        args.extend([
            "--local".to_owned(),
            format!("context={}", self.context.display()),
            "--local".to_owned(),
            format!("dockerfile={}", self.work.display()),
            "--source-policy-file".to_owned(),
            self.work.join("policy.json").display().to_string(),
        ]);
        args.extend(self.secrets.iter().flat_map(|secret| {
            [
                "--secret".to_owned(),
                format!("id={},env={SECRET_ENV_PREFIX}{}", secret.name, secret.name),
            ]
        }));
        args
    }

    async fn attempt(&self, dockerfile: &str, dockerignore: &str) -> Attempt {
        if dockerfile.len() > MAX_DOCKERFILE {
            return failed(format!("the Dockerfile is over {MAX_DOCKERFILE} bytes"));
        }
        if let Err(e) = self.write(dockerfile, dockerignore).await {
            return failed(format!("cannot write the Dockerfile: {e}"));
        }
        let child = Command::new("buildctl")
            .args(self.args())
            .envs(
                self.secrets
                    .iter()
                    .map(|secret| (format!("{SECRET_ENV_PREFIX}{}", secret.name), &secret.value)),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(e) => return failed(format!("cannot run buildctl: {e}")),
        };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return failed("buildctl has no stdout or stderr".to_owned());
        };
        let image = match stdout.into_owned_fd() {
            Ok(fd) => tokio::task::spawn_blocking(move || image::user(std::fs::File::from(fd))),
            Err(e) => return failed(format!("cannot read buildctl's output: {e}")),
        };
        let mut progress = Progress::new(Redactor::new(&self.secrets));
        let run = async {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                progress.line(&line);
            }
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((status, image.await))
        };
        match tokio::time::timeout(self.timeout, run).await {
            Ok(Ok((status, user))) if status.success() => match user {
                Ok(Ok(user)) => {
                    image::runs_as_root(&user).map_or(Attempt::Built, |summary| Attempt::Failed {
                        summary,
                        log: progress.log,
                    })
                }
                Ok(Err(e)) => failed(format!("cannot read the image config: {e}")),
                Err(e) => failed(format!("cannot read the image config: {e}")),
            },
            Ok(Ok(_)) => progress.failure(),
            Ok(Err(e)) => failed(format!("buildctl failed: {e}")),
            Err(_) => {
                let mut attempt = progress.failure();
                if let Attempt::Failed { summary, .. } = &mut attempt {
                    *summary = format!(
                        "the build took longer than {} s and was stopped\n{summary}",
                        self.timeout.as_secs()
                    );
                }
                attempt
            }
        }
    }

    async fn write(&self, dockerfile: &str, rules: &str) -> std::io::Result<()> {
        tokio::fs::create_dir_all(&self.work).await?;
        tokio::fs::write(self.work.join("Dockerfile"), dockerfile).await?;
        tokio::fs::write(
            self.work.join("Dockerfile.dockerignore"),
            dockerignore(rules),
        )
        .await?;
        let policy =
            serde_json::to_vec(&source_policy(&self.registries)).map_err(std::io::Error::other)?;
        tokio::fs::write(self.work.join("policy.json"), policy).await
    }
}

impl TrialBuild for Buildctl {
    async fn build(&self, dockerfile: &str, dockerignore: &str) -> Attempt {
        self.attempt(dockerfile, dockerignore).await
    }

    fn variables(&self) -> Vec<String> {
        self.secrets
            .iter()
            .map(|secret| secret.name.clone())
            .collect()
    }
}

fn failed(summary: String) -> Attempt {
    Attempt::Failed {
        log: summary.clone(),
        summary,
    }
}

/// A BuildKit source policy (`sourcepolicy/pb.Policy`), as `buildctl` reads it with Go's
/// `encoding/json`: enums are numbers.
#[derive(Debug, Serialize)]
struct Policy {
    version: i64,
    rules: Vec<Rule>,
}

#[derive(Debug, Serialize)]
struct Rule {
    /// `ALLOW` is 0, `DENY` is 1.
    action: i32,
    selector: Selector,
}

#[derive(Debug, Serialize)]
struct Selector {
    identifier: String,
    /// `WILDCARD` is 0, `EXACT` is 1.
    match_type: i32,
}

const ALLOW: i32 = 0;
const DENY: i32 = 1;
const WILDCARD: i32 = 0;
const EXACT: i32 = 1;

/// Deny everything, then allow the two local sources and images of `registries`.
fn source_policy(registries: &[String]) -> Policy {
    let rule = |action, identifier: String, match_type| Rule {
        action,
        selector: Selector {
            identifier,
            match_type,
        },
    };
    let rules = std::iter::once(rule(DENY, "*".to_owned(), WILDCARD))
        .chain(
            ["context", "dockerfile"].map(|local| rule(ALLOW, format!("local://{local}"), EXACT)),
        )
        .chain(
            registries
                .iter()
                .map(|registry| rule(ALLOW, format!("docker-image://{registry}/*"), WILDCARD)),
        )
        .collect();
    Policy { version: 1, rules }
}

/// Replaces each secret value in build output with `[secret NAME]`, so the model never reads one.
#[derive(Debug, Default)]
struct Redactor {
    /// Value and replacement, the longest value first.
    secrets: Vec<(Vec<u8>, String)>,
}

impl Redactor {
    fn new(secrets: &[Secret]) -> Self {
        let mut secrets: Vec<_> = secrets
            .iter()
            .filter(|s| !s.value.is_empty())
            .map(|s| (s.value.as_bytes().to_vec(), format!("[secret {}]", s.name)))
            .collect();
        secrets.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
        Self { secrets }
    }

    /// Redacts `text` in one pass, the longest value first where values overlap. While `more`
    /// output follows, a tail that can still grow into a value is not redacted and is returned.
    fn redact<'a>(&self, text: &'a [u8], more: bool) -> (Vec<u8>, &'a [u8]) {
        let mut out = Vec::with_capacity(text.len());
        let mut rest = text;
        while let Some((&first, tail)) = rest.split_first() {
            if more
                && self
                    .secrets
                    .iter()
                    .any(|(value, _)| value.len() > rest.len() && value.starts_with(rest))
            {
                return (out, rest);
            }
            if let Some((value, name)) = self.secrets.iter().find(|(v, _)| rest.starts_with(v)) {
                out.extend_from_slice(name.as_bytes());
                rest = rest.get(value.len()..).unwrap_or_default();
            } else {
                out.push(first);
                rest = tail;
            }
        }
        (out, rest)
    }

    fn text(&self, text: &str) -> String {
        String::from_utf8_lossy(&self.redact(text.as_bytes(), false).0).into_owned()
    }
}

/// What a `--progress rawjson` stream said about a build.
#[derive(Debug, Default)]
struct Progress {
    redactor: Redactor,
    /// Digest and stream → output not yet redacted, since it can still grow into a secret value.
    pending: BTreeMap<(String, u8), Vec<u8>>,
    /// Digest → step name, in the order steps first appeared.
    names: BTreeMap<String, String>,
    /// Digest → output.
    output: BTreeMap<String, String>,
    /// The first step that failed: digest and error.
    failed: Option<(String, String)>,
    /// `buildctl`'s own messages, such as a refused source.
    messages: String,
    /// Every step's output, in order, at most [`MAX_LOG`] bytes.
    log: String,
}

impl Progress {
    fn new(redactor: Redactor) -> Self {
        Self {
            redactor,
            ..Self::default()
        }
    }

    fn line(&mut self, line: &str) {
        let Some(status) = SolveStatus::parse(line) else {
            let line = self.redactor.text(line);
            push_bounded(&mut self.messages, &line, SUMMARY_OUTPUT);
            push_bounded(&mut self.log, &line, MAX_LOG);
            return;
        };
        for vertex in status.vertexes {
            if !vertex.name.is_empty() && !self.names.contains_key(&vertex.digest) {
                push_bounded(&mut self.log, &format!("#{}", vertex.name), MAX_LOG);
                self.names.insert(vertex.digest.clone(), vertex.name);
            }
            if let Some(error) = vertex.error
                && self.failed.is_none()
            {
                self.failed = Some((vertex.digest, self.redactor.text(&error)));
            }
        }
        for log in status.logs {
            let Ok(data) = base64::engine::general_purpose::STANDARD.decode(&log.data) else {
                continue;
            };
            let pending = self
                .pending
                .entry((log.vertex.clone(), log.stream))
                .or_default();
            pending.extend_from_slice(&data);
            let (text, rest) = self.redactor.redact(pending, true);
            *pending = rest.to_vec();
            self.push(log.vertex, &text);
        }
    }

    /// Adds redacted output of the step `digest`.
    fn push(&mut self, digest: String, text: &[u8]) {
        if text.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(text);
        push_bounded(&mut self.log, text.trim_end(), MAX_LOG);
        let output = self.output.entry(digest).or_default();
        output.push_str(&text);
        keep_tail(output, SUMMARY_OUTPUT);
    }

    /// The failure: the failing step, its error and the end of its output.
    fn failure(mut self) -> Attempt {
        for ((digest, _), pending) in std::mem::take(&mut self.pending) {
            let text = self.redactor.redact(&pending, false).0;
            self.push(digest, &text);
        }
        let mut summary = String::new();
        if let Some((digest, error)) = &self.failed {
            let name = self.names.get(digest).map_or("(unnamed)", String::as_str);
            writeln!(summary, "failed step: {name}\nerror: {error}").unwrap_or_default();
            if let Some(output) = self.output.get(digest) {
                write!(summary, "last output:\n{}", output.trim_end()).unwrap_or_default();
            }
        } else {
            write!(summary, "buildctl failed:\n{}", self.messages.trim_end()).unwrap_or_default();
        }
        Attempt::Failed {
            summary,
            log: self.log,
        }
    }
}

/// Appends `line` and a newline to `text` while it stays within `max` bytes.
fn push_bounded(text: &mut String, line: &str, max: usize) {
    if text.len() + line.len() < max {
        text.push_str(line);
        text.push('\n');
    }
}

/// Drops the start of `text` so it is at most `max` bytes.
fn keep_tail(text: &mut String, max: usize) {
    if text.len() > max {
        let mut cut = text.len() - max;
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        text.drain(..cut);
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    fn line(value: &impl Serialize) -> String {
        serde_json::to_string(value).unwrap()
    }

    fn b64(text: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(text)
    }

    #[test]
    fn the_policy_denies_first_and_allows_after() {
        let policy = serde_json::to_value(source_policy(&["docker.io".to_owned()])).unwrap();
        assert_eq!(
            policy,
            json!({"version": 1, "rules": [
                {"action": 1, "selector": {"identifier": "*", "match_type": 0}},
                {"action": 0, "selector": {"identifier": "local://context", "match_type": 1}},
                {"action": 0, "selector": {"identifier": "local://dockerfile", "match_type": 1}},
                {"action": 0, "selector": {"identifier": "docker-image://docker.io/*", "match_type": 0}},
            ]})
        );
    }

    #[test]
    fn pins_the_bundled_frontend_without_entitlements() {
        let buildctl = Buildctl {
            addr: "tcp://buildkitd:1234".to_owned(),
            tlsdir: None,
            context: "/workspace/src".into(),
            work: "/workspace/plan".into(),
            registries: vec![],
            timeout: Duration::from_secs(1),
            secrets: vec![],
        };
        let args = buildctl.args();
        assert!(
            args.windows(2)
                .any(|w| w == ["--opt", "cmdline=dockerfile.v0"])
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--frontend", "dockerfile.v0"])
        );
        assert!(!args.iter().any(|a| a.starts_with("--allow")));
    }

    #[test]
    fn passes_build_variables_as_secrets_and_keeps_their_values_from_the_model() {
        let buildctl = Buildctl {
            addr: "tcp://buildkitd:1234".to_owned(),
            tlsdir: None,
            context: "/workspace/src".into(),
            work: "/workspace/plan".into(),
            registries: vec![],
            timeout: Duration::from_secs(1),
            secrets: vec![Secret {
                name: "NPM_TOKEN".to_owned(),
                value: "npm_s3cret".to_owned(),
            }],
        };
        assert!(
            buildctl
                .args()
                .windows(2)
                .any(|w| w == ["--secret", "id=NPM_TOKEN,env=ZB_SECRET_NPM_TOKEN"])
        );
        assert_eq!(buildctl.variables(), ["NPM_TOKEN"]);
    }

    fn redactor(secrets: &[(&str, &str)]) -> Redactor {
        let secrets: Vec<Secret> = secrets
            .iter()
            .map(|(name, value)| Secret {
                name: (*name).to_owned(),
                value: (*value).to_owned(),
            })
            .collect();
        Redactor::new(&secrets)
    }

    #[test]
    fn a_secret_split_across_log_records_stays_hidden() {
        let mut progress = Progress::new(redactor(&[("NPM_TOKEN", "npm_s3cret")]));
        // stdout is stream 1 and stderr stream 2.
        for (stream, part) in [
            (1, "401 for npm_s3"),
            (2, "warning\n"),
            (1, "cret\n"),
            (1, "next line\n"),
        ] {
            progress.line(&line(
                &json!({"logs": [{"vertex": "b", "stream": stream, "data": b64(part)}]}),
            ));
        }
        progress.line(&line(
            &json!({"logs": [{"vertex": "c", "data": b64("npm_s")}]}),
        ));
        progress.line(&line(&json!({"vertexes": [
            {"digest": "b", "name": "RUN npm ci", "error": "exit code: 1 (npm_s3cret)"},
        ]})));
        let Attempt::Failed { summary, log } = progress.failure() else {
            panic!("built");
        };
        assert_eq!(
            summary,
            "failed step: RUN npm ci\nerror: exit code: 1 ([secret NPM_TOKEN])\nlast output:\n401 for warning\n[secret NPM_TOKEN]\nnext line"
        );
        assert!(log.contains("[secret NPM_TOKEN]"), "{log}");
        assert!(log.contains("npm_s\n"), "{log}");
        assert!(!log.contains("s3"), "{log}");
    }

    #[test]
    fn overlapping_secrets_hide_the_longest_value() {
        let redactor = redactor(&[
            ("ORIGIN", "https://example.com"),
            ("PRIVATE_URL", "https://example.com/private-token"),
        ]);
        assert_eq!(
            redactor.text("at https://example.com/private-token and https://example.com/"),
            "at [secret PRIVATE_URL] and [secret ORIGIN]/"
        );
        let (text, rest) = redactor.redact(b"at https://example.com/priv", true);
        assert_eq!(
            (text.as_slice(), rest),
            (b"at ".as_slice(), b"https://example.com/priv".as_slice())
        );
    }

    #[test]
    fn summarises_the_failing_step() {
        let mut progress = Progress::default();
        progress.line(&line(&json!({"vertexes": [
            {"digest": "a", "name": "[1/2] FROM docker.io/library/node:24.9-alpine"},
            {"digest": "b", "name": "[2/2] RUN npm ci"},
        ]})));
        progress.line(&line(&json!({"logs": [
            {"vertex": "b", "stream": 2, "data": b64("npm error missing package-lock.json\n")},
        ]})));
        progress.line(&line(&json!({"vertexes": [
            {"digest": "b", "name": "[2/2] RUN npm ci", "error": "exit code: 1"},
        ]})));
        let Attempt::Failed { summary, log } = progress.failure() else {
            panic!("built");
        };
        assert_eq!(
            summary,
            "failed step: [2/2] RUN npm ci\nerror: exit code: 1\nlast output:\nnpm error missing package-lock.json"
        );
        assert!(log.contains("#[1/2] FROM"));
    }

    #[test]
    fn a_refused_source_shows_buildctl_s_message() {
        let mut progress = Progress::default();
        progress
            .line("error: failed to solve: source denied by policy: docker-image://evil.example/x");
        let Attempt::Failed { summary, .. } = progress.failure() else {
            panic!("built");
        };
        assert!(summary.contains("source denied by policy"));
    }

    #[test]
    fn keeps_the_end_of_long_output() {
        let mut text = "x".repeat(10) + &"é".repeat(SUMMARY_OUTPUT);
        keep_tail(&mut text, SUMMARY_OUTPUT);
        assert!(text.len() <= SUMMARY_OUTPUT);
        assert!(text.ends_with('é'));
    }

    /// Builds `dockerfile` on the buildkitd at `ZB_TEST_BUILDKIT`.
    async fn on_buildkitd(dockerfile: &str) -> Attempt {
        on_buildkitd_with(dockerfile, vec![]).await
    }

    async fn on_buildkitd_with(dockerfile: &str, secrets: Vec<Secret>) -> Attempt {
        let addr = std::env::var("ZB_TEST_BUILDKIT").expect("set ZB_TEST_BUILDKIT");
        let context = crate::testing::Dir::new();
        context.write("hello.txt", "hello\n");
        let work = crate::testing::Dir::new();
        let buildctl = Buildctl {
            addr,
            tlsdir: None,
            context: context.path().to_owned(),
            work: work.path().to_owned(),
            registries: vec!["docker.io".to_owned()],
            timeout: Duration::from_mins(5),
            secrets,
        };
        buildctl.build(dockerfile, "").await
    }

    fn summary(attempt: Attempt) -> String {
        match attempt {
            Attempt::Built => panic!("the build succeeded"),
            Attempt::Failed { summary, .. } => summary,
        }
    }

    #[tokio::test]
    #[ignore = "needs a buildkitd at ZB_TEST_BUILDKIT"]
    async fn builds_from_an_allowed_registry() {
        let attempt = on_buildkitd(
            "FROM docker.io/library/busybox:1.37\nCOPY hello.txt /\nRUN cat /hello.txt\nUSER 1000\n",
        )
        .await;
        assert_eq!(attempt, Attempt::Built);
    }

    #[tokio::test]
    #[ignore = "needs a buildkitd at ZB_TEST_BUILDKIT"]
    async fn refuses_an_image_from_another_registry() {
        let summary = summary(on_buildkitd("FROM registry.k8s.io/pause:3.10\n").await);
        assert!(summary.contains("denied"), "{summary}");
    }

    #[tokio::test]
    #[ignore = "needs a buildkitd at ZB_TEST_BUILDKIT"]
    async fn a_build_reads_a_variable_and_its_value_stays_hidden() {
        let secret = Secret {
            name: "NPM_TOKEN".to_owned(),
            value: "npm_s3cret".to_owned(),
        };
        let dockerfile = "FROM busybox:1.37\n\
            RUN --mount=type=secret,id=NPM_TOKEN,env=NPM_TOKEN echo \"token $NPM_TOKEN\" && false\n";
        let summary = summary(on_buildkitd_with(dockerfile, vec![secret]).await);
        assert!(summary.contains("token [secret NPM_TOKEN]"), "{summary}");
        assert!(!summary.contains("npm_s3cret"), "{summary}");
    }

    #[tokio::test]
    #[ignore = "needs a buildkitd at ZB_TEST_BUILDKIT"]
    async fn refuses_a_download() {
        let summary = summary(
            on_buildkitd("FROM docker.io/library/busybox:1.37\nADD https://example.com/ /x\n")
                .await,
        );
        assert!(summary.contains("denied"), "{summary}");
    }

    #[tokio::test]
    #[ignore = "needs a buildkitd at ZB_TEST_BUILDKIT"]
    async fn refuses_the_host_network() {
        let summary = summary(
            on_buildkitd("FROM docker.io/library/busybox:1.37\nRUN --network=host true\n").await,
        );
        assert!(summary.contains("network.host"), "{summary}");
    }

    #[tokio::test]
    #[ignore = "needs a buildkitd at ZB_TEST_BUILDKIT"]
    async fn ignores_a_syntax_line() {
        // A frontend from a registry the policy refuses: the build only succeeds if it is not used.
        let attempt = on_buildkitd(
            "# syntax=registry.k8s.io/pause:3.10\nFROM docker.io/library/busybox:1.37\nRUN true\nUSER 1000\n",
        )
        .await;
        assert_eq!(attempt, Attempt::Built);
    }

    #[tokio::test]
    #[ignore = "needs a buildkitd at ZB_TEST_BUILDKIT"]
    async fn an_image_that_runs_as_root_fails() {
        for dockerfile in [
            "FROM docker.io/library/busybox:1.37\nRUN true\n",
            "FROM docker.io/library/busybox:1.37\nUSER nobody\n",
        ] {
            let summary = summary(on_buildkitd(dockerfile).await);
            assert!(summary.contains("USER <uid>"), "{summary}");
        }
    }
}
