//! The build result a Job reports through its termination message.
//!
//! The build container writes one JSON object to `/dev/termination-log`; the kubelet copies it into
//! the Pod's container status, where the controller reads it without `pods/log` access or a side
//! channel. The kubelet keeps at most 4096 bytes of a termination message ([termination
//! messages]), counted in bytes of UTF-8. [`BuildResult::encode`] caps the lists and shortens the
//! object until it fits, in the order its documentation gives, and [`BuildResult::decode`] refuses
//! anything larger.
//!
//! [termination messages]: https://kubernetes.io/docs/tasks/debug/debug-application/determine-reason-pod-failure/#customizing-the-termination-message

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The kubelet's limit for a termination message.
pub const MAX_MESSAGE_BYTES: usize = 4096;

/// The longest `message` kept.
pub const MAX_DETAIL_BYTES: usize = 1024;

/// The most base images recorded.
pub const MAX_BASE_IMAGES: usize = 10;

/// The most exposed ports recorded.
pub const MAX_EXPOSED_PORTS: usize = 32;

/// The most steps recorded: the last ones, in the order they started.
pub const MAX_STEPS: usize = 20;

/// The longest step name or step message kept, in characters.
pub const MAX_STEP_TEXT: usize = 120;

/// What a build Job reports.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildResult {
    /// Whether the image was pushed.
    pub result: Outcome,
    /// Why it failed; `None` on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<FailureReason>,
    /// A short explanation for people, at most [`MAX_DETAIL_BYTES`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// The pushed image's manifest digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// The tags pushed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// The plan's digest, when a planner ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_digest: Option<String>,
    /// Where the plan came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_source: Option<PlanSource>,
    /// `name@sha256:…` of the base images, at most [`MAX_BASE_IMAGES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub base_images: Vec<String>,
    /// The image's `ExposedPorts` keys such as `3000/tcp`, sorted, at most [`MAX_EXPOSED_PORTS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exposed_ports: Vec<String>,
    /// Seconds spent per phase: `fetch`, `plan`, `build`, `push`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub phases: BTreeMap<String, f64>,
    /// Build steps, and how many came from cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheStats>,
    /// The last steps, at most [`MAX_STEPS`], in the order they started.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
    /// How many earlier steps `steps` leaves out.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_steps: u32,
    /// The step that failed, also when `steps` leaves it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_step: Option<Step>,
}

/// One step of a build: a `BuildKit` vertex.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    /// Its name, such as `[2/3] RUN npm ci`, at most [`MAX_STEP_TEXT`] characters.
    pub name: String,
    /// Where it got to.
    pub state: StepState,
    /// Whether it came from cache.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cached: bool,
    /// When it started, RFC 3339 to the second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    /// When it finished, RFC 3339 to the second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_time: Option<String>,
    /// Its error, at most [`MAX_STEP_TEXT`] characters.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

impl Step {
    /// The step with its name and message cut to [`MAX_STEP_TEXT`] characters.
    #[must_use]
    pub fn bounded(mut self) -> Self {
        self.name = chars(&self.name, MAX_STEP_TEXT);
        self.message = chars(&self.message, MAX_STEP_TEXT);
        self
    }
}

/// Where a [`Step`] got to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StepState {
    /// Started, and not reported finished.
    #[default]
    Running,
    /// Finished, or answered from cache.
    Succeeded,
    /// Finished with an error.
    Failed,
}

/// Success or failure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    /// The image was pushed.
    Succeeded,
    /// It was not.
    #[default]
    Failed,
}

/// The `BuildRun` failure reasons a Job can report.
///
/// `Timeout`, `Cancelled` and `InfraError` are decided by the controller from the Job and Pod,
/// never reported by the Job itself.
#[expect(
    clippy::enum_variant_names,
    reason = "the names are the BuildRun reasons people read in `kubectl get buildrun`"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureReason {
    /// Clone or checkout failed.
    SourceFailed,
    /// The planner failed.
    PlanFailed,
    /// `buildctl` failed, in the build or in the push.
    BuildFailed,
}

/// Where a plan came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanSource {
    /// The planner produced it in this run.
    Generated,
    /// The builder needs no plan.
    None,
}

/// Cache use of one build.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStats {
    /// Distinct build steps.
    pub total: u32,
    /// Steps answered from cache.
    pub cached: u32,
}

/// Why a termination message cannot be read.
#[derive(Debug, Error)]
pub enum DecodeError {
    /// Larger than the kubelet keeps, so it was cut and is not trustworthy.
    #[error("the termination message is longer than {MAX_MESSAGE_BYTES} bytes")]
    TooLong,
    /// Not the expected JSON.
    #[error("the termination message is not a build result: {0}")]
    Json(#[from] serde_json::Error),
    /// A success without a digest, or a digest in the wrong form.
    #[error("the termination message reports success without a valid sha256 digest")]
    Digest,
    /// A failure without a reason.
    #[error("the termination message reports a failure without a reason")]
    Reason,
}

impl BuildResult {
    /// Reads a termination message.
    ///
    /// # Errors
    ///
    /// When it is too long, not JSON, or inconsistent.
    pub fn decode(message: &str) -> Result<Self, DecodeError> {
        if message.len() > MAX_MESSAGE_BYTES {
            return Err(DecodeError::TooLong);
        }
        let result: Self = serde_json::from_str(message)?;
        match result.result {
            Outcome::Succeeded if !result.digest.as_deref().is_some_and(is_digest) => {
                Err(DecodeError::Digest)
            }
            Outcome::Failed if result.reason.is_none() => Err(DecodeError::Reason),
            Outcome::Succeeded | Outcome::Failed => Ok(result),
        }
    }

