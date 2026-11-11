//! `Build`: what to build, and the newest image of it.

#![expect(
    unreachable_pub,
    reason = "the KubeSchema derive re-emits these types inside a private module"
)]

use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::build_run::Reason;
use crate::common::{
    Condition, DIGEST_PATTERN, DURATION_PATTERN, MAX_NAME, MAX_REVISION, REVISION_PATTERN,
    SecretKeyRef, SecretRef,
};

/// Longest repository URL.
pub const MAX_URL: usize = 2048;
/// Longest Git ref.
pub const MAX_REF: usize = 255;
/// Longest resolved ref, with its `refs/heads/` prefix.
pub const MAX_RESOLVED_REF: usize = 1024;
/// Longest path inside the repository.
pub const MAX_PATH: usize = 1024;
/// Longest image repository.
pub const MAX_IMAGE: usize = 512;
/// Longest tag template.
pub const MAX_TAG_TEMPLATE: usize = 256;
/// Most extra tags.
pub const MAX_EXTRA_TAGS: usize = 8;
/// Most build variables.
pub const MAX_ENV: usize = 128;
/// Most notification targets.
pub const MAX_NOTIFY: usize = 8;
/// Most base images remembered.
pub const MAX_BASE_IMAGES: usize = 10;

/// The most exposed ports a `Build` or `BuildRun` records.
pub const MAX_EXPOSED_PORTS: usize = 32;
/// Most conditions.
pub const MAX_CONDITIONS: usize = 8;
/// The longest upload token request the controller answers.
pub const MAX_UPLOAD_REQUEST: usize = 63;
/// A relative path whose segments do not start with `.`, so it cannot climb out with `..`.
pub const RELATIVE_PATH_PATTERN: &str =
    r"^[A-Za-z0-9_@+-][A-Za-z0-9._@+-]*(/[A-Za-z0-9_@+-][A-Za-z0-9._@+-]*)*$";

/// A Git ref to follow, or an uploaded archive, and the image to keep built from it.
///
/// ```yaml
/// apiVersion: builder.zeabur.com/v1alpha1
/// kind: Build
/// metadata: {name: api, namespace: team-a}
/// spec:
///   source: {git: {url: https://github.com/acme/api, ref: main}}
///   builderRef: railpack
/// ```
#[expect(
    clippy::duplicated_attributes,
    reason = "repeated printcolumn arguments such as `type_ = \"string\"` are distinct columns"
)]
#[derive(CustomResource, KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kube(
    group = "builder.zeabur.com",
    version = "v1alpha1",
    kind = "Build",
    namespaced,
    status = "BuildStatus",
    shortname = "zbbuild",
    category = "zeabur-builder",
    printcolumn(name = "Builder", type_ = "string", json_path = ".spec.builderRef"),
    printcolumn(name = "Source", type_ = "string", json_path = ".status.sourceMode"),
    printcolumn(
        name = "Revision",
        type_ = "string",
        json_path = ".status.latestRevision"
    ),
    printcolumn(
        name = "Ready",
        type_ = "string",
        json_path = ".status.conditions[?(@.type==\"Ready\")].status"
    ),
    printcolumn(
        name = "Image",
        type_ = "string",
        json_path = ".status.latestImage",
        priority = 1
    ),
    printcolumn(
        name = "Age",
        type_ = "date",
        json_path = ".metadata.creationTimestamp"
    ),
    doc = "A Git ref or an uploaded archive that Zeabur Builder keeps built into a container image."
)]
#[x_kube(
    validation = Rule::new("!has(self.source.archive) || !has(self.triggers.webhook)").message("an archive source has no webhook trigger")
)]
#[serde(rename_all = "camelCase")]
pub struct BuildSpec {
    /// Where the source comes from.
    pub source: SourceSpec,
    /// The `ClusterBuilder`.
    #[schemars(length(min = 1, max = MAX_NAME))]
    pub builder_ref: String,
    /// Where to push. Default: `<defaultRegistry>/<namespace>/<name>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputSpec>,
    /// Build variables, always passed as `BuildKit` secrets, never as build arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = MAX_ENV))]
    pub env: Vec<EnvVar>,
    /// What starts a build.
    #[serde(default)]
    pub triggers: TriggerSpec,
    /// What a new commit does to a running build.
    #[serde(default)]
    pub concurrency: Concurrency,
    /// Finished `BuildRun`s to keep. The newest success is always kept. Default 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 100))]
    pub history_limit: Option<i32>,
    /// Who to tell after a successful build.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = MAX_NOTIFY))]
    pub notify: Vec<NotifyTarget>,
    /// Stop automatic builds. `BuildRun`s created by hand still run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub suspend: bool,
}

