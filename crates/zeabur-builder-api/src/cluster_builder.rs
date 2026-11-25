//! `ClusterBuilder`: how to plan a build and which frontend builds it.

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube_derive::CustomResource;
#[cfg(feature = "schema")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[cfg(feature = "schema")]
use crate::build::MAX_CONDITIONS;
use crate::common::{Condition, SecretKeyRef};
#[cfg(feature = "schema")]
use crate::common::{DURATION_PATTERN, MAX_NAME};

/// Longest image reference.
pub const MAX_IMAGE: usize = 512;
/// Most frontend options.
pub const MAX_OPTS: usize = 32;
/// Longest frontend option name, in characters.
pub const MAX_OPT_KEY: usize = 128;
/// Longest frontend option value, in characters. With the other limits, the build plan of the
/// largest Build stays under the 256 KiB a Pod's annotations may hold, even in 4-byte characters.
pub const MAX_OPT_VALUE: usize = 512;
/// Longest quantity in the status.
pub const MAX_QUANTITY: usize = 64;
/// Longest model provider id, such as a federation rule.
#[cfg(feature = "schema")]
const MAX_ID: usize = 256;

/// A way to build. Every builder reduces to the same shape: an optional planner that writes files
/// into `/workspace/plan`, then a `BuildKit` frontend with options that reads them.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[kube(
    group = "builder.zeabur.com",
    version = "v1alpha1",
    kind = "ClusterBuilder",
    shortname = "cbuilder",
    category = "zeabur-builder",
    status = "ClusterBuilderStatus",
    printcolumn(name = "Type", type_ = "string", json_path = ".spec.type"),
    printcolumn(name = "Frontend", type_ = "string", json_path = ".spec.frontend.type"),
    printcolumn(
        name = "Ready",
        type_ = "string",
        json_path = ".status.conditions[?(@.type==\"Ready\")].status"
    ),
    printcolumn(
        name = "Age",
        type_ = "date",
        json_path = ".metadata.creationTimestamp"
    ),
    doc = "How Zeabur Builder plans and builds: Dockerfile, Railpack or the agent."
)]
#[cfg_attr(not(feature = "schema"), kube(schema = "disabled"))]
#[serde(rename_all = "camelCase")]
pub struct ClusterBuilderSpec {
    /// What the planning step does.
    #[serde(flatten)]
    pub planner: Planner,
    /// The frontend that builds the plan.
    pub frontend: FrontendSpec,
}

/// The controller's settings for every build, whatever the builder. Each value is the
/// controller's command-line setting; a Namespace annotation can change some of them for its
/// builds.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ClusterBuilderStatus {
    /// What one build gets on its namespace's buildkitd when the Namespace annotations
    /// `builder.zeabur.com/build-cpu` and `builder.zeabur.com/build-memory` do not say otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_defaults: Option<BuildDefaults>,
    /// The most one build may get, whatever a Namespace annotation asks. A missing `cpu` or
    /// `memory` means no ceiling for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_ceiling: Option<BuildCeiling>,
    /// Builds a namespace runs at once when its annotation
    /// `builder.zeabur.com/max-concurrent-builds` does not say otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_concurrent_builds: Option<u32>,
    /// `Ready` is `True` once the controller that holds the lease has written this status for the
    /// current generation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "schema", schemars(length(max = MAX_CONDITIONS)))]
    pub conditions: Vec<Condition>,
}

/// One build's CPU and memory by default, laid out as Kubernetes writes `resources`. A missing
/// value means the controller sets none.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct BuildDefaults {
    /// `--buildkitd-cpu-request` and `--buildkitd-memory-request`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<BuildQuantities>,
    /// `--buildkitd-cpu-limit` and `--buildkitd-memory-limit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<BuildQuantities>,
}

/// A CPU and a memory quantity.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct BuildQuantities {
    /// CPU, such as `100m` or `1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>", length(max = MAX_QUANTITY)))]
    pub cpu: Option<Quantity>,
    /// Memory, such as `256Mi`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>", length(max = MAX_QUANTITY)))]
    pub memory: Option<Quantity>,
}

/// The most CPU and memory one build may get.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct BuildCeiling {
    /// `--buildkitd-cpu-max`; missing: no ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>", length(max = MAX_QUANTITY)))]
    pub cpu: Option<Quantity>,
    /// `--buildkitd-memory-max`; missing: no ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>", length(max = MAX_QUANTITY)))]
    pub memory: Option<Quantity>,
}

