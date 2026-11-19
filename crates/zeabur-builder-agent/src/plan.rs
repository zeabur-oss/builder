//! What a run ends with: a plan that built, or the reason it stopped.

use serde::{Deserialize, Serialize};

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// A Dockerfile built.
    Ready(Plan),
    /// Stopped: what the user must provide or decide.
    NeedsUser {
        /// Why, from a closed set.
        reason: Reason,
        /// What to do, for the user.
        detail: String,
    },
}

/// A Dockerfile that built, and what the agent learned about the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Where the Dockerfile came from.
    pub(crate) source: Source,
    /// The Dockerfile, as it built.
    pub(crate) dockerfile: String,
    /// The `.dockerignore` it built with, without the rules every build adds.
    pub(crate) dockerignore: String,
    /// What the agent reported when it finished; empty when no model was called.
    pub(crate) details: Details,
}

/// Where a Dockerfile came from, in the order a run tries them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Source {
    /// The repository has one; it is used as is.
    Repository,
    /// The last plan of this service built again.
    Previous,
    /// The agent wrote it in this run.
    Agent,
}

/// What the agent reports with its plan: the arguments of the `finish` tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Details {
    /// The port the app listens on.
    #[serde(default)]
    pub(crate) port: Option<u16>,
    /// Such as `node`, `python`.
    #[serde(default)]
    pub(crate) language: String,
    /// Such as `24.9`.
    #[serde(default)]
    pub(crate) version: String,
    /// The file the version came from, such as `.nvmrc`; empty for a default.
    #[serde(default)]
    pub(crate) version_source: String,
    /// Variables the build reads as `RUN --mount=type=secret,id=NAME,env=NAME`.
    #[serde(default)]
    pub(crate) build_variables: Vec<String>,
    /// Variables the app reads when it starts.
    #[serde(default)]
    pub(crate) runtime_variables: Vec<String>,
    /// Short notes for the user.
    #[serde(default)]
    pub(crate) notes: Vec<String>,
}

/// Why a run stopped without a plan. Zeabur and coding agents act on it, so the set is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Reason {
    /// No API key for the model.
    ModelKeyMissing,
    /// The model API refused or failed the request.
    ModelFailed,
    /// The repository holds no app this agent can build.
    NoAppFound,
    /// The repository holds several apps; the user picks one.
    SeveralApps,
    /// The build needs a variable the user has not set.
    NeedsBuildVariable,
    /// The build needs a private package or repository.
    NeedsPrivateDependency,
    /// The app needs a platform BuildKit cannot build for.
    UnsupportedPlatform,
    /// A host-side limit was reached.
    BudgetSpent,
    /// Every build attempt failed.
    BuildKeepsFailing,
}

/// The short report of a run, for `BuildRun.status.plan`: no Dockerfile, which is in the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Info {
    /// Where the Dockerfile came from, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<Source>,
    /// What the agent reported, when it wrote the Dockerfile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) details: Option<Details>,
    /// Why the run stopped, when it has no plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<Reason>,
    /// What the user must do, when it has no plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
    /// Whether the model was called.
    pub(crate) model_called: bool,
    /// Input tokens over every model call.
    pub(crate) input_tokens: u64,
    /// Output tokens over every model call.
    pub(crate) output_tokens: u64,
}
