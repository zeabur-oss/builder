//! What the agent planner reports about a run: where its Dockerfile came from, or why it stopped.
//!
//! The planner writes it to `/workspace/plan/info.json` and the plan step copies it to its
//! termination message, where the controller reads it into `BuildRun.status.plan`. The kubelet
//! keeps at most [`MAX_MESSAGE_BYTES`] of a termination message, so [`Info::encode`] cuts the
//! free text until it fits.
//!
//! A build is two steps: a planner turns the source into a plan, the folder `/workspace/plan`,
//! and the build turns the source and the plan into an image. After a planner runs and the build
//! pushes its image, the build step pushes the plan folder as an artifact ([`ARTIFACT_TYPE`],
//! tagged [`saved_tag`]) and reports its digest; the controller records it in `Build.status.plan`.
//! Later runs of the `Build` skip the planner: they unpack that plan into `/workspace/plan` and
//! build with it, unless the `Build` has `builder.zeabur.com/plan: fresh`.

use serde::{Deserialize, Serialize};

use crate::result::{MAX_DETAIL_BYTES, MAX_MESSAGE_BYTES, truncate};

/// The `artifactType` of a saved plan: the planner's folder as one gzip tar layer, in the form of
/// an archive source ([`crate::archive`]).
pub const ARTIFACT_TYPE: &str = "application/vnd.zeabur.plan.v1+tar";

/// The tag of a `Build`'s plan artifact, beside its images.
///
/// Runs pull the image by digest; the tag only keeps it from the registry's garbage collection.
/// One tag per `Build`, so `Build`s that push to one repository do not replace each other's plan.
#[must_use]
pub fn saved_tag(build_uid: &str) -> String {
    format!("zeabur-plan-{build_uid}")
}

/// Where a Dockerfile came from, in the order a run tries them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// The repository has one; it is used as is.
    Repository,
    /// A plan built before: the agent's `--previous`, or the plan an earlier run saved.
    Previous,
    /// The agent wrote it in this run.
    Agent,
}

/// What the agent reports with its plan: the arguments of the `finish` tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Details {
    /// The port the app listens on.
    #[serde(default)]
    pub port: Option<u16>,
    /// Such as `node`, `python`.
    #[serde(default)]
    pub language: String,
    /// Such as `24.9`.
    #[serde(default)]
    pub version: String,
    /// The file the version came from, such as `.nvmrc`; empty for a default.
    #[serde(default)]
    pub version_source: String,
    /// Variables the build reads as `RUN --mount=type=secret,id=NAME,env=NAME`.
    #[serde(default)]
    pub build_variables: Vec<String>,
    /// Variables the app reads when it starts.
    #[serde(default)]
    pub runtime_variables: Vec<String>,
    /// Short notes for the user.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Why a run stopped without a plan. Zeabur and coding agents act on it, so the set is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    /// Where the Dockerfile came from, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// What the agent reported when it wrote the Dockerfile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Details>,
    /// Why the run stopped, when it has no plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
    /// What the user must do, when it has no plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Whether the model was called.
    pub model_called: bool,
    /// Input tokens over every model call.
    pub input_tokens: u64,
    /// Output tokens over every model call.
    pub output_tokens: u64,
}

impl Info {
    /// The report as JSON of at most [`MAX_MESSAGE_BYTES`]: `detail` with its control
    /// characters other than newline and tab as spaces and cut to [`MAX_DETAIL_BYTES`], then the
    /// notes, the variable names and the other details left out, in that order, until it fits.
    /// Without those control characters, the JSON of `detail` is at most twice its length.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut info = self.clone();
        info.detail = info
            .detail
            .map(|detail| truncate(&printable(&detail), MAX_DETAIL_BYTES));
        let mut json = to_json(&info);
        if json.len() > MAX_MESSAGE_BYTES
            && let Some(details) = &mut info.details
        {
            details.notes.clear();
            json = to_json(&info);
        }
        if json.len() > MAX_MESSAGE_BYTES
            && let Some(details) = &mut info.details
        {
            details.build_variables.clear();
            details.runtime_variables.clear();
            json = to_json(&info);
        }
        if json.len() > MAX_MESSAGE_BYTES {
            info.details = None;
            json = to_json(&info);
        }
        json
    }

    /// Reads a report [`Info::encode`] wrote.
    ///
    /// # Errors
    ///
    /// When it is longer than [`MAX_MESSAGE_BYTES`], so the kubelet may have cut it, or not a
    /// report.
    pub fn decode(message: &str) -> Result<Self, String> {
        if message.len() > MAX_MESSAGE_BYTES {
            return Err(format!(
                "the plan report is longer than {MAX_MESSAGE_BYTES} bytes"
            ));
        }
        serde_json::from_str(message.trim()).map_err(|e| format!("the plan report: {e}"))
    }
}

/// `s` with its control characters other than newline and tab as spaces. JSON escapes them
/// as `\u00XX`, six bytes each.
fn printable(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// `info` as JSON. It holds strings, numbers and enums only, so this cannot fail.
fn to_json(info: &Info) -> String {
    serde_json::to_string(info).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn info() -> Info {
        Info {
            source: Some(Source::Agent),
            details: Some(Details {
                port: Some(3000),
                language: "node".to_owned(),
                version: "24.9".to_owned(),
                version_source: ".nvmrc".to_owned(),
                build_variables: vec!["NPM_TOKEN".to_owned()],
                runtime_variables: vec!["DATABASE_URL".to_owned()],
                notes: vec!["Uses pnpm.".to_owned()],
            }),
            reason: None,
            detail: None,
            model_called: true,
            input_tokens: 41_000,
            output_tokens: 2_300,
        }
    }

    #[test]
    fn a_short_report_round_trips() {
        let info = info();
        assert_eq!(Info::decode(&info.encode()), Ok(info));
    }

    #[test]
    fn a_long_report_drops_notes_first() {
        let mut long = info();
        if let Some(details) = &mut long.details {
            details.notes = vec!["n".repeat(200); 30];
        }

        let decoded = Info::decode(&long.encode()).unwrap();

        let details = decoded.details.unwrap();
        assert_eq!(details.notes, Vec::<String>::new());
        assert_eq!(details.runtime_variables, ["DATABASE_URL"]);
    }

    #[test]
    fn a_long_detail_is_cut_on_a_character() {
        let stopped = Info {
            source: None,
            details: None,
            reason: Some(Reason::ModelFailed),
            detail: Some("模".repeat(1000)),
            model_called: false,
            input_tokens: 0,
            output_tokens: 0,
        };

        let encoded = stopped.encode();

        assert!(encoded.len() <= MAX_MESSAGE_BYTES);
        let detail = Info::decode(&encoded).unwrap().detail.unwrap();
        assert!(detail.len() <= MAX_DETAIL_BYTES && detail.starts_with('模'));
    }

    #[test]
    fn a_detail_of_escaped_characters_still_fits() {
        let mut stopped = info();
        stopped.reason = Some(Reason::ModelFailed);
        stopped.detail = Some("\u{1}".repeat(MAX_DETAIL_BYTES));

        let encoded = stopped.encode();

        assert!(encoded.len() <= MAX_MESSAGE_BYTES, "{}", encoded.len());
        let decoded = Info::decode(&encoded).unwrap();
        assert_eq!(decoded.reason, Some(Reason::ModelFailed));
        assert_eq!(decoded.detail.unwrap(), " ".repeat(MAX_DETAIL_BYTES));
    }

    #[test]
    fn a_cut_message_is_refused() {
        assert!(Info::decode(&"x".repeat(MAX_MESSAGE_BYTES + 1)).is_err());
    }
}
