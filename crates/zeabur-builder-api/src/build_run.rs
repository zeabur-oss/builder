//! `BuildRun`: one build of one commit or archive.

#![cfg_attr(
    feature = "schema",
    expect(
        unreachable_pub,
        reason = "the KubeSchema derive re-emits these types inside a private module"
    )
)]

use std::collections::BTreeMap;

use jiff::Timestamp;
use kube_derive::CustomResource;
#[cfg(feature = "schema")]
use kube_derive::KubeSchema;
#[cfg(feature = "schema")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[cfg(feature = "schema")]
use crate::build::MAX_CONDITIONS;
use crate::common::Condition;
#[cfg(feature = "schema")]
use crate::common::{DIGEST_PATTERN, MAX_NAME, MAX_REVISION, REVISION_PATTERN};

/// The most steps a `BuildRun` records: the last ones, in the order they started.
pub const MAX_STEPS: usize = 20;
/// The longest step name or step message, in characters.
pub const MAX_STEP_TEXT: usize = 120;
/// The longest rebuild request `observedRebuild` shows, in characters.
pub const MAX_OBSERVED_REBUILD: usize = 1024;

/// One build of one commit or archive. The spec is immutable: to build again, create another.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(KubeSchema))]
#[kube(
    group = "builder.zeabur.com",
    version = "v1alpha1",
    kind = "BuildRun",
    namespaced,
    status = "BuildRunStatus",
    shortname = "zbrun",
    category = "zeabur-builder",
    selectable = ".spec.buildRef",
    selectable = ".status.phase",
    printcolumn(name = "Build", type_ = "string", json_path = ".spec.buildRef"),
    printcolumn(
        name = "Revision",
        type_ = "string",
        json_path = ".spec.revision",
        priority = 1
    ),
    printcolumn(name = "Phase", type_ = "string", json_path = ".status.phase"),
    printcolumn(name = "Reason", type_ = "string", json_path = ".status.reason"),
    printcolumn(
        name = "Ready",
        type_ = "string",
        json_path = ".status.conditions[?(@.type==\"Ready\")].status",
        priority = 1
    ),
    printcolumn(name = "Started", type_ = "date", json_path = ".status.startTime"),
    printcolumn(
        name = "Finished",
        type_ = "date",
        json_path = ".status.completionTime"
    ),
    printcolumn(
        name = "Age",
        type_ = "date",
        json_path = ".metadata.creationTimestamp"
    ),
    doc = "One build of one Git commit or uploaded archive by Zeabur Builder."
)]
#[cfg_attr(not(feature = "schema"), kube(schema = "disabled"))]
#[cfg_attr(feature = "schema", x_kube(validation = Rule::new("self == oldSelf").message("a BuildRun's spec is immutable; create another")))]
#[serde(rename_all = "camelCase")]
pub struct BuildRunSpec {
    /// The `Build`.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = MAX_NAME)))]
    pub build_ref: String,
    /// The full commit SHA of a Git source, or the manifest digest of an archive source.
    #[cfg_attr(feature = "schema", schemars(length(max = MAX_REVISION), regex(pattern = REVISION_PATTERN)))]
    pub revision: String,
    /// The full name of the ref `revision` was resolved from, such as `refs/heads/master`.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(length(max = crate::build::MAX_RESOLVED_REF)))]
    pub git_ref: Option<String>,
    /// What started it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<Trigger>,
}

/// What started a `BuildRun`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum Trigger {
    /// A push event.
    Webhook,
    /// The poller saw a new commit.
    Poll,
    /// A person.
    Manual,
}