/// The source: a Git repository or an uploaded archive.
#[derive(KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[x_kube(
    validation = Rule::new("has(self.git) != has(self.archive)").message("set exactly one of git and archive"),
    validation = Rule::new("has(self.git) || !(has(self.credentialsRef) || has(self.githubApp))").message("credentialsRef and githubApp need a git source"),
    validation = Rule::new("!(has(self.credentialsRef) && has(self.githubApp))").message("set at most one of credentialsRef and githubApp"),
    validation = Rule::new("!has(self.githubApp) || (has(self.git) && self.git.url.startsWith('https://'))").message("a githubApp source needs an https URL"),
)]
#[serde(rename_all = "camelCase")]
pub struct SourceSpec {
    /// The Git repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitSource>,
    /// An archive pushed as an OCI artifact to `<registry>/<namespace>/<name>/source`
    /// (`docs/builds/archive-source.mdx`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<ArchiveSource>,
    /// Credentials for a plain Git server: a `kubernetes.io/ssh-auth` Secret (with `known_hosts`)
    /// or a `kubernetes.io/basic-auth` one, labelled `builder.zeabur.com/git-credentials: "true"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_ref: Option<SecretRef>,
    /// Reach the repository through an installation of the cluster's GitHub App, which a cluster
    /// admin has allowed for this namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_app: Option<GitHubAppSource>,
}

/// An installation of the cluster's GitHub App.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitHubAppSource {
    /// The installation's id, from `https://github.com/settings/installations/<id>`.
    #[schemars(range(min = 1))]
    pub installation_id: i64,
}

/// A Git repository and ref.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitSource {
    /// `https://`, `ssh://` or `user@host:path`.
    #[schemars(length(min = 1, max = MAX_URL))]
    pub url: String,
    /// A branch or tag; no wildcards. Default `HEAD`: the repository's default branch, which is
    /// resolved again on every poll, so a renamed default branch is followed.
    #[schemars(length(min = 1, max = MAX_REF))]
    #[serde(rename = "ref", default = "default_ref")]
    pub git_ref: String,
    /// The build context inside the repository, for monorepos.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_PATH), regex(pattern = RELATIVE_PATH_PATTERN))]
    pub context_dir: Option<String>,
    /// The Dockerfile, relative to the context, for the `dockerfile` builder. Default
    /// `Dockerfile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_PATH), regex(pattern = RELATIVE_PATH_PATTERN))]
    pub dockerfile: Option<String>,
}

/// An uploaded archive. The repository follows from the `Build`'s namespace and name, so a
/// `Build` reads only its own archives.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveSource {
    /// The artifact's manifest digest. Unset: the `Build` waits for an upload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 71), regex(pattern = DIGEST_PATTERN))]
    pub digest: Option<String>,
    /// The build context inside the archive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_PATH), regex(pattern = RELATIVE_PATH_PATTERN))]
    pub context_dir: Option<String>,
    /// The Dockerfile, relative to the context, for the `dockerfile` builder. Default
    /// `Dockerfile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_PATH), regex(pattern = RELATIVE_PATH_PATTERN))]
    pub dockerfile: Option<String>,
}

impl SourceSpec {
    /// The build context inside the source.
    #[must_use]
    pub fn context_dir(&self) -> Option<&str> {
        match (&self.git, &self.archive) {
            (Some(git), _) => git.context_dir.as_deref(),
            (None, Some(archive)) => archive.context_dir.as_deref(),
            (None, None) => None,
        }
    }

