//! Custom resources in the `builder.zeabur.com` API group.
//!
//! - [`cluster_builder::ClusterBuilder`]: how to plan and which `BuildKit` frontend to build with.
//!   Cluster-scoped and written by admins; the chart installs `dockerfile` and `railpack`.
//! - [`build::Build`]: what to build, written by a team. The desired state: follow a Git ref and
//!   keep an image of its newest commit.
//! - [`build_run::BuildRun`]: one build of one commit. Its spec is immutable; the controller
//!   creates them from a `Build`, and people may create them by hand.
//!
//! Format rules (lengths, patterns, list sizes) are `OpenAPI` `maxLength`, `pattern` and
//! `maxItems`, which cost nothing against the CEL cost budget; CEL rules
//! (`x-kubernetes-validations`) are used only for immutability and cross-field checks. Every
//! string and list is bounded, because the API server rejects a definition whose estimated rule
//! cost exceeds the budget ([CRD validation]). Rules that compare a whole object with `oldSelf`
//! also catch a field being removed, which a rule on the field itself would not: a transition
//! rule does not run when the field is absent from the new object.
//!
//! The namespace rule (a `Build` pushes only to its own repositories in the default registry) is
//! kept by the registry: the controller gives each run a token for those repositories only.
//!
//! The default feature `schema` adds `JsonSchema` to every type and [`definitions`]. A crate that
//! only reads and writes the objects can turn it off and builds without schemars.
//!
//! [CRD validation]: https://kubernetes.io/docs/tasks/extend-kubernetes/custom-resources/custom-resource-definitions/#validation-rules

pub mod build;
pub mod build_run;
pub mod cluster_builder;
pub mod common;
#[cfg(feature = "schema")]
mod union;

#[cfg(feature = "schema")]
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
#[cfg(feature = "schema")]
use kube::CustomResourceExt as _;

/// The API group.
pub const GROUP: &str = "builder.zeabur.com";

/// Labels on every Job and Pod of a build, for log collectors and selectors.
pub mod labels {
    /// The `Build`.
    pub const BUILD: &str = "builder.zeabur.com/build";
    /// The `BuildRun`.
    pub const BUILD_RUN: &str = "builder.zeabur.com/buildrun";
    /// The `ClusterBuilder`.
    pub const BUILDER: &str = "builder.zeabur.com/builder";
    /// Marks a Secret the controller may read as Git credentials. Its informer watches only
    /// Secrets with this label set to `"true"`.
    pub const GIT_CREDENTIALS: &str = "builder.zeabur.com/git-credentials";
    /// Marks a Secret the controller may read as push credentials (`spec.output.pushSecretRef`).
    pub const PUSH_CREDENTIALS: &str = "builder.zeabur.com/push-credentials";
    /// On a buildkitd and its objects: the build namespace it serves.
    pub const NAMESPACE: &str = "builder.zeabur.com/namespace";
    /// `app.kubernetes.io/managed-by`.
    pub const MANAGED_BY: &str = "app.kubernetes.io/managed-by";
    /// The value of [`MANAGED_BY`].
    pub const MANAGER: &str = "zeabur-builder";
}

/// Annotations people write to act on a `Build`, and that the controller writes for them.
pub mod annotations {
    /// On a `Build`: a new value rebuilds the ref's current commit. On a `BuildRun` the
    /// controller created: the `Build`'s value when it was created, the request it answered.
    pub const REBUILD: &str = "builder.zeabur.com/rebuild";
    /// On a `Build`: an `http` or `https` URL of the page that shows it, such as a dashboard's.
    /// The commit statuses of its runs link to it as their details.
    pub const DETAILS_URL: &str = "builder.zeabur.com/details-url";
    /// On a `BuildRun` the controller created: the `Build`'s `metadata.generation` when it was
    /// created, which the `Build`'s `Ready` condition compares with the current one.
    pub const BUILD_GENERATION: &str = "builder.zeabur.com/build-generation";
    /// On a Namespace: overrides the controller's limit of concurrent builds, and with it how many
    /// builds its buildkitd runs at once.
    pub const MAX_CONCURRENT_BUILDS: &str = "builder.zeabur.com/max-concurrent-builds";
    /// On a Namespace: overrides the controller's CPU limit of one build, such as `2`.
    pub const BUILD_CPU: &str = "builder.zeabur.com/build-cpu";
    /// On a Namespace: overrides the controller's memory limit of one build, such as `2Gi`.
    pub const BUILD_MEMORY: &str = "builder.zeabur.com/build-memory";
    /// On a Job and Pod: the commit built.
    pub const REVISION: &str = "builder.zeabur.com/revision";
    /// On a Job: the repository it pushes the image and the plan to.
    pub const IMAGE: &str = "builder.zeabur.com/image";
    /// On a Job: the digest of the saved plan it restores instead of running the planner.
    pub const SAVED_PLAN: &str = "builder.zeabur.com/saved-plan";
    /// On a build Job's Pod: what its build container runs, a `BuildPlan` as JSON. The container
    /// reads it as a file: as an argument it can pass Linux's 128 KiB limit for one argument.
    pub const BUILD_PLAN: &str = "builder.zeabur.com/build-plan";
    /// On a `Build` with an archive source: a new value asks for an upload token
    /// (`status.uploadToken`).
    pub const UPLOAD_TOKEN_REQUEST: &str = "builder.zeabur.com/upload-token-request";
    /// On a `Build`: [`PLAN_FRESH`] makes each new Job run the planner.
    ///
    /// Without it, a new Job builds with the plan the last successful run saved (`status.plan`),
    /// when the `Build` still names that plan's builder and repository.
    pub const PLAN: &str = "builder.zeabur.com/plan";
    /// The value of [`PLAN`] that plans every run again.
    pub const PLAN_FRESH: &str = "fresh";
    /// On a buildkitd `StatefulSet`: the SHA-256 of the spec the controller applied.
    pub const SPEC_HASH: &str = "builder.zeabur.com/spec-hash";
    /// On a buildkitd Pod template: the SHA-256 of the default registry's CA. buildkitd keeps the
    /// TLS clients of the registries it pulls from, with the CA they read, so a new CA restarts it.
    pub const REGISTRY_CA_HASH: &str = "builder.zeabur.com/registry-ca-hash";
}