    /// Writes the message as JSON of at most [`MAX_MESSAGE_BYTES`] bytes.
    ///
    /// First `message` is cut to [`MAX_DETAIL_BYTES`], the lists to [`MAX_BASE_IMAGES`],
    /// [`MAX_EXPOSED_PORTS`] and the last [`MAX_STEPS`] steps, and step names and messages to
    /// [`MAX_STEP_TEXT`] characters. While the JSON is still too long, it drops, one at a time:
    ///
    /// 1. the oldest step, counted in `omittedSteps`;
    /// 2. the last base image;
    /// 3. the last exposed port;
    /// 4. every tag but the first;
    /// 5. `message`;
    /// 6. the phase times and the tags.
    ///
    /// `result`, `reason`, `digest`, `cache` and `failedStep` are always kept.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut result = self.clone();
        result.message = truncate(&result.message, MAX_DETAIL_BYTES);
        result.base_images.truncate(MAX_BASE_IMAGES);
        result.exposed_ports.truncate(MAX_EXPOSED_PORTS);
        let extra = result.steps.len().saturating_sub(MAX_STEPS);
        result.steps.drain(..extra);
        result.omitted_steps = result
            .omitted_steps
            .saturating_add(u32::try_from(extra).unwrap_or(u32::MAX));
        result.steps = result.steps.into_iter().map(Step::bounded).collect();
        result.failed_step = result.failed_step.map(Step::bounded);
        loop {
            let text = serde_json::to_string(&result).unwrap_or_default();
            if text.len() <= MAX_MESSAGE_BYTES {
                return text;
            }
            if !result.steps.is_empty() {
                result.steps.remove(0);
                result.omitted_steps = result.omitted_steps.saturating_add(1);
                continue;
            }
            if result.base_images.pop().is_some() {
                continue;
            }
            if result.exposed_ports.pop().is_some() {
                continue;
            }
            if result.tags.len() > 1 {
                result.tags.truncate(1);
                continue;
            }
            if !result.message.is_empty() {
                result.message.clear();
                continue;
            }
            result.phases.clear();
            result.tags.clear();
            return serde_json::to_string(&result).unwrap_or_default();
        }
    }
}