    /// The Dockerfile, relative to the context.
    #[must_use]
    pub fn dockerfile(&self) -> Option<&str> {
        match (&self.git, &self.archive) {
            (Some(git), _) => git.dockerfile.as_deref(),
            (None, Some(archive)) => archive.dockerfile.as_deref(),
            (None, None) => None,
        }
    }
}

fn default_ref() -> String {
    "HEAD".to_owned()
}

/// The image to push.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct OutputSpec {
    /// The image repository, without a tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_IMAGE), regex(pattern = r"^[a-z0-9]([a-z0-9._:/-]*[a-z0-9])?$"))]
    pub image: Option<String>,
    /// The tag: `{{.Field}}` with `.Branch`, `.ShortSHA`, `.SHA`, `.UnixTime` and `.BuildRun`.
    /// Default `{{.Branch}}-{{.UnixTime}}-{{.ShortSHA}}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_TAG_TEMPLATE))]
    pub tag_template: Option<String>,
    /// Fixed tags pushed as well, such as `latest`. Rollout tools should not rely on them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(
        length(max = MAX_EXTRA_TAGS),
        inner(length(max = 128), regex(pattern = r"^[A-Za-z0-9_][A-Za-z0-9._-]{0,127}$"))
    )]
    pub extra_tags: Vec<String>,
    /// Push credentials for an image outside the default registry: a
    /// `kubernetes.io/dockerconfigjson` Secret in the same namespace, labelled
    /// `builder.zeabur.com/push-credentials: "true"`. The default registry gets a token
    /// instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_secret_ref: Option<SecretRef>,
}

/// A build variable.
#[derive(KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[x_kube(
    validation = Rule::new("has(self.value) != has(self.valueFrom)").message("set exactly one of value and valueFrom")
)]
#[serde(rename_all = "camelCase")]
pub struct EnvVar {
    /// The name, as the build sees it.
    #[schemars(
        length(min = 1, max = 128),
        regex(pattern = "^[A-Za-z_][A-Za-z0-9_]*$")
    )]
    pub name: String,
    /// A literal value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 32768))]
    pub value: Option<String>,
    /// A value from a Secret in the same namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_from: Option<EnvVarSource>,
}

/// Where a variable's value comes from.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EnvVarSource {
    /// A key of a Secret.
    pub secret_key_ref: SecretKeyRef,
}

/// What starts a build.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct TriggerSpec {
    /// Push events from a repository webhook, signed with a secret this `Build` names. A
    /// `githubApp` source also wakes on the App's push events, without this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookTrigger>,
    /// How often to check the ref. Default `1m`; `10m`, as a safety net, for a `githubApp`
    /// source when the controller receives webhooks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 32), regex(pattern = DURATION_PATTERN))]
    pub poll_interval: Option<String>,
}

/// A repository webhook.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebhookTrigger {
    /// The shared secret, in a Secret in the same namespace: the webhook's secret on GitHub,
    /// Gitea, Forgejo and Bitbucket, its secret token or signing token on GitLab, and a
    /// `whsec_` key for the generic form.
    pub secret_ref: SecretKeyRef,
}

/// What a new commit does to a build in progress.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema, Default)]
pub enum Concurrency {
    /// Cancel it and build the new commit.
    #[default]
    Replace,
    /// Let it finish, then build the new commit.
    Queue,
}

/// A notification after a successful build.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NotifyTarget {
    /// The receiver.
    #[serde(rename = "type")]
    pub kind: NotifyType,
    /// Where to POST.
    #[schemars(length(min = 1, max = MAX_URL), regex(pattern = "^https?://"))]
    pub url: String,
    /// A token or HMAC key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretRef>,
}

/// Notification receivers.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
pub enum NotifyType {
    /// Keel's native webhook, `{"name", "tag"}`.
    #[serde(rename = "keel")]
    Keel,
}