/// The planners.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "type", rename_all = "camelCase")]
#[cfg_attr(feature = "schema", schemars(transform = crate::union::tagged))]
pub enum Planner {
    /// The repository's own Dockerfile; nothing to plan.
    Dockerfile,
    /// Railpack plans; its gateway frontend builds.
    Railpack {
        /// The Railpack planner.
        railpack: PlannerSpec,
    },
    /// An agent writes a Dockerfile and trial-builds it; a Dockerfile frontend builds it.
    Agent {
        /// The agent planner.
        agent: AgentPlanner,
    },
}

/// A planner's container.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct PlannerSpec {
    /// The planner image, pinned by digest in production.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = MAX_IMAGE)))]
    pub image: String,
    /// How long planning may take. Default `10m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(length(max = 32), regex(pattern = DURATION_PATTERN)))]
    pub timeout: Option<String>,
    /// CPU and memory for the plan container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
}

/// The agent planner and the model it calls.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct AgentPlanner {
    /// The planner's container.
    #[serde(flatten)]
    pub planner: PlannerSpec,
    /// The model.
    pub model: ModelSpec,
}

/// The model of the `agent` planner.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", schemars(extend("x-kubernetes-validations" = [
    {
        "rule": "self.auth.type != 'anthropicFederation' || self.provider == 'anthropic'",
        "message": "anthropicFederation needs provider anthropic",
    },
    {
        "rule": "self.auth.type != 'openaiFederation' || self.provider == 'openai'",
        "message": "openaiFederation needs provider openai",
    },
    {
        "rule": "(self.auth.type == 'awsFederation') == (self.provider == 'aws_bedrock')",
        "message": "provider aws_bedrock signs in with awsFederation, and only it",
    },
])))]
pub struct ModelSpec {
    /// The provider, by Rig's name, such as `anthropic`, `openai`, `gcp.gemini` or `openrouter`.
    /// It sets the endpoint and the API format.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = 64)))]
    pub provider: String,
    /// The model, as the provider names it, such as `claude-opus-5-5`.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = 256)))]
    pub model: String,
    /// The model API's base URL, for a proxy or a self-hosted endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(length(max = 2048)))]
    pub base_url: Option<String>,
    /// How the run signs in to the model API.
    pub auth: ModelAuth,
}

/// How a run signs in to the model API. A run waits with `ModelKeyMissing` while the Secret or
/// the service account it names is missing from the `Build`'s namespace.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[cfg_attr(feature = "schema", schemars(transform = crate::union::tagged))]
pub enum ModelAuth {
    /// An API key in a Secret.
    ApiKey {
        /// The Secret key that holds the model API key, in each `Build`'s own namespace.
        api_key: SecretKeyRef,
    },
    /// Anthropic's workload identity federation: a service account token for a short-lived
    /// access token.
    AnthropicFederation {
        /// The federation rule, and the service account whose token the run exchanges.
        anthropic_federation: AnthropicFederation,
    },
    /// OpenAI's workload identity federation: a service account token for a short-lived access
    /// token.
    OpenaiFederation {
        /// The identity provider, and the service account whose token the run exchanges.
        openai_federation: OpenAiFederation,
    },
    /// An IAM role assumed with a service account token: temporary credentials that sign each
    /// Amazon Bedrock request.
    AwsFederation {
        /// The role and region, and the service account whose token the run exchanges.
        aws_federation: AwsFederation,
    },
}

impl ModelAuth {
    /// What the run reads from its namespace to sign in.
    #[must_use]
    pub const fn credential(&self) -> Credential<'_> {
        match self {
            Self::ApiKey { api_key } => Credential::Secret(api_key),
            Self::AnthropicFederation {
                anthropic_federation:
                    AnthropicFederation {
                        service_account_name,
                        ..
                    },
            }
            | Self::OpenaiFederation {
                openai_federation:
                    OpenAiFederation {
                        service_account_name,
                        ..
                    },
            }
            | Self::AwsFederation {
                aws_federation:
                    AwsFederation {
                        service_account_name,
                        ..
                    },
            } => Credential::ServiceAccount(service_account_name.as_str()),
        }
    }
}

