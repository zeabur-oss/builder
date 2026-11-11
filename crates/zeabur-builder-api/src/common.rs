//! Types shared by the custom resources.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Longest Kubernetes object name (DNS subdomain).
pub const MAX_NAME: usize = 253;

/// A Go duration as Kubernetes writes them, such as `90s`, `10m` or `1h30m`.
pub const DURATION_PATTERN: &str = r"^([0-9]{1,6}(\.[0-9]{1,3})?(ms|s|m|h))+$";

/// What a build built from: a full commit SHA, or an archive's manifest digest.
pub const REVISION_PATTERN: &str = "^([0-9a-f]{40}|sha256:[0-9a-f]{64})$";

/// The longest [`REVISION_PATTERN`] value.
pub const MAX_REVISION: usize = 71;

/// An OCI manifest digest.
pub const DIGEST_PATTERN: &str = "^sha256:[0-9a-f]{64}$";

/// A Secret in the same namespace as the object that names it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct SecretRef {
    /// The Secret's name.
    #[schemars(length(min = 1, max = MAX_NAME))]
    pub name: String,
}

/// One key of a Secret in the same namespace.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct SecretKeyRef {
    /// The Secret's name.
    #[schemars(length(min = 1, max = MAX_NAME))]
    pub name: String,
    /// The key.
    #[schemars(length(min = 1, max = MAX_NAME))]
    pub key: String,
}

/// A status condition, as `metav1.Condition`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    /// The type: on a `Build`, `Ready`, `SourceReady` or `BuilderReady`; on a `BuildRun`,
    /// `Reconciling`, `Stalled` or `Ready`; on a `ClusterBuilder`, `Ready`.
    #[serde(rename = "type")]
    #[schemars(length(min = 1, max = 316))]
    pub kind: String,
    /// `True`, `False` or `Unknown`.
    pub status: ConditionStatus,
    /// A `CamelCase` reason.
    #[schemars(length(min = 1, max = 1024))]
    pub reason: String,
    /// A human-readable message.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    #[schemars(length(max = 32768))]
    pub message: String,
    /// When the status last changed (RFC 3339).
    #[schemars(extend("format" = "date-time"))]
    pub last_transition_time: String,
    /// The `metadata.generation` the condition was computed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0))]
    pub observed_generation: Option<i64>,
}

/// The status of a [`Condition`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
pub enum ConditionStatus {
    /// It holds.
    True,
    /// It does not.
    False,
    /// Not known yet.
    Unknown,
}
