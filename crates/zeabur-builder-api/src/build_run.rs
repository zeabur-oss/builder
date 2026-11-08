//! `BuildRun`: one build of one commit or archive.

#![expect(
    unreachable_pub,
    reason = "the KubeSchema derive re-emits these types inside a private module"
)]

use std::collections::BTreeMap;

use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::build::MAX_UPLOAD_REQUEST;
use crate::common::{DIGEST_PATTERN, MAX_NAME, MAX_REVISION, REVISION_PATTERN};

/// One build of one commit or archive. The spec is immutable: to build again, create another.
#[expect(
    clippy::duplicated_attributes,
    reason = "repeated printcolumn arguments such as `type_ = \"string\"` are distinct columns"
)]
#[derive(CustomResource, KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
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
#[x_kube(validation = Rule::new("self == oldSelf").message("a BuildRun's spec is immutable; create another"))]
#[serde(rename_all = "camelCase")]
pub struct BuildRunSpec {
    /// The `Build`.
    #[schemars(length(min = 1, max = MAX_NAME))]
    pub build_ref: String,
    /// The full commit SHA of a Git source, or the manifest digest of an archive source.
    #[schemars(length(max = MAX_REVISION), regex(pattern = REVISION_PATTERN))]
    pub revision: String,
    /// The full name of the ref `revision` was resolved from, such as `refs/heads/master`.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = crate::build::MAX_RESOLVED_REF))]
    pub git_ref: Option<String>,
    /// What started it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<Trigger>,
}

/// What started a `BuildRun`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
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
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, Default)]
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
    #[schemars(length(max = 4096))]
    pub message: Option<String>,
    /// The Job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_NAME))]
    pub job_ref: Option<String>,
    /// The namespace's buildkitd Pod, such as `buildkitd-team-a-0`, set once the run is admitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_NAME))]
    pub buildkitd_instance: Option<String>,
    /// The pushed manifest's digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 71), regex(pattern = DIGEST_PATTERN))]
    pub image_digest: Option<String>,
    /// The tags pushed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 16), inner(length(max = 128)))]
    pub tags: Vec<String>,
    /// The plan used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 71), regex(pattern = DIGEST_PATTERN))]
    pub plan_digest: Option<String>,
    /// Where the plan came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_source: Option<PlanSource>,
    /// Base images, `name@sha256:…`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 10), inner(length(max = 1024)))]
    pub base_images: Vec<String>,
    /// Ports the image declares in its `ExposedPorts`, as the image config writes them
    /// (`3000/tcp`), sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32), inner(length(max = 32)))]
    pub exposed_ports: Vec<String>,
    /// Time per phase (`fetch`, `plan`, `build`, `push`), as durations such as `82.0s`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schemars(extend("maxProperties" = 8, "additionalProperties" = {"type": "string", "maxLength": 32}))]
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
    #[schemars(extend("format" = "date-time"))]
    pub start_time: Option<String>,
    /// When it finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "date-time"))]
    pub completion_time: Option<String>,
    /// When the `Build`'s notify targets were sent this run's image, whatever they answered.
    /// Set once; a run with it is not sent again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "date-time"))]
    pub notified_at: Option<String>,
    /// The last pull token the controller issued, for the annotation
    /// `builder.zeabur.com/pull-token-request`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_token: Option<PullToken>,
}

/// A pull token: a Secret with a short-lived token that only pulls the run's image repository.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PullToken {
    /// The annotation value it answers.
    #[schemars(length(max = MAX_UPLOAD_REQUEST))]
    pub request: String,
    /// The `kubernetes.io/dockerconfigjson` Secret, in the `BuildRun`'s namespace. Its key
    /// `token` holds the bare token.
    #[schemars(length(max = MAX_NAME))]
    pub secret_name: String,
    /// When the token expires.
    #[schemars(extend("format" = "date-time"))]
    pub expires_at: String,
}

/// The phases: `Pending → Planning → Building → Succeeded | Failed | Cancelled`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
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
}

/// Why a `BuildRun` waits or failed.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
pub enum Reason {
    /// The namespace or the cluster runs as many builds as it may, or the `Build`'s previous run
    /// has not stopped yet.
    QuotaExceeded,
    /// The namespace's buildkitd is starting, from zero replicas or after a restart.
    BuildkitdStarting,
    /// The Secret of `spec.output.pushSecretRef` is missing or unusable, or the registry token
    /// key cannot be read.
    RegistryCredentialsMissing,
    /// Clone, checkout or the archive download failed.
    SourceFailed,
    /// The planner failed.
    PlanFailed,
    /// `buildctl` failed, in the build or in the push.
    BuildFailed,
    /// buildkitd was killed for using more memory than its limit.
    OutOfMemory,
    /// The Job ran past its deadline.
    Timeout,
    /// Replaced, cancelled, or its Job was deleted.
    Cancelled,
    /// The Pod disappeared, or buildkitd could not be reached.
    InfraError,
}

/// Where a plan came from.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PlanSource {
    /// Planned in this run.
    Generated,
    /// The builder needs no plan.
    None,
}

/// Build steps and cache hits.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema, Default)]
pub struct CacheStats {
    /// Distinct steps.
    #[schemars(range(min = 0))]
    pub total: i64,
    /// Steps answered from cache.
    #[schemars(range(min = 0))]
    pub cached: i64,
}
