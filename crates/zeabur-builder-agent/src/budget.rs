//! The limits of one run, kept by the host: the prompt cannot change them.

use std::time::{Duration, Instant};

use rig_core::completion::Usage;

use crate::plan::Reason;

/// The limits of one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    /// Model calls.
    pub(crate) turns: u32,
    /// Trial builds that failed.
    pub(crate) failed_builds: u32,
    /// The longest one trial build may take.
    pub(crate) per_build: Duration,
    /// The longest the run may take.
    pub(crate) per_run: Duration,
    /// Input tokens over every model call.
    pub(crate) input_tokens: u64,
    /// The conversation's text, in bytes.
    pub(crate) prompt_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            turns: 40,
            failed_builds: 4,
            per_build: Duration::from_mins(10),
            per_run: Duration::from_mins(20),
            input_tokens: 1_000_000,
            prompt_bytes: 200 * 1024,
        }
    }
}

/// What a run has used.
#[derive(Debug, Clone)]
pub(crate) struct Spent {
    started: Instant,
    pub(crate) turns: u32,
    pub(crate) failed_builds: u32,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) prompt_bytes: usize,
}

impl Spent {
    /// Nothing used yet; the run's clock starts now.
    pub(crate) fn new() -> Self {
        Self {
            started: Instant::now(),
            turns: 0,
            failed_builds: 0,
            input_tokens: 0,
            output_tokens: 0,
            prompt_bytes: 0,
        }
    }

    /// Adds one model call's `usage`.
    pub(crate) fn record(&mut self, usage: &Usage) {
        self.turns += 1;
        self.input_tokens += usage.input_tokens.unwrap_or_default();
        self.output_tokens += usage.output_tokens.unwrap_or_default();
    }

    /// The time left in the run.
    pub(crate) fn time_left(&self, limits: &Limits) -> Duration {
        limits.per_run.saturating_sub(self.started.elapsed())
    }

    /// The first limit reached, with a message for the user, or `None`.
    pub(crate) fn exceeded(&self, limits: &Limits) -> Option<(Reason, String)> {
        let spent = |message: String| Some((Reason::BudgetSpent, message));
        if self.failed_builds >= limits.failed_builds {
            Some((
                Reason::BuildKeepsFailing,
                format!("{} trial builds failed", self.failed_builds),
            ))
        } else if self.turns >= limits.turns {
            spent(format!("used all {} model calls", limits.turns))
        } else if self.input_tokens >= limits.input_tokens {
            spent(format!("used {} input tokens", self.input_tokens))
        } else if self.prompt_bytes >= limits.prompt_bytes {
            spent(format!(
                "the conversation reached {} bytes",
                self.prompt_bytes
            ))
        } else if self.time_left(limits).is_zero() {
            spent(format!("ran for {} s", limits.per_run.as_secs()))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_limit_stops_the_run() {
        let limits = Limits {
            per_run: Duration::from_secs(3600),
            ..Limits::default()
        };
        let fresh = Spent::new();
        assert_eq!(fresh.exceeded(&limits), None);

        let message = |spent: &Spent, limits: &Limits| {
            let (reason, message) = spent.exceeded(limits).unwrap();
            assert_eq!(
                reason == Reason::BuildKeepsFailing,
                message.contains("trial builds")
            );
            message
        };

        let mut turns = fresh.clone();
        turns.turns = limits.turns;
        assert!(message(&turns, &limits).contains("model calls"));

        let mut builds = fresh.clone();
        builds.failed_builds = limits.failed_builds;
        assert!(message(&builds, &limits).contains("trial builds"));

        let mut tokens = fresh.clone();
        let mut usage = Usage::default();
        usage.input_tokens = Some(limits.input_tokens);
        tokens.record(&usage);
        assert!(message(&tokens, &limits).contains("input tokens"));

        let mut prompt = fresh.clone();
        prompt.prompt_bytes = limits.prompt_bytes;
        assert!(message(&prompt, &limits).contains("bytes"));

        let no_time = Limits {
            per_run: Duration::ZERO,
            ..limits
        };
        assert!(message(&fresh, &no_time).contains("ran for"));
    }
}