/// Progress and result.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct BuildRunStatus {
    /// Where it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    /// Why it waits or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
    /// Details for people, at most 1 KiB from the Job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(length(max = 4096)))]
    pub message: Option<String>,
    /// The Job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(length(max = MAX_NAME)))]
    pub job_ref: Option<String>,
    /// The namespace's buildkitd Pod, such as `buildkitd-team-a-0`, set once the run is admitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(length(max = MAX_NAME)))]
    pub buildkitd_instance: Option<String>,
    /// The pushed manifest's digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(length(max = 71), regex(pattern = DIGEST_PATTERN)))]
    pub image_digest: Option<String>,
    /// The tags pushed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(
        feature = "schema",
        schemars(length(max = 16), inner(length(max = 128)))
    )]
    pub tags: Vec<String>,
    /// The `ClusterBuilder` the Job built with, read from the Job when the run finishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(length(max = MAX_NAME)))]
    pub builder: Option<String>,
    /// The repository the Job pushed the image and the plan to, read from the Job when the run
    /// finishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(length(max = 1024)))]
    pub image: Option<String>,
    /// The plan artifact in `image` the run built with: the one its planner wrote, or the saved
    /// one it restored (`planSource`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(length(max = 71), regex(pattern = DIGEST_PATTERN)))]
    pub plan_digest: Option<String>,
    /// Where the plan came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_source: Option<PlanSource>,
    /// What the `agent` planner reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanReport>,
    /// Base images, `name@sha256:…`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(
        feature = "schema",
        schemars(length(max = 10), inner(length(max = 1024)))
    )]
    pub base_images: Vec<String>,
    /// Ports the image declares in its `ExposedPorts`, as the image config writes them
    /// (`3000/tcp`), sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(
        feature = "schema",
        schemars(length(max = 32), inner(length(max = 32)))
    )]
    pub exposed_ports: Vec<String>,
    /// Time per phase (`fetch`, `plan`, `build`, `push`), as durations such as `82.0s`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[cfg_attr(feature = "schema", schemars(extend("maxProperties" = 8, "additionalProperties" = {"type": "string", "maxLength": 32})))]
    pub phase_durations: BTreeMap<String, String>,
    /// Build steps and cache hits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_stats: Option<CacheStats>,
    /// The GitHub check run.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "checkRunID"
    )]
    pub check_run_id: Option<i64>,
    /// When the Job was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>", extend("format" = "date-time")))]
    pub start_time: Option<Timestamp>,
    /// When it finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>", extend("format" = "date-time")))]
    pub completion_time: Option<Timestamp>,
    /// When the run last gave its cluster slot back to a run that could start. The queue puts a
    /// run that gave its slot back behind every run that did so earlier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>", extend("format" = "date-time")))]
    pub slot_released_at: Option<Timestamp>,
    /// When the `Build`'s notify targets were sent this run's image, whatever they answered.
    /// Set once; a run with it is not sent again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>", extend("format" = "date-time")))]
    pub notified_at: Option<Timestamp>,
    /// The last build steps, in the order they started. Set when the run finishes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "schema", schemars(length(max = MAX_STEPS)))]
    pub steps: Vec<Step>,
    /// How many earlier steps `steps` leaves out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    pub omitted_steps: Option<i64>,
    /// The step that failed, also when `steps` leaves it out. `fetch` or `plan` when the run
    /// stopped while fetching the source or planning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_step: Option<Step>,
    /// The run's own `builder.zeabur.com/rebuild` annotation: the rebuild request it answers.
    /// Not set for a value longer than 1024 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(length(max = MAX_OBSERVED_REBUILD)))]
    pub observed_rebuild: Option<String>,
    /// `Reconciling`, `Stalled` and `Ready`, in kstatus terms. `Reconciling` is `True` while
    /// the phase is `Pending`, `Planning` or `Building`; `Stalled` is `True` on `Failed` and
    /// `Cancelled`, with the run's `reason`; `Ready` is `True` on `Succeeded` and `False`
    /// otherwise. `Reconciling` and `Stalled` are left out when they do not hold.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "schema", schemars(length(max = MAX_CONDITIONS)))]
    pub conditions: Vec<Condition>,
}

/// One build step: a `BuildKit` vertex, or `fetch` or `plan`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Step {
    /// Its name, such as `[2/3] RUN npm ci`.
    #[cfg_attr(feature = "schema", schemars(length(max = MAX_STEP_TEXT)))]
    pub name: String,
    /// Where it got to.
    pub state: StepState,
    /// Whether it came from cache.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cached: bool,
    /// When it started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>", extend("format" = "date-time")))]
    pub start_time: Option<Timestamp>,
    /// When it finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>", extend("format" = "date-time")))]
    pub completion_time: Option<Timestamp>,
    /// Its error.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    #[cfg_attr(feature = "schema", schemars(length(max = MAX_STEP_TEXT)))]
    pub message: String,
}

/// Where a [`Step`] got to.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub enum StepState {
    /// Started, and not reported finished.
    Running,
    /// Finished, or answered from cache.
    Succeeded,
    /// Finished with an error.
    Failed,
    /// A state this version does not know. It encodes as `Unknown`, so a writer must not send
    /// back a status it read.
    #[serde(other)]
    Unknown,
}

/// The phases: `Pending → Planning → Building → Succeeded | Failed | Cancelled`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub enum Phase {
    /// Waiting for capacity, or for its Job to start.
    Pending,
    /// Fetching and planning.
    Planning,
    /// Building and pushing.
    Building,
    /// The image was pushed.
    Succeeded,
    /// It failed; see `reason`.
    Failed,
    /// A newer commit replaced it, or a person cancelled it.
    Cancelled,
    /// A phase this version does not know. It encodes as `Unknown`, so a writer must not send
    /// back a status it read.
    #[serde(other)]
    Unknown,
}