/// `sha256:` followed by 64 lowercase hexadecimal digits.
#[must_use]
pub fn is_digest(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's `skip_serializing_if` passes a reference"
)]
const fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Cuts a string to at most `max` characters.
fn chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Cuts a string to at most `max` bytes on a character boundary.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn reads_the_documented_example() {
        let message = format!(
            r#"{{"result":"succeeded","digest":"{DIGEST}","tags":["main-1761058800-3e1a9c2"],
            "planSource":"generated","baseImages":["docker.io/library/node:24-alpine@{DIGEST}"],
            "phases":{{"fetch":3.1,"plan":0.4,"build":82.0,"push":6.2}},"cache":{{"total":41,"cached":37}}}}"#
        );
        let result = BuildResult::decode(&message).unwrap();
        assert_eq!(result.result, Outcome::Succeeded);
        assert_eq!(
            result.cache,
            Some(CacheStats {
                total: 41,
                cached: 37
            })
        );
        assert_eq!(result.plan_source, Some(PlanSource::Generated));
        assert_eq!(BuildResult::decode(&result.encode()).unwrap(), result);
    }

    #[test]
    fn rejects_inconsistent_messages() {
        assert!(matches!(
            BuildResult::decode(r#"{"result":"succeeded"}"#),
            Err(DecodeError::Digest)
        ));
        assert!(matches!(
            BuildResult::decode(r#"{"result":"succeeded","digest":"sha256:abc"}"#),
            Err(DecodeError::Digest)
        ));
        assert!(matches!(
            BuildResult::decode(r#"{"result":"failed"}"#),
            Err(DecodeError::Reason)
        ));
        assert!(matches!(
            BuildResult::decode(r#"{"result":"failed","reason":"Timeout"}"#),
            Err(DecodeError::Json(_))
        ));
        assert!(matches!(
            BuildResult::decode(&" ".repeat(4097)),
            Err(DecodeError::TooLong)
        ));
    }

    #[test]
    fn encode_fits_the_kubelet_limit() {
        let result = BuildResult {
            result: Outcome::Failed,
            reason: Some(FailureReason::BuildFailed),
            message: "錯誤".repeat(2000),
            tags: (0..100).map(|i| format!("tag-{i:0>100}")).collect(),
            base_images: (0..20)
                .map(|i| format!("docker.io/library/i{i}@{DIGEST}"))
                .collect(),
            ..BuildResult::default()
        };
        let text = result.encode();
        assert!(text.len() <= MAX_MESSAGE_BYTES, "{}", text.len());
        let back = BuildResult::decode(&text).unwrap();
        assert_eq!(back.reason, Some(FailureReason::BuildFailed));
        assert!(back.message.len() <= MAX_DETAIL_BYTES);
    }

    #[test]
    fn exposed_ports_travel_and_are_dropped_before_the_digest() {
        let mut result = BuildResult {
            result: Outcome::Succeeded,
            digest: Some(DIGEST.to_owned()),
            exposed_ports: vec!["3000/tcp".to_owned(), "53/udp".to_owned()],
            ..BuildResult::default()
        };
        assert!(
            result
                .encode()
                .contains(r#""exposedPorts":["3000/tcp","53/udp"]"#)
        );
        assert_eq!(BuildResult::decode(&result.encode()).unwrap(), result);

        result.exposed_ports = (0..100).map(|i| format!("{}/tcp", 10_000 + i)).collect();
        result.tags = vec!["t".repeat(3000)];
        let back = BuildResult::decode(&result.encode()).unwrap();
        assert_eq!(back.digest.as_deref(), Some(DIGEST));
        assert!(back.exposed_ports.len() <= MAX_EXPOSED_PORTS);

        result.exposed_ports.clear();
        assert!(!result.encode().contains("exposedPorts"));
    }

    fn step(name: String, message: String) -> Step {
        Step {
            name,
            state: StepState::Succeeded,
            cached: false,
            start_time: Some("2026-10-22T00:00:00Z".to_owned()),
            completion_time: Some("2026-10-22T00:00:01Z".to_owned()),
            message,
        }
    }

    #[test]
    fn decode_takes_4096_bytes_and_refuses_4097() {
        let message = |len: usize| {
            let head = r#"{"result":"failed","reason":"BuildFailed","message":""#;
            let tail = r#""}"#;
            format!("{head}{}{tail}", "x".repeat(len - head.len() - tail.len()))
        };
        let full = message(MAX_MESSAGE_BYTES);
        assert_eq!(full.len(), MAX_MESSAGE_BYTES);
        assert_eq!(
            BuildResult::decode(&full).unwrap().reason,
            Some(FailureReason::BuildFailed)
        );
        assert!(matches!(
            BuildResult::decode(&message(MAX_MESSAGE_BYTES + 1)),
            Err(DecodeError::TooLong)
        ));
    }

    #[test]
    fn encode_keeps_the_last_steps_and_counts_the_rest() {
        let result = BuildResult {
            result: Outcome::Succeeded,
            digest: Some(DIGEST.to_owned()),
            steps: (0..25)
                .map(|i| step(format!("step {i}"), String::new()))
                .collect(),
            ..BuildResult::default()
        };
        let back = BuildResult::decode(&result.encode()).unwrap();
        let names: Vec<&str> = back.steps.iter().map(|s| s.name.as_str()).collect();
        let want: Vec<String> = (5..25).map(|i| format!("step {i}")).collect();
        assert_eq!(names, want);
        assert_eq!(back.omitted_steps, 5);
    }

    #[test]
    fn encode_drops_the_oldest_steps_first_by_bytes_and_keeps_the_failure() {
        let long = |c: &str| c.repeat(200);
        for (case, text) in [("ascii", "x"), ("chinese", "錯")] {
            let failed = Step {
                state: StepState::Failed,
                ..step(format!("0 {}", long(text)), long(text))
            };
            let mut steps = vec![failed.clone()];
            steps.extend((1..30).map(|i| step(format!("{i} {}", long(text)), long(text))));
            let result = BuildResult {
                result: Outcome::Failed,
                reason: Some(FailureReason::BuildFailed),
                message: long(text),
                base_images: (0..10)
                    .map(|i| format!("docker.io/library/i{i}@{DIGEST}"))
                    .collect(),
                steps: steps.clone(),
                failed_step: Some(failed.clone()),
                ..BuildResult::default()
            };

            let text = result.encode();
            assert!(text.len() <= MAX_MESSAGE_BYTES, "{case}: {}", text.len());
            let back = BuildResult::decode(&text).unwrap();
            assert_eq!(back.result, Outcome::Failed, "{case}");
            assert_eq!(back.reason, Some(FailureReason::BuildFailed), "{case}");
            assert_eq!(back.failed_step, Some(failed.bounded()), "{case}");
            let kept = back.steps.len();
            let last: Vec<Step> = steps[steps.len() - kept..]
                .iter()
                .cloned()
                .map(Step::bounded)
                .collect();
            assert_eq!(back.steps, last, "{case}");
            assert_eq!(back.omitted_steps as usize + kept, steps.len(), "{case}");
            if kept > 0 {
                assert_eq!(back.base_images, result.base_images, "{case}");
            }
            for step in &back.steps {
                assert!(step.name.chars().count() <= MAX_STEP_TEXT, "{case}");
                assert!(step.message.chars().count() <= MAX_STEP_TEXT, "{case}");
            }
        }
    }
}
