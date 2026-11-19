//! Trial builds: `buildctl build` of a Dockerfile against the repository, with nothing exported.
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
}

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
        args
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
        if dockerfile.len() > MAX_DOCKERFILE {
            return failed(format!("the Dockerfile is over {MAX_DOCKERFILE} bytes"));
        }
        if let Err(e) = self.write(dockerfile, dockerignore).await {
            return failed(format!("cannot write the Dockerfile: {e}"));
        }
        let child = Command::new("buildctl")
            .args(self.args())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(e) => return failed(format!("cannot run buildctl: {e}")),
        };
        let Some(stderr) = child.stderr.take() else {
            return failed("buildctl has no stderr".to_owned());
        };
        let mut progress = Progress::default();
        let run = async {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                progress.line(&line);
            }
            child.wait().await
        };
        match tokio::time::timeout(self.timeout, run).await {
            Ok(Ok(status)) if status.success() => Attempt::Built,
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
    let mut rules = vec![rule(DENY, "*".to_owned(), WILDCARD)];
    for local in ["context", "dockerfile"] {
        rules.push(rule(ALLOW, format!("local://{local}"), EXACT));
    }
    for registry in registries {
        rules.push(rule(
            ALLOW,
            format!("docker-image://{registry}/*"),
            WILDCARD,
        ));
    }
    Policy { version: 1, rules }
}

/// What a `--progress rawjson` stream said about a build.
#[derive(Debug, Default)]
struct Progress {
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
    fn line(&mut self, line: &str) {
        let Some(status) = SolveStatus::parse(line) else {
            push_bounded(&mut self.messages, line, SUMMARY_OUTPUT);
            push_bounded(&mut self.log, line, MAX_LOG);
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
                self.failed = Some((vertex.digest, error));
            }
        }
        for log in status.logs {
            let Ok(data) = base64::engine::general_purpose::STANDARD.decode(&log.data) else {
                continue;
            };
            let text = String::from_utf8_lossy(&data);
            push_bounded(&mut self.log, text.trim_end(), MAX_LOG);
            let output = self.output.entry(log.vertex).or_default();
            output.push_str(&text);
            keep_tail(output, SUMMARY_OUTPUT);
        }
    }

    /// The failure: the failing step, its error and the end of its output.
    fn failure(self) -> Attempt {
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

    fn line(value: &serde_json::Value) -> String {
        value.to_string()
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
        assert!(!args.iter().any(|a| a.starts_with("--output")));
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
            "FROM docker.io/library/busybox:1.37\nCOPY hello.txt /\nRUN cat /hello.txt\n",
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
            "# syntax=registry.k8s.io/pause:3.10\nFROM docker.io/library/busybox:1.37\nRUN true\n",
        )
        .await;
        assert_eq!(attempt, Attempt::Built);
    }
}
