//! The agent loop: one conversation per run, with building as a tool inside it, so a later
//! attempt keeps what the earlier ones learned. The host runs the loop and checks the budget
//! before every model call and every trial build.

use std::sync::Arc;

use rig_core::DynModel;
use rig_core::completion::{CompletionRequest, ToolDefinition};
use rig_core::message::{Message, ToolCall, ToolName, ToolResult, ToolResultContent};
use rig_core::operation::Completion;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use zeabur_builder_core::refusal::Refusal;

use crate::budget::{Limits, Spent};
use crate::plan::{Details, Outcome, Plan, Reason, Source};
use crate::repo::Repo;
use crate::trial::{Attempt, TrialBuild};

/// One page of `read_build_log`.
const LOG_PAGE: usize = 8 * 1024;

const SYSTEM: &str = "\
You write a Dockerfile that builds and runs the app in this repository.

Work in this order:
1. Look at the repository with `list`, `read` and `grep`. Find the language, its version (from \
version files such as .nvmrc, .python-version, .tool-versions, package.json engines, go.mod, \
pyproject.toml), the package manager, and the build and start commands.
2. Call `build` with a whole Dockerfile. A failure returns the failing step and its output; \
`read_build_log` pages through the full log. Fix the cause and build again. Cached steps are \
reused, so a new attempt is cheap.
3. When a build succeeds, call `finish` with what you learned.

Rules for the Dockerfile:
- Use official images from Docker Hub or GHCR, pinned to a minor version such as \
node:24.9-alpine, never a bare major or `latest`. Use a multi-stage build with a small runtime \
stage.
- The app listens on the port in the PORT variable when it reads one; EXPOSE that port.
- Never copy credentials. A value the build needs is a secret: \
RUN --mount=type=secret,id=NAME,env=NAME. Trial builds have the values of the Build's variables \
the task names, so a step that needs any other variable is a reason to stop.
- No `# syntax` line, no RUN --network or --security: the build refuses them.

Call `stop` instead of guessing when the repository has no app, holds several apps (name them), \
needs a build variable or a private dependency, or needs a platform BuildKit cannot build. Every \
reply calls a tool.";

/// What the agent is asked first.
fn task(hint: Option<&str>, variables: &[String]) -> String {
    let mut task = "Write a Dockerfile for this repository and build it.".to_owned();
    if !variables.is_empty() {
        task.push_str("\n\nThe Build's variables, as secrets: ");
        task.push_str(&variables.join(", "));
    }
    if let Some(hint) = hint {
        task.push_str("\n\nThe last Dockerfile of this service no longer builds:\n\n");
        task.push_str(hint);
    }
    task
}