/// Every custom resource definition, in a stable order.
#[cfg(feature = "schema")]
#[must_use]
pub fn definitions() -> Vec<CustomResourceDefinition> {
    vec![
        cluster_builder::ClusterBuilder::crd(),
        build::Build::crd(),
        build_run::BuildRun::crd(),
    ]
}

/// Renders all definitions as a YAML stream of JSON documents, without a trailing newline. Both
/// `kubectl apply -f -` and the `crds/` directory of a Helm chart read it; Helm refuses a `List`.
///
/// # Errors
///
/// Returns an error if serialization fails.
#[cfg(feature = "schema")]
pub fn definitions_yaml() -> serde_json::Result<String> {
    let documents = definitions()
        .iter()
        .map(serde_json::to_string_pretty)
        .collect::<serde_json::Result<Vec<_>>>()?;
    Ok(format!("---\n{}", documents.join("\n---\n")))
}

#[cfg(test)]
#[cfg(feature = "schema")]
mod tests {
    use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::{
        JSONSchemaProps, JSONSchemaPropsOrArray, JSONSchemaPropsOrBool,
    };

    use super::*;

    /// The committed manifest must match the code. Regenerate with
    /// `cargo run -q -- crds > deploy/chart/crds/crds.yaml`.
    #[test]
    fn committed_manifest_is_current() {
        let committed = include_str!("../../../deploy/chart/crds/crds.yaml");
        let parse = |s: &str| -> Vec<CustomResourceDefinition> {
            s.split("---\n")
                .filter(|d| !d.trim().is_empty())
                .map(|d| serde_json::from_str(d).unwrap())
                .collect()
        };
        assert!(
            committed.ends_with('\n') && parse(committed) == parse(&definitions_yaml().unwrap()),
            "deploy/chart/crds/crds.yaml is stale: run `cargo run -q -- crds > deploy/chart/crds/crds.yaml`"
        );
    }

    #[test]
    fn kinds_and_scopes() {
        let scopes: Vec<(String, String)> = definitions()
            .into_iter()
            .map(|crd| (crd.spec.names.kind, crd.spec.scope))
            .collect();
        assert_eq!(
            scopes,
            [
                ("ClusterBuilder", "Cluster"),
                ("Build", "Namespaced"),
                ("BuildRun", "Namespaced")
            ]
            .map(|(k, s)| (k.to_owned(), s.to_owned()))
        );
        for crd in definitions() {
            assert_eq!(crd.spec.group, GROUP);
            assert_eq!(crd.spec.versions.len(), 1);
            assert_eq!(crd.spec.versions[0].name, "v1alpha1");
        }
    }

    /// Every string, list and map in every schema has an upper bound, so CEL cost estimates stay
    /// finite.
    #[test]
    fn every_value_is_bounded() {
        fn walk(path: &str, schema: &JSONSchemaProps, unbounded: &mut Vec<String>) {
            let bounded = match schema.type_.as_deref() {
                Some("string") => {
                    schema.max_length.is_some() || schema.enum_.is_some() || schema.format.is_some()
                }
                Some("array") => schema.max_items.is_some(),
                Some("object") if schema.additional_properties.is_some() => {
                    schema.max_properties.is_some()
                }
                _ => true,
            };
            if !bounded {
                unbounded.push(path.to_owned());
            }
            for (name, property) in schema.properties.iter().flatten() {
                walk(&format!("{path}.{name}"), property, unbounded);
            }
            if let Some(JSONSchemaPropsOrArray::Schema(items)) = &schema.items {
                walk(&format!("{path}[]"), items, unbounded);
            }
            if let Some(JSONSchemaPropsOrBool::Schema(values)) = &schema.additional_properties {
                walk(&format!("{path}[]"), values, unbounded);
            }
        }
        let mut unbounded = Vec::new();
        for crd in definitions() {
            let version = &crd.spec.versions[0];
            let schema = version
                .schema
                .as_ref()
                .unwrap()
                .open_api_v3_schema
                .as_ref()
                .unwrap();
            for part in ["spec", "status"] {
                walk(
                    &format!("{}.{part}", crd.spec.names.kind),
                    &schema.properties.as_ref().unwrap()[part],
                    &mut unbounded,
                );
            }
        }
        assert!(unbounded.is_empty(), "unbounded: {unbounded:#?}");
    }
}