/// What the controller knows about a `Build`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct BuildStatus {
    /// The generation last acted on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// `GitHubApp`, `PlainGit` or `Archive`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_mode: Option<SourceMode>,
    /// The commit the ref points at, or the archive's digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_REVISION), regex(pattern = REVISION_PATTERN))]
    pub remote_revision: Option<String>,
    /// The full name of the ref `remoteRevision` came from, such as `refs/heads/master` when
    /// `ref` is `HEAD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_RESOLVED_REF))]
    pub resolved_ref: Option<String>,
    /// When the ref was last resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "date-time"))]
    pub last_resolved_at: Option<String>,
    /// When a webhook last woke this `Build`; written by the receiver only. Later than
    /// `lastResolvedAt`: the ref is asked again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "date-time"))]
    pub last_webhook_at: Option<String>,
    /// The commit or archive digest of the newest successful build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_REVISION), regex(pattern = REVISION_PATTERN))]
    pub latest_revision: Option<String>,
    /// Its image, `<repository>:<tag>@<digest>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 1024))]
    pub latest_image: Option<String>,
    /// The revision the controller last created a `BuildRun` for. Deleting that run does not
    /// build the revision again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_REVISION), regex(pattern = REVISION_PATTERN))]
    pub last_run_revision: Option<String>,
    /// The newest `BuildRun`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_NAME))]
    pub last_build_run_ref: Option<String>,
    /// Base images of the newest successful build, `name@sha256:…`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = MAX_BASE_IMAGES), inner(length(max = 1024)))]
    pub base_images: Vec<String>,
    /// Ports the newest successful build's image declares in its `ExposedPorts`, as the image
    /// config writes them (`3000/tcp`, `53/udp`), sorted. Empty when it declares none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = MAX_EXPOSED_PORTS), inner(length(max = 32)))]
    pub exposed_ports: Vec<String>,
    /// `SourceReady`, `BuilderReady` and `Ready`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = MAX_CONDITIONS))]
    pub conditions: Vec<Condition>,
    /// The last upload token the controller issued, for the annotation
    /// `builder.zeabur.com/upload-token-request`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_token: Option<UploadToken>,
}

/// An upload token: a Secret with a short-lived token that pushes to the `Build`'s archive
/// repository.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadToken {
    /// The annotation value it answers.
    #[schemars(length(max = MAX_UPLOAD_REQUEST))]
    pub request: String,
    /// The `kubernetes.io/dockerconfigjson` Secret, in the `Build`'s namespace. Its key `token`
    /// holds the bare token.
    #[schemars(length(max = MAX_NAME))]
    pub secret_name: String,
    /// When the token expires.
    #[schemars(extend("format" = "date-time"))]
    pub expires_at: String,
}

/// How the source is reached.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
pub enum SourceMode {
    /// Through a GitHub App installation.
    GitHubApp,
    /// Over plain Git.
    PlainGit,
    /// From an uploaded archive.
    Archive,
}

