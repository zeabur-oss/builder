//! The buildkitd of each build namespace: applied when a run starts and scaled to zero when the
//! namespace has been idle a while. Its objects name the namespace as their owner, so Kubernetes
//! garbage collection deletes them with the namespace.
//!
//! The objects are [`zeabur_builder_jobspec::buildkitd`]'s, applied with server-side apply.
//! Scaling down goes through the `scale` subresource; the next apply takes `replicas` back. The
//! daemon is a cache: a running `StatefulSet` whose spec differs from the one wanted, such as a new
//! cache size, storage class, build node or build limits, keeps running as it is. Once it has
//! scaled to zero, the next wake deletes it with its cache and creates it again.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{Namespace, Pod};
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, Preconditions};
use kube::runtime::reflector::Store;
use kube::{Client, ResourceExt as _};
use zeabur_builder_api::build_run::BuildRun;
use zeabur_builder_api::{annotations, labels};
use zeabur_builder_core::daemon;
use zeabur_builder_jobspec::Placement;
use zeabur_builder_jobspec::buildkitd::{self, DaemonInputs, Resources};

use crate::decide;
use crate::leader::Leader;
use crate::tls::{self, Pems};

const MANAGER: &str = "zeabur-builder";
/// How often idle daemons are looked for.
const SWEEP: Duration = Duration::from_secs(30);

/// How every namespace's buildkitd runs, from the command line.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// The builder's namespace, where the daemons run.
    pub(crate) namespace: String,
    /// `moby/buildkit`.
    pub(crate) image: String,
    /// The `ConfigMap` with `buildkitd.toml`.
    pub(crate) config_map: String,
    /// The cache volume's size.
    pub(crate) cache_size: String,
    /// The cache volume's storage class.
    pub(crate) storage_class: Option<String>,
    /// A sandboxed runtime class.
    pub(crate) runtime_class: Option<String>,
    /// The resources of one build.
    pub(crate) resources: Resources,
    /// How long a namespace goes without builds before its daemon scales to zero.
    pub(crate) idle_after: Duration,
    /// The nodes the daemons and build Jobs run on.
    pub(crate) placement: Placement,
}

/// A namespace's ready daemon.
#[derive(Debug, Clone)]
pub(crate) struct Woken {
    /// Its Pod, which takes builds once ready.
    pub(crate) pod: String,
    /// The address Jobs dial.
    pub(crate) address: String,
    /// The client files for the namespace's Jobs.
    pub(crate) client: Pems,
}

fn inputs<'a>(
    settings: &'a Settings,
    namespace: &'a str,
    namespace_uid: &'a str,
    limits: &'a decide::Limits,
) -> DaemonInputs<'a> {
    DaemonInputs {
        namespace,
        namespace_uid,
        daemon_namespace: &settings.namespace,
        image: &settings.image,
        config_map: &settings.config_map,
        replicas: 1,
        cache_size: &settings.cache_size,
        storage_class: settings.storage_class.as_deref(),
        runtime_class: settings.runtime_class.as_deref(),
        resources: &limits.resources,
        placement: &settings.placement,
        builds: limits.concurrent,
    }
}

async fn apply<K>(client: &Client, namespace: &str, object: &K) -> Result<K, kube::Error>
where
    K: kube::Resource<DynamicType = (), Scope = kube::core::NamespaceResourceScope>
        + Clone
        + serde::Serialize
        + serde::de::DeserializeOwned
        + std::fmt::Debug
        + Sync,
{
    let api: Api<K> = Api::namespaced(client.clone(), namespace);
    let mut body = serde_json::to_value(object).map_err(kube::Error::SerdeError)?;
    if let Some(map) = body.as_object_mut() {
        map.insert("apiVersion".to_owned(), K::api_version(&()).into());
        map.insert("kind".to_owned(), K::kind(&()).into());
    }
    api.patch(
        &object.name_any(),
        &PatchParams::apply(MANAGER).force(),
        &Patch::Apply(body),
    )
    .await
}

/// What [`wake`] found.
#[derive(Debug, Clone)]
pub(crate) enum Wake {
    /// The daemon's Pod is ready.
    Ready(Woken),
    /// The daemon is starting or being replaced; why the run waits.
    Starting(String),
    /// The daemon's Pod cannot be scheduled for want of memory or CPU, since the given time.
    Short { pod: String, since: Timestamp },
}