/// Why a `BuildRun` waits or failed, written as its name. The `Build`'s `Ready` condition
/// repeats it.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub enum Reason {
    /// The namespace or the cluster runs as many builds as it may.
    QuotaExceeded,
    /// The `Build`'s previous run has not stopped yet.
    PreviousRunRunning,
    /// The cluster has no free memory or CPU for the namespace's buildkitd. The controller
    /// frees an idle buildkitd in another namespace, and the run gives its slot back to another
    /// that can start now.
    InsufficientResources,
    /// The namespace's buildkitd is starting, from zero replicas or after a restart.
    BuildkitdStarting,
    /// The Secret of `spec.output.pushSecretRef` is missing or unusable, or the registry token
    /// key cannot be read.
    RegistryCredentialsMissing,
    /// The Secret key or the service account the `agent` builder's `agent.model.auth` names is
    /// missing from the `Build`'s namespace.
    ModelKeyMissing,
    /// Clone, checkout or the archive download failed.
    SourceFailed,
    /// The planner failed.
    PlanFailed,
    /// `buildctl` failed, in the build or in the push.
    BuildFailed,
    /// buildkitd or the build Job was killed for using more memory than its limit.
    OutOfMemory,
    /// The Job ran past its deadline.
    Timeout,
    /// Replaced, cancelled, or its Job was deleted.
    Cancelled,
    /// The Pod disappeared, or buildkitd could not be reached.
    InfraError,
    /// A reason this version does not know. It encodes as `Unknown`, so a writer must not send
    /// back a status it read.
    #[serde(other)]
    Unknown,
}

/// Where a plan came from.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum PlanSource {
    /// Planned in this run.
    Generated,
    /// Restored from the plan an earlier run saved (`Build.status.plan`), instead of running the
    /// planner.
    Saved,
    /// The builder needs no plan.
    None,
}

/// What the `agent` planner reported: where the Dockerfile came from, or why it stopped.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct PlanReport {
    /// Where the Dockerfile came from, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<DockerfileSource>,
    /// Why the planner stopped without a Dockerfile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<PlanStop>,
    /// What the user must do, when it stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(length(max = 1024)))]
    pub detail: Option<String>,
    /// Whether the model was called.
    #[serde(default)]
    pub model_called: bool,
    /// Input tokens over every model call.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(range(min = 0)))]
    pub input_tokens: i64,
    /// Output tokens over every model call.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(range(min = 0)))]
    pub output_tokens: i64,
    /// The port the app listens on, when the agent wrote the Dockerfile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 65535)))]
    pub port: Option<i32>,
    /// Such as `node`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    #[cfg_attr(feature = "schema", schemars(length(max = 64)))]
    pub language: String,
    /// Such as `24.9`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    #[cfg_attr(feature = "schema", schemars(length(max = 64)))]
    pub version: String,
    /// Variables the build reads as secrets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(
        feature = "schema",
        schemars(length(max = 64), inner(length(max = 256)))
    )]
    pub build_variables: Vec<String>,
    /// Variables the app reads when it starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(
        feature = "schema",
        schemars(length(max = 64), inner(length(max = 256)))
    )]
    pub runtime_variables: Vec<String>,
}

/// Where the `agent` planner's Dockerfile came from, in the order it tries them.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum DockerfileSource {
    /// The repository's own Dockerfile.
    Repository,
    /// The last plan of this `Build`, built again.
    Previous,
    /// The agent wrote it in this run.
    Agent,
    /// A source this version does not know. It encodes as `unknown`, so a writer must not
    /// send back a status it read.
    #[serde(other)]
    Unknown,
}

/// Why the `agent` planner stopped without a Dockerfile.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub enum PlanStop {
    /// The model API key or service account token was missing or refused.
    ModelKeyMissing,
    /// The model API refused or failed a request.
    ModelFailed,
    /// The repository holds no app the agent can build.
    NoAppFound,
    /// The repository holds several apps; the user picks one.
    SeveralApps,
    /// The build needs a variable the user has not set.
    NeedsBuildVariable,
    /// The build needs a private package or repository.
    NeedsPrivateDependency,
    /// The app needs a platform `BuildKit` cannot build for.
    UnsupportedPlatform,
    /// A limit on turns, failed builds, time or tokens was reached.
    BudgetSpent,
    /// Every build attempt failed.
    BuildKeepsFailing,
    /// A reason this version does not know. It encodes as `Unknown`, so a writer must not send
    /// back a status it read.
    #[serde(other)]
    Unknown,
}

/// Build steps and cache hits.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct CacheStats {
    /// Distinct steps.
    #[cfg_attr(feature = "schema", schemars(range(min = 0)))]
    pub total: i64,
    /// Steps answered from cache.
    #[cfg_attr(feature = "schema", schemars(range(min = 0)))]
    pub cached: i64,
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values_keep_their_spelling_and_round_trip() {
        let status = BuildRunStatus {
            phase: Some(Phase::Failed),
            reason: Some(Reason::OutOfMemory),
            ..BuildRunStatus::default()
        };
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#"{"phase":"Failed","reason":"OutOfMemory"}"#);
        assert_eq!(
            serde_json::from_str::<BuildRunStatus>(&json).unwrap(),
            status
        );
    }

    #[test]
    fn an_unrecognised_phase_or_reason_decodes_as_unknown() {
        let status: BuildRunStatus =
            serde_json::from_str(r#"{"phase": "Paused", "reason": "DiskFull", "message": "kept"}"#)
                .unwrap();
        assert_eq!(status.phase, Some(Phase::Unknown));
        assert_eq!(status.reason, Some(Reason::Unknown));
        assert_eq!(status.message.as_deref(), Some("kept"));

        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            r#"{"phase":"Unknown","reason":"Unknown","message":"kept"}"#
        );
    }
}
