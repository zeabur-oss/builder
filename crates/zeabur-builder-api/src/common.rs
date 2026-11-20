//! Types shared by the custom resources.

use jiff::Timestamp;
#[cfg(feature = "schema")]
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
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct SecretRef {
    /// The Secret's name.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = MAX_NAME)))]
    pub name: String,
}

/// One key of a Secret in the same namespace.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct SecretKeyRef {
    /// The Secret's name.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = MAX_NAME)))]
    pub name: String,
    /// The key.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = MAX_NAME)))]
    pub key: String,
}

/// A status condition, as `metav1.Condition`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    /// The type.
    #[serde(rename = "type")]
    pub kind: ConditionType,
    /// `True`, `False` or `Unknown`.
    pub status: ConditionStatus,
    /// A `CamelCase` reason.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = 1024)))]
    pub reason: String,
    /// A human-readable message.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    #[cfg_attr(feature = "schema", schemars(length(max = 32768)))]
    pub message: String,
    /// When the status last changed (RFC 3339).
    #[cfg_attr(feature = "schema", schemars(with = "String", extend("format" = "date-time")))]
    pub last_transition_time: Timestamp,
    /// The `metadata.generation` the condition was computed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(range(min = 0)))]
    pub observed_generation: Option<i64>,
}

/// The status of a [`Condition`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub enum ConditionStatus {
    /// It holds.
    True,
    /// It does not.
    False,
    /// Not known yet.
    Unknown,
}

/// The `type` of a [`Condition`]. A `Build` has `Ready`, `SourceReady` and `BuilderReady`, a
/// `BuildRun` has `Reconciling`, `Stalled` and `Ready`, a `ClusterBuilder` has `Ready`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub enum ConditionType {
    /// The object has reached what its spec asks.
    Ready,
    /// A `Build`: the source resolves to a revision.
    SourceReady,
    /// A `Build`: its `ClusterBuilder` exists.
    BuilderReady,
    /// Work toward the current spec is under way.
    Reconciling,
    /// Work toward the current spec cannot go on.
    Stalled,
    /// A type this version does not know. It encodes as `Unknown`, so a writer must not send
    /// back a status it read.
    #[serde(other)]
    Unknown,
}