/// Makes the namespace's certificates and applies its daemon with one replica.
pub(crate) async fn wake(
    client: &Client,
    settings: &Settings,
    namespace: &str,
    limits: &decide::Limits,
) -> Result<Wake, kube::Error> {
    let name = daemon::name(namespace);
    let uid = Api::<Namespace>::all(client.clone())
        .get(namespace)
        .await?
        .uid()
        .unwrap_or_default();
    let gone = || {
        Ok(Wake::Starting(format!(
            "waiting for garbage collection to delete {name} of a deleted namespace with the same name"
        )))
    };
    let sets: Api<StatefulSet> = Api::namespaced(client.clone(), &settings.namespace);
    let old = sets.get_opt(&name).await?;
    if old
        .as_ref()
        .is_some_and(|o| buildkitd::of_deleted_namespace(&o.metadata, &uid))
    {
        return gone();
    }
    let Some(client_pems) = tls::ensure(
        client,
        &settings.namespace,
        &buildkitd::tls_secret(namespace),
        &name,
        &buildkitd::owner(namespace, &uid),
    )
    .await?
    else {
        return gone();
    };
    let objects = buildkitd::daemon(&inputs(settings, namespace, &uid, limits));
    apply(client, &settings.namespace, &objects.network_policy).await?;
    apply(client, &settings.namespace, &objects.service).await?;
    let desired = &objects.stateful_set;
    let set = match old {
        Some(old) if old.metadata.deletion_timestamp.is_some() => {
            return Ok(Wake::Starting(format!(
                "waiting for the old {name} to be deleted"
            )));
        }
        Some(old) if spec_hash(&old) != spec_hash(desired) => {
            let replicas = old.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0);
            if replicas > 0 {
                old
            } else {
                replace(&sets, &old).await?;
                return Ok(Wake::Starting(format!(
                    "{name} is being replaced with the new settings"
                )));
            }
        }
        _ => match apply(client, &settings.namespace, desired).await {
            Ok(set) => set,
            Err(kube::Error::Api(status)) if status.code == 409 => {
                return Ok(Wake::Starting(format!(
                    "{name} changed while being applied: {}",
                    status.message
                )));
            }
            Err(e) => return Err(e),
        },
    };
    let pod_name = daemon::pod(namespace);
    let pod = Api::<Pod>::namespaced(client.clone(), &settings.namespace)
        .get_opt(&pod_name)
        .await?;
    if !pod.as_ref().is_some_and(|p| ready(p, &set)) {
        if let Some(since) = pod.as_ref().and_then(decide::short_of_resources) {
            return Ok(Wake::Short {
                pod: pod_name,
                since,
            });
        }
        return Ok(Wake::Starting(format!(
            "waiting for {pod_name} to be ready"
        )));
    }
    Ok(Wake::Ready(Woken {
        pod: pod_name,
        address: daemon::address(namespace, &settings.namespace),
        client: client_pems,
    }))
}

/// Whether `pod` is ready, belongs to `set` and not to a `StatefulSet` of the same name that was
/// deleted, and is of the revision that `set` rolls out. A Pod of the old revision is about to be
/// replaced.
fn ready(pod: &Pod, set: &StatefulSet) -> bool {
    let owned = pod
        .owner_references()
        .iter()
        .any(|o| o.controller == Some(true) && Some(&o.uid) == set.metadata.uid.as_ref());
    let status = set.status.as_ref();
    let seen = status.and_then(|s| s.observed_generation) == set.metadata.generation;
    let revision = status.and_then(|s| s.update_revision.as_ref());
    let current = revision.is_some() && pod.labels().get("controller-revision-hash") == revision;
    owned && seen && current && pod_ready(pod)
}

fn pod_ready(pod: &Pod) -> bool {
    pod.metadata.deletion_timestamp.is_none()
        && pod
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
}

fn spec_hash(set: &StatefulSet) -> Option<&String> {
    set.annotations().get(annotations::SPEC_HASH)
}

/// Deletes the daemon's `StatefulSet` in the foreground: it stays, with a `deletionTimestamp`,
/// until its Pod and cache claim are gone, since the `StatefulSet` controller makes itself their
/// owner with `blockOwnerDeletion` ([`updateClaimOwnerRefForSetAndPod`]). Until then a new one of
/// the same name would take over the old claim. The `uid` and `resourceVersion` preconditions
/// spare a `StatefulSet` created or scaled up after the read.
///
/// [`updateClaimOwnerRefForSetAndPod`]: https://github.com/kubernetes/kubernetes/blob/v1.34.0/pkg/controller/statefulset/stateful_set_utils.go#L305-L350
async fn replace(sets: &Api<StatefulSet>, old: &StatefulSet) -> Result<(), kube::Error> {
    let name = old.name_any();
    let delete = DeleteParams {
        preconditions: Some(Preconditions {
            uid: old.metadata.uid.clone(),
            resource_version: old.metadata.resource_version.clone(),
        }),
        ..DeleteParams::foreground()
    };
    match sets.delete(&name, &delete).await {
        Ok(_) => {
            tracing::info!(statefulset = %name, "buildkitd replaced with new settings");
            Ok(())
        }
        Err(kube::Error::Api(status)) if status.code == 404 || status.code == 409 => Ok(()),
        Err(e) => Err(e),
    }
}

