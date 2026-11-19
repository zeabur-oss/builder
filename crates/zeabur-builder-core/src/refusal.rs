//! A refusal: the reason a step stopped, and the words that tell a person what to do.

/// Why a step refused. `R` is the vocabulary of the caller: a condition reason, a run reason or
/// a failure reason.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct Refusal<R: std::fmt::Debug> {
    /// Which rule refused.
    pub reason: R,
    /// What the person must change.
    pub message: String,
}

impl<R: std::fmt::Debug> Refusal<R> {
    /// A refusal for `reason`, told as `message`.
    #[must_use]
    pub fn new(reason: R, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}