/// The `reason` of a `Build` or `ClusterBuilder` condition, written as its name.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConditionReason {
    /// `SourceReady`: neither `git` nor `archive` is set.
    NoSource,
    /// `SourceReady`: the archive has no digest yet.
    WaitingForUpload,
    /// `SourceReady`: the archive's digest is set.
    Archive,
    /// `SourceReady`: the ref resolved to a commit.
    Resolved,
    /// `SourceReady`: the repository has no such ref.
    RefNotFound,
    /// `SourceReady`: the server refused the credentials or the repository.
    AuthFailed,
    /// `SourceReady`: the repository URL is not one the builder can fetch.
    UnsupportedURL,
    /// `SourceReady`: the credentials the source names are missing.
    CredentialsMissing,
    /// `SourceReady`: the credentials do not fit the URL, or do not parse.
    CredentialsInvalid,
    /// `SourceReady`: the credentials' Secret could not be read.
    CredentialsUnreadable,
    /// `SourceReady`: the SSH host key is not in `known_hosts`.
    HostKeyMismatch,
    /// `SourceReady`: the ref could not be asked, or the answer was not understood.
    ResolveFailed,
    /// `SourceReady`: the GitHub App's Secret is missing or unusable.
    GitHubAppMissing,
    /// `SourceReady`: the namespace may not use the App installation.
    InstallationNotAllowed,
    /// `BuilderReady`: the `ClusterBuilder` exists.
    Found,
    /// A `ClusterBuilder`'s `Ready`: its status holds the controller's settings.
    Published,
    /// `BuilderReady`: there is no such `ClusterBuilder`.
    BuilderNotFound,
    /// `Ready`: no run has finished.
    NotBuilt,
    /// `Ready`: the latest finished run succeeded.
    Succeeded,
    /// `Ready`: the latest finished run failed without a reason.
    Failed,
    /// `Ready`: the latest finished run failed or was cancelled for [`Reason::QuotaExceeded`].
    QuotaExceeded,
    /// `Ready`: [`Reason::InsufficientResources`].
    InsufficientResources,
    /// `Ready`: [`Reason::BuildkitdStarting`].
    BuildkitdStarting,
    /// `Ready`: [`Reason::RegistryCredentialsMissing`].
    RegistryCredentialsMissing,
    /// `Ready`: [`Reason::SourceFailed`].
    SourceFailed,
    /// `Ready`: [`Reason::PlanFailed`].
    PlanFailed,
    /// `Ready`: [`Reason::BuildFailed`].
    BuildFailed,
    /// `Ready`: [`Reason::OutOfMemory`].
    OutOfMemory,
    /// `Ready`: [`Reason::Timeout`].
    Timeout,
    /// `Ready`: [`Reason::Cancelled`].
    Cancelled,
    /// `Ready`: [`Reason::InfraError`].
    InfraError,
    /// `Ready`: the run's reason is one this version does not know.
    Unknown,
}

impl std::fmt::Display for ConditionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl From<Reason> for ConditionReason {
    fn from(reason: Reason) -> Self {
        match reason {
            Reason::QuotaExceeded => Self::QuotaExceeded,
            Reason::InsufficientResources => Self::InsufficientResources,
            Reason::BuildkitdStarting => Self::BuildkitdStarting,
            Reason::RegistryCredentialsMissing => Self::RegistryCredentialsMissing,
            Reason::SourceFailed => Self::SourceFailed,
            Reason::PlanFailed => Self::PlanFailed,
            Reason::BuildFailed => Self::BuildFailed,
            Reason::OutOfMemory => Self::OutOfMemory,
            Reason::Timeout => Self::Timeout,
            Reason::Cancelled => Self::Cancelled,
            Reason::InfraError => Self::InfraError,
            Reason::Unknown => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_condition_reason_is_written_as_its_name() {
        for (reason, written) in [
            (ConditionReason::WaitingForUpload, "WaitingForUpload"),
            (ConditionReason::UnsupportedURL, "UnsupportedURL"),
            (ConditionReason::GitHubAppMissing, "GitHubAppMissing"),
            (ConditionReason::BuilderNotFound, "BuilderNotFound"),
            (ConditionReason::NotBuilt, "NotBuilt"),
        ] {
            assert_eq!(reason.to_string(), written);
            assert_eq!(serde_json::to_value(reason).unwrap(), written);
        }
    }

    #[test]
    fn a_run_reason_is_the_same_word_in_the_ready_condition() {
        for reason in [
            Reason::QuotaExceeded,
            Reason::BuildkitdStarting,
            Reason::RegistryCredentialsMissing,
            Reason::SourceFailed,
            Reason::PlanFailed,
            Reason::BuildFailed,
            Reason::OutOfMemory,
            Reason::Timeout,
            Reason::Cancelled,
            Reason::InfraError,
            Reason::Unknown,
        ] {
            assert_eq!(
                ConditionReason::from(reason).to_string(),
                serde_json::to_value(reason).unwrap().as_str().unwrap()
            );
        }
    }
}
