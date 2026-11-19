//! What a run ends with: a plan that built, or the reason it stopped.

pub(crate) use zeabur_builder_core::plan::{Details, Info, Reason, Source};

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
    /// The `.dockerignore` it built with, without the rules every build adds. For the repository's
    /// own Dockerfile, its `Dockerfile.dockerignore` as is, or empty when it has none.
    pub(crate) dockerignore: String,
    /// What the agent reported when it finished; empty when no model was called.
    pub(crate) details: Details,
}