fn selector() -> String {
    format!(
        "{}={},app.kubernetes.io/name={}",
        labels::MANAGED_BY,
        labels::MANAGER,
        buildkitd::APP
    )
}

async fn scale_to_zero(
    client: &Client,
    settings: &Settings,
    set: &StatefulSet,
) -> Result<(), kube::Error> {
    Api::<StatefulSet>::namespaced(client.clone(), &settings.namespace)
        .patch_scale(
            &set.name_any(),
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({"spec": {"replicas": 0}})),
        )
        .await
        .map(drop)
}

/// Scales to zero the daemon of the namespace that has been idle longest, other than `mine`, and
/// returns the namespace, unless a live list shows it has an admitted run. Called while `mine`
/// waits for memory or CPU.
pub(crate) async fn free_one(
    client: &Client,
    settings: &Settings,
    runs: &[&BuildRun],
    mine: &str,
    quiet: Duration,
) -> Result<Option<String>, kube::Error> {
    let sets: Api<StatefulSet> = Api::namespaced(client.clone(), &settings.namespace);
    let sets = sets
        .list(&ListParams::default().labels(&selector()))
        .await?;
    let mut up = Vec::new();
    for set in &sets.items {
        let replicas = set.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0);
        if let (true, Some(namespace)) = (replicas > 0, set.labels().get(labels::NAMESPACE)) {
            up.push(namespace.clone());
        }
    }
    let Some(namespace) = decide::idlest(&up, runs, mine, Timestamp::now(), quiet) else {
        return Ok(None);
    };
    let live = Api::<BuildRun>::namespaced(client.clone(), namespace)
        .list(&ListParams::default())
        .await?;
    if live.items.iter().any(decide::holds_slot) {
        return Ok(None);
    }
    for set in &sets.items {
        if set.labels().get(labels::NAMESPACE).map(String::as_str) == Some(namespace) {
            scale_to_zero(client, settings, set).await?;
            tracing::info!(%namespace, waiting = %mine, "buildkitd scaled to zero to free memory");
        }
    }
    Ok(Some(namespace.to_owned()))
}

/// The namespaces whose daemon Pod is ready.
pub(crate) async fn ready_namespaces(
    client: &Client,
    settings: &Settings,
) -> Result<BTreeSet<String>, kube::Error> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), &settings.namespace);
    let pods = pods
        .list(&ListParams::default().labels(&selector()))
        .await?;
    let mut ready = BTreeSet::new();
    for pod in &pods.items {
        if let (true, Some(namespace)) = (pod_ready(pod), pod.labels().get(labels::NAMESPACE)) {
            ready.insert(namespace.clone());
        }
    }
    Ok(ready)
}

/// One pass: scales idle daemons to zero. The runs come from the `BuildRun` reconciler's cache.
async fn sweep(
    client: &Client,
    settings: &Settings,
    runs: &Store<BuildRun>,
) -> Result<(), kube::Error> {
    let sets: Api<StatefulSet> = Api::namespaced(client.clone(), &settings.namespace);
    let sets = sets
        .list(&ListParams::default().labels(&selector()))
        .await?;
    if sets.items.is_empty() {
        return Ok(());
    }
    let mut runs_by_namespace: BTreeMap<String, Vec<Arc<BuildRun>>> = BTreeMap::new();
    for run in runs.state() {
        runs_by_namespace
            .entry(run.namespace().unwrap_or_default())
            .or_default()
            .push(run);
    }
    let now = Timestamp::now();
    for set in &sets.items {
        let Some(namespace) = set.labels().get(labels::NAMESPACE) else {
            continue;
        };
        let replicas = set.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0);
        let runs = runs_by_namespace
            .get(namespace)
            .map_or(&[][..], Vec::as_slice);
        if replicas > 0 && decide::idle(runs.iter().map(AsRef::as_ref), now, settings.idle_after) {
            scale_to_zero(client, settings, set).await?;
            tracing::info!(%namespace, statefulset = %set.name_any(), "idle; buildkitd scaled to zero");
        }
    }
    Ok(())
}

/// Sweeps every 30 seconds while this replica is the leader, until the process ends. The first
/// sweep waits for the cache of runs: an empty cache would make every daemon look idle.
#[expect(clippy::infinite_loop, reason = "runs for the life of the process")]
pub(crate) async fn sweep_forever(
    client: Client,
    settings: Arc<Settings>,
    runs: Store<BuildRun>,
    leader: Leader,
) {
    if runs.wait_until_ready().await.is_err() {
        return;
    }
    let mut interval = tokio::time::interval(SWEEP);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if !leader.is_leader() {
            continue;
        }
        if let Err(e) = sweep(&client, &settings, &runs).await {
            tracing::warn!(error = %e, "buildkitd sweep");
        }
    }
}

#[cfg(test)]
mod tests;