/// What a run signs in to the model API with, in the `Build`'s namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential<'a> {
    /// A Secret key with an API key.
    Secret(&'a SecretKeyRef),
    /// A service account whose projected token the run exchanges.
    ServiceAccount(&'a str),
}

/// Anthropic's federation rule. The projected token's audience is `https://api.anthropic.com`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct AnthropicFederation {
    /// The Kubernetes service account, in each `Build`'s own namespace, whose token the run
    /// exchanges.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = MAX_NAME)))]
    pub service_account_name: String,
    /// The federation rule, `fdrl_…`.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = MAX_ID)))]
    pub federation_rule_id: String,
    /// The organization.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = MAX_ID)))]
    pub organization_id: String,
    /// The workspace, `wrkspc_…`, needed when the rule has several.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = MAX_ID)))]
    pub workspace_id: Option<String>,
    /// Anthropic's service account, `svac_…`, when the rule does not name one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = MAX_ID)))]
    pub service_account_id: Option<String>,
}

impl AnthropicFederation {
    /// The audience Anthropic takes service account tokens for.
    pub const AUDIENCE: &str = "https://api.anthropic.com";
}

/// OpenAI's workload identity provider. The projected token's audience is
/// `https://api.openai.com/v1`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct OpenAiFederation {
    /// The Kubernetes service account, in each `Build`'s own namespace, whose token the run
    /// exchanges.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = MAX_NAME)))]
    pub service_account_name: String,
    /// The workload identity provider.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = MAX_ID)))]
    pub identity_provider_id: String,
    /// OpenAI's service account the token maps to.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = MAX_ID)))]
    pub service_account_id: String,
}

impl OpenAiFederation {
    /// The audience OpenAI takes service account tokens for.
    pub const AUDIENCE: &str = "https://api.openai.com/v1";
}

/// The IAM role a run assumes with `AssumeRoleWithWebIdentity`. The projected token's audience is
/// `sts.amazonaws.com`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct AwsFederation {
    /// The Kubernetes service account, in each `Build`'s own namespace, whose token the run
    /// exchanges.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = MAX_NAME)))]
    pub service_account_name: String,
    /// The role, `arn:aws:iam::<account>:role/<name>`, whose trust policy names the cluster's
    /// IAM OIDC identity provider.
    #[cfg_attr(feature = "schema", schemars(length(min = 20, max = 2048)))]
    pub role_arn: String,
    /// The region of Amazon Bedrock and STS, such as `us-east-1`.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = 64)))]
    pub region: String,
}

impl AwsFederation {
    /// The audience the run asks for; the IAM OIDC identity provider lists it.
    pub const AUDIENCE: &str = "sts.amazonaws.com";
}

/// CPU and memory, as Kubernetes quantities.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct Resources {
    /// Requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<ResourceList>,
    /// Limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<ResourceList>,
}

/// A CPU and memory amount.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct ResourceList {
    /// CPU, such as `500m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(length(max = 32)))]
    pub cpu: Option<String>,
    /// Memory, such as `1Gi`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(length(max = 32)))]
    pub memory: Option<String>,
}

/// The `BuildKit` frontend.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct FrontendSpec {
    /// Built into buildkitd, or an image.
    #[serde(flatten)]
    pub kind: Frontend,
    /// Frontend options added to every build, as `buildctl --opt key=value`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[cfg_attr(feature = "schema", schemars(extend(
        "maxProperties" = MAX_OPTS,
        "additionalProperties" = {"type": "string", "maxLength": MAX_OPT_VALUE},
        "x-kubernetes-validations" = [{
            "rule": format!("self.all(k, size(k) <= {MAX_OPT_KEY})"),
            "message": format!("an option name is at most {MAX_OPT_KEY} characters"),
        }],
    )))]
    pub opts: BTreeMap<String, String>,
}

/// Frontend kinds.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "type")]
#[cfg_attr(feature = "schema", schemars(transform = crate::union::tagged))]
pub enum Frontend {
    /// The Dockerfile frontend in buildkitd.
    #[serde(rename = "dockerfile.v0")]
    Dockerfile,
    /// A frontend image.
    #[serde(rename = "gateway.v0")]
    Gateway {
        /// The frontend image, such as `ghcr.io/railwayapp/railpack-frontend:v0.43.0` for
        /// `railpack`, whose version must match the planner's, or `docker/dockerfile:1`.
        #[cfg_attr(feature = "schema", schemars(length(min = 1, max = MAX_IMAGE)))]
        source: String,
    },
}