/// The tools the model may call, with their argument schemas.
fn tools() -> Vec<Arc<ToolDefinition>> {
    let tool = |name: &str, description: &str, parameters: Value| {
        ToolName::new(name)
            .map(|name| Arc::new(ToolDefinition::new(name, description, parameters)))
            .ok()
    };
    let path = json!({"type": "string", "description": "A path relative to the repository root; `.` is the root."});
    [
        tool(
            "list",
            "List files and directories under a path, `depth` levels down (1 to 4).",
            json!({"type": "object", "properties": {"path": path, "depth": {"type": "integer"}}, "required": ["path"]}),
        ),
        tool(
            "read",
            "Read a text file, 12 KiB at a time from byte `offset`.",
            json!({"type": "object", "properties": {"path": path, "offset": {"type": "integer"}}, "required": ["path"]}),
        ),
        tool(
            "grep",
            "Find the lines that contain `pattern` (plain text, case-sensitive) under a path.",
            json!({"type": "object", "properties": {"pattern": {"type": "string"}, "path": path}, "required": ["pattern", "path"]}),
        ),
        tool(
            "build",
            "Build a Dockerfile against the repository. `facts` is what you now know about the app (runtime, version and where it came from, package manager, build and start commands); it is kept and returned with every result.",
            json!({"type": "object", "properties": {
                "dockerfile": {"type": "string"},
                "dockerignore": {"type": "string", "description": "Lines of a .dockerignore. It replaces the repository's own; credentials and .git are always ignored."},
                "facts": {"type": "string"}
            }, "required": ["dockerfile", "facts"]}),
        ),
        tool(
            "read_build_log",
            "Read the full log of the last failed build, 8 KiB per page from page 0.",
            json!({"type": "object", "properties": {"page": {"type": "integer"}}, "required": ["page"]}),
        ),
        tool(
            "finish",
            "Finish after a build succeeded. The last Dockerfile that built is the result.",
            json!({"type": "object", "properties": {
                "port": {"type": "integer"},
                "language": {"type": "string"},
                "version": {"type": "string"},
                "versionSource": {"type": "string", "description": "The file the version came from; empty for a default."},
                "buildVariables": {"type": "array", "items": {"type": "string"}},
                "runtimeVariables": {"type": "array", "items": {"type": "string"}},
                "notes": {"type": "array", "items": {"type": "string"}}
            }, "required": ["language"]}),
        ),
        tool(
            "stop",
            "Stop without a Dockerfile and tell the user what to do.",
            json!({"type": "object", "properties": {
                "reason": {"type": "string", "enum": ["NoAppFound", "SeveralApps", "NeedsBuildVariable", "NeedsPrivateDependency", "UnsupportedPlatform"]},
                "detail": {"type": "string"}
            }, "required": ["reason", "detail"]}),
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

#[derive(Debug, Deserialize)]
struct ListArgs {
    path: String,
    #[serde(default)]
    depth: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: usize,
}

#[derive(Debug, Deserialize)]
struct GrepArgs {
    pattern: String,
    path: String,
}

#[derive(Debug, Deserialize)]
struct BuildArgs {
    dockerfile: String,
    #[serde(default)]
    dockerignore: String,
    #[serde(default)]
    facts: String,
}

#[derive(Debug, Deserialize)]
struct LogArgs {
    page: usize,
}

#[derive(Debug, Deserialize)]
struct StopArgs {
    reason: Reason,
    detail: String,
}

/// How a run ended, and what it used.
#[derive(Debug)]
pub(crate) struct Run {
    pub(crate) outcome: Outcome,
    pub(crate) spent: Spent,
}

/// What the run keeps between turns.
#[derive(Debug, Default)]
struct State {
    /// The last Dockerfile and `.dockerignore` that built.
    built: Option<(String, String)>,
    /// The log of the last failed build.
    log: String,
    /// What the agent last said it knows.
    facts: String,
}

/// What one tool call leads to.
enum Step {
    /// A result for the model.
    Answer { text: String, error: bool },
    /// The run is over.
    Done(Box<Outcome>),
}

impl Step {
    fn done(outcome: Outcome) -> Self {
        Self::Done(Box::new(outcome))
    }

    fn ok(text: impl Into<String>) -> Self {
        Self::Answer {
            text: text.into(),
            error: false,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self::Answer {
            text: text.into(),
            error: true,
        }
    }
}

/// The agent of one run.
#[derive(Debug)]
pub(crate) struct Agent<B> {
    pub(crate) model: DynModel<Completion>,
    pub(crate) repo: Repo,
    pub(crate) builder: B,
    pub(crate) limits: Limits,
}

impl<B: TrialBuild> Agent<B> {
    /// Runs until a Dockerfile builds and the agent finishes, the agent stops, or a limit is
    /// reached. `hint` is an earlier Dockerfile and its failure.
    pub(crate) async fn run(&self, hint: Option<&str>) -> Run {
        let mut spent = Spent::new();
        let outcome = self.converse(hint, &mut spent).await;
        Run { outcome, spent }
    }

    async fn converse(&self, hint: Option<&str>, spent: &mut Spent) -> Outcome {
        let tools = tools();
        let mut state = State::default();
        let mut history = vec![Message::user(task(hint, &self.builder.variables()))];
        loop {
            if let Some(Refusal { reason, message }) = spent.exceeded(&self.limits) {
                return Outcome::NeedsUser {
                    reason,
                    detail: message,
                };
            }
            let request = CompletionRequest::from(history.clone())
                .preamble(SYSTEM)
                // Not `ToolChoice::Required`: Claude models with thinking refuse a forced tool
                // choice. A reply without a tool call gets a nudge below.
                .tools(tools.iter().cloned());
            let call =
                tokio::time::timeout(spent.time_left(&self.limits), self.model.call(request));
            let response = match call.await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => {
                    return Outcome::NeedsUser {
                        reason: Reason::ModelFailed,
                        detail: e.to_string(),
                    };
                }
                Err(_) => continue,
            };
            spent.record(&response.usage);
            let calls: Vec<ToolCall> = response.tool_calls().cloned().collect();
            if let Some(message) = response.message() {
                spent.prompt_bytes += size(&message);
                history.push(message);
            }
            if calls.is_empty() {
                let nudge = Message::user("Reply with a tool call: build, finish or stop.");
                spent.prompt_bytes += size(&nudge);
                history.push(nudge);
                continue;
            }
            let mut results = Vec::with_capacity(calls.len());
            for call in &calls {
                match self.dispatch(call, &mut state, spent).await {
                    Step::Answer { text, error } => results.push(ToolResult {
                        is_error: error,
                        ..call.result(vec![ToolResultContent::text(text)])
                    }),
                    Step::Done(outcome) => return *outcome,
                }
            }
            let message = Message::tool_results(results);
            spent.prompt_bytes += size(&message);
            history.push(message);
        }
    }

    async fn dispatch(&self, call: &ToolCall, state: &mut State, spent: &mut Spent) -> Step {
        match call.function.name.as_str() {
            "list" => answer(call, |a: ListArgs| {
                self.repo.list(&a.path, a.depth.unwrap_or(1))
            }),
            "read" => answer(call, |a: ReadArgs| self.repo.read(&a.path, a.offset)),
            "grep" => answer(call, |a: GrepArgs| self.repo.grep(&a.pattern, &a.path)),
            "build" => match args::<BuildArgs>(call) {
                Ok(a) => self.build(a, state, spent).await,
                Err(step) => step,
            },
            "read_build_log" => match args::<LogArgs>(call) {
                Ok(a) => Step::ok(page(&state.log, a.page)),
                Err(step) => step,
            },
            "finish" => match (args::<Details>(call), &state.built) {
                (Err(step), _) => step,
                (Ok(_), None) => Step::error("No build has succeeded yet: call build first."),
                (Ok(details), Some((dockerfile, dockerignore))) => {
                    Step::done(Outcome::Ready(Plan {
                        source: Source::Agent,
                        dockerfile: dockerfile.clone(),
                        dockerignore: dockerignore.clone(),
                        details,
                    }))
                }
            },
            "stop" => match args::<StopArgs>(call) {
                Ok(a) if user_reason(a.reason) => Step::done(Outcome::NeedsUser {
                    reason: a.reason,
                    detail: a.detail,
                }),
                Ok(_) => Step::error("That reason is not one stop takes."),
                Err(step) => step,
            },
            other => Step::error(format!("There is no tool {other}.")),
        }
    }

    async fn build(&self, args: BuildArgs, state: &mut State, spent: &mut Spent) -> Step {
        if let Some(Refusal { reason, message }) = spent.exceeded(&self.limits) {
            return Step::done(Outcome::NeedsUser {
                reason,
                detail: message,
            });
        }
        state.facts = args.facts;
        let build = self.builder.build(&args.dockerfile, &args.dockerignore);
        let Ok(attempt) = tokio::time::timeout(spent.time_left(&self.limits), build).await else {
            return Step::done(Outcome::NeedsUser {
                reason: Reason::BudgetSpent,
                detail: format!("ran for {} s", self.limits.per_run.as_secs()),
            });
        };
        let facts = &state.facts;
        match attempt {
            Attempt::Built => {
                state.built = Some((args.dockerfile, args.dockerignore));
                Step::ok(format!(
                    "The build succeeded. Call finish.\n\nFacts kept:\n{facts}"
                ))
            }
            Attempt::Failed { summary, log } => {
                spent.failed_builds += 1;
                state.log = log;
                if spent.failed_builds >= self.limits.failed_builds {
                    return Step::done(Outcome::NeedsUser {
                        reason: Reason::BuildKeepsFailing,
                        detail: summary,
                    });
                }
                Step::error(format!(
                    "The build failed ({} of {} failures allowed).\n{summary}\n\nFacts kept:\n{facts}",
                    spent.failed_builds, self.limits.failed_builds
                ))
            }
        }
    }
}

/// Whether the agent may stop with `reason`; the others are the host's.
const fn user_reason(reason: Reason) -> bool {
    match reason {
        Reason::NoAppFound
        | Reason::SeveralApps
        | Reason::NeedsBuildVariable
        | Reason::NeedsPrivateDependency
        | Reason::UnsupportedPlatform => true,
        Reason::ModelKeyMissing
        | Reason::ModelFailed
        | Reason::BudgetSpent
        | Reason::BuildKeepsFailing => false,
    }
}

/// The call's arguments, or an error result the model reads.
fn args<T: DeserializeOwned>(call: &ToolCall) -> Result<T, Step> {
    if let Some(invalid) = &call.function.invalid_arguments {
        return Err(Step::error(format!(
            "The arguments are not a JSON object: {invalid}"
        )));
    }
    serde_json::from_value(Value::Object(call.function.arguments.clone()))
        .map_err(|e| Step::error(format!("Invalid arguments: {e}")))
}

/// Runs a repository tool on the call's arguments.
fn answer<T, E>(call: &ToolCall, run: impl FnOnce(T) -> Result<String, E>) -> Step
where
    T: DeserializeOwned,
    E: std::fmt::Display,
{
    match args(call) {
        Ok(a) => match run(a) {
            Ok(text) => Step::ok(text),
            Err(e) => Step::error(e.to_string()),
        },
        Err(step) => step,
    }
}

/// Page `page` of `log`, [`LOG_PAGE`] bytes each.
fn page(log: &str, page: usize) -> String {
    if log.is_empty() {
        return "No build has failed yet.".to_owned();
    }
    let pages = log.len().div_ceil(LOG_PAGE);
    let start = floor(log, page.saturating_mul(LOG_PAGE));
    let end = floor(log, start.saturating_add(LOG_PAGE));
    match log.get(start..end) {
        Some(text) if !text.is_empty() => format!("{text}\n[page {page} of {pages}]"),
        _ => format!("There are {pages} pages, from 0."),
    }
}

fn floor(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// A message's size as sent, near enough to bound the conversation.
fn size(message: &Message) -> usize {
    serde_json::to_vec(message).map_or(0, |bytes| bytes.len())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;
    use crate::testing::{Dir, Replay, Script, failed, text, tool_use};

    const DOCKERFILE: &str = "FROM node:24.9-alpine\nCOPY . .\nCMD [\"node\", \"index.js\"]\n";

    fn agent(replay: &Replay, script: Script, limits: Limits) -> (Dir, Agent<Script>) {
        let dir = Dir::new();
        dir.write("package.json", r#"{"name":"app"}"#);
        dir.write(
            "index.js",
            "require('http').createServer().listen(process.env.PORT);\n",
        );
        let agent = Agent {
            model: replay.model(),
            repo: Repo::new(dir.path()).unwrap(),
            builder: script,
            limits,
        };
        (dir, agent)
    }

    fn build(id: &str, dockerfile: &str) -> String {
        tool_use(
            id,
            "build",
            &json!({"dockerfile": dockerfile, "facts": "node 24.9 from .nvmrc"}),
        )
    }

    #[tokio::test]
    async fn fixes_a_failed_build_and_finishes_with_the_one_that_built() {
        let replay = Replay::new([
            tool_use("1", "list", &json!({"path": "."})),
            build("2", "FROM node:24.9-alpine\nRUN npm ci\n"),
            build("3", DOCKERFILE),
            tool_use(
                "4",
                "finish",
                &json!({"port": 3000, "language": "node", "version": "24.9"}),
            ),
        ]);
        let script = Script::new([
            failed("npm error missing package-lock.json"),
            Attempt::Built,
        ]);
        let (_dir, agent) = agent(&replay, script.clone(), Limits::default());

        let run = agent.run(None).await;

        let Outcome::Ready(plan) = run.outcome else {
            panic!("{:?}", run.outcome);
        };
        assert_eq!(plan.dockerfile, DOCKERFILE);
        assert_eq!(plan.source, Source::Agent);
        assert_eq!(plan.details.port, Some(3000));
        assert_eq!(run.spent.turns, 4);
        assert_eq!(run.spent.failed_builds, 1);
        assert_eq!(run.spent.input_tokens, 4000);
        let requests = replay.requests.lock().unwrap().clone();
        assert!(requests[1].contains("index.js"), "list result sent back");
        assert!(
            requests[2].contains("missing package-lock.json"),
            "failure summary sent back"
        );
        assert!(requests[2].contains("node 24.9 from .nvmrc"), "facts kept");
    }

    #[tokio::test]
    async fn finish_before_a_build_succeeds_is_refused() {
        let replay = Replay::new([
            tool_use("1", "finish", &json!({"language": "node"})),
            tool_use(
                "2",
                "stop",
                &json!({"reason": "NeedsBuildVariable", "detail": "set NPM_TOKEN"}),
            ),
        ]);
        let (_dir, agent) = agent(&replay, Script::default(), Limits::default());

        let run = agent.run(None).await;

        assert_eq!(
            run.outcome,
            Outcome::NeedsUser {
                reason: Reason::NeedsBuildVariable,
                detail: "set NPM_TOKEN".to_owned()
            }
        );
        assert!(replay.requests.lock().unwrap()[1].contains("call build first"));
    }

    #[tokio::test]
    async fn a_reply_without_a_tool_call_is_nudged() {
        let replay = Replay::new([
            text("1", "I will look at the files first."),
            tool_use(
                "2",
                "stop",
                &json!({"reason": "NoAppFound", "detail": "empty"}),
            ),
        ]);
        let (_dir, agent) = agent(&replay, Script::default(), Limits::default());

        let run = agent.run(None).await;

        assert!(matches!(
            run.outcome,
            Outcome::NeedsUser {
                reason: Reason::NoAppFound,
                ..
            }
        ));
        let requests = replay.requests.lock().unwrap().clone();
        assert!(!requests[0].contains("tool_choice"), "{}", requests[0]);
        assert!(requests[1].contains("Reply with a tool call"));
    }

    #[tokio::test]
    async fn the_agent_cannot_stop_with_a_host_reason() {
        let replay = Replay::new([
            tool_use(
                "1",
                "stop",
                &json!({"reason": "BudgetSpent", "detail": "tired"}),
            ),
            tool_use(
                "2",
                "stop",
                &json!({"reason": "NoAppFound", "detail": "no app"}),
            ),
        ]);
        let (_dir, agent) = agent(&replay, Script::default(), Limits::default());

        let run = agent.run(None).await;

        assert!(matches!(
            run.outcome,
            Outcome::NeedsUser {
                reason: Reason::NoAppFound,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn stops_when_the_turns_are_spent() {
        let replay =
            Replay::new((0..5).map(|i| tool_use(&i.to_string(), "list", &json!({"path": "."}))));
        let limits = Limits {
            turns: 3,
            ..Limits::default()
        };
        let (_dir, agent) = agent(&replay, Script::default(), limits);

        let run = agent.run(None).await;

        assert!(matches!(
            run.outcome,
            Outcome::NeedsUser {
                reason: Reason::BudgetSpent,
                ..
            }
        ));
        assert_eq!(replay.requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn stops_with_the_last_failure_when_builds_keep_failing() {
        let replay = Replay::new([build("1", DOCKERFILE), build("2", DOCKERFILE)]);
        let script = Script::new([failed("first"), failed("exit code: 127")]);
        let limits = Limits {
            failed_builds: 2,
            ..Limits::default()
        };
        let (_dir, agent) = agent(&replay, script, limits);

        let run = agent.run(None).await;

        assert_eq!(
            run.outcome,
            Outcome::NeedsUser {
                reason: Reason::BuildKeepsFailing,
                detail: "exit code: 127".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn a_build_stops_at_the_end_of_the_run() {
        let replay = Replay::new([build("1", DOCKERFILE)]);
        let limits = Limits {
            per_run: Duration::from_millis(50),
            ..Limits::default()
        };
        let (_dir, agent) = agent(&replay, Script::default(), limits);

        let run = agent.run(None).await;

        assert!(matches!(
            run.outcome,
            Outcome::NeedsUser {
                reason: Reason::BudgetSpent,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn stops_when_the_input_tokens_are_spent() {
        let replay =
            Replay::new((0..5).map(|i| tool_use(&i.to_string(), "list", &json!({"path": "."}))));
        let limits = Limits {
            input_tokens: 2000,
            ..Limits::default()
        };
        let (_dir, agent) = agent(&replay, Script::default(), limits);

        let run = agent.run(None).await;

        assert!(matches!(
            run.outcome,
            Outcome::NeedsUser {
                reason: Reason::BudgetSpent,
                ..
            }
        ));
        assert_eq!(run.spent.input_tokens, 2000);
    }

    #[tokio::test]
    async fn a_model_error_stops_the_run() {
        let replay = Replay::new([]);
        let (_dir, agent) = agent(&replay, Script::default(), Limits::default());

        let run = agent.run(None).await;

        assert!(matches!(
            run.outcome,
            Outcome::NeedsUser {
                reason: Reason::ModelFailed,
                ..
            }
        ));
    }

    #[test]
    fn pages_the_build_log() {
        let log = "x".repeat(LOG_PAGE + 10);
        assert!(page(&log, 0).ends_with("[page 0 of 2]"));
        assert!(page(&log, 1).starts_with("xxxxxxxxxx\n"));
        assert_eq!(page(&log, 2), "There are 2 pages, from 0.");
        assert_eq!(page("", 0), "No build has failed yet.");
    }
}
