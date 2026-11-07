//! The buildkitd of each build namespace: applied when a run starts, scaled to zero when the
//! namespace has been idle a while, and deleted when the namespace is gone.
//!
//! The objects are [`zeabur_builder_jobspec::buildkitd`]'s, applied with server-side apply, so a
//! changed setting reaches each daemon the next time it is woken. Scaling down goes through the
//! `scale` subresource; the next apply takes `replicas` back. A change apply cannot make, such as
//! the cache volume's size, deletes the `StatefulSet` and its cache, and the next wake creates it
//! again.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{Namespace, Pod, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};
use kube::runtime::reflector::Store;
use kube::{Client, ResourceExt as _};
use zeabur_builder_api::build_run::BuildRun;
use zeabur_builder_api::labels;
use zeabur_builder_core::daemon;
use zeabur_builder_jobspec::buildkitd::{self, DaemonInputs, Resources};

use crate::decide;
use crate::leader::Leader;
use crate::tls::{self, Pems};

const MANAGER: &str = "zeabur-builder";
/// How often idle daemons are looked for.
const SWEEP: Duration = Duration::from_secs(30);
/// Objects read per page of a LIST.
const PAGE: u32 = 500;

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
}

/// A namespace's daemon after [`wake`].
#[derive(Debug, Clone)]
pub(crate) struct Woken {
    /// Its Pod, which takes builds once ready.
    pub(crate) pod: String,
    /// The address Jobs dial.
    pub(crate) address: String,
    /// Whether the Pod is ready.
    pub(crate) ready: bool,
    /// Since when the Pod has been Pending for want of memory or CPU, if it has.
    pub(crate) short_since: Option<Timestamp>,
    /// The client files for the namespace's Jobs.
    pub(crate) client: Pems,
}

fn inputs<'a>(
    settings: &'a Settings,
    namespace: &'a str,
    limits: &'a decide::Limits,
) -> DaemonInputs<'a> {
    DaemonInputs {
        namespace,
        daemon_namespace: &settings.namespace,
        image: &settings.image,
        config_map: &settings.config_map,
        replicas: 1,
        cache_size: &settings.cache_size,
        storage_class: settings.storage_class.as_deref(),
        runtime_class: settings.runtime_class.as_deref(),
        resources: &limits.resources,
        builds: limits.concurrent,
    }
}

async fn apply<K>(client: &Client, namespace: &str, object: &K) -> Result<(), kube::Error>
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
    .map(drop)
}

/// Makes the namespace's certificates and applies its daemon with one replica.
pub(crate) async fn wake(
    client: &Client,
    settings: &Settings,
    namespace: &str,
    limits: &decide::Limits,
) -> Result<Woken, kube::Error> {
    let name = daemon::name(namespace);
    let client_pems = tls::ensure(
        client,
        &settings.namespace,
        &buildkitd::tls_secret(namespace),
        &name,
        namespace,
    )
    .await?;
    let objects = buildkitd::daemon(&inputs(settings, namespace, limits));
    apply(client, &settings.namespace, &objects.network_policy).await?;
    apply(client, &settings.namespace, &objects.service).await?;
    let pod_name = daemon::pod(namespace);
    match apply(client, &settings.namespace, &objects.stateful_set).await {
        // Kubernetes refuses a change to the cache volume (its size or storage class). The volume
        // only holds cache, so the daemon starts over: the StatefulSet goes, its volume with it
        // (`whenDeleted: Delete`), and a later wake creates both again.
        Err(kube::Error::Api(status)) if status.code == 422 => {
            Api::<StatefulSet>::namespaced(client.clone(), &settings.namespace)
                .delete(&name, &DeleteParams::foreground())
                .await?;
            tracing::info!(%namespace, statefulset = %name, "cache volume changed; buildkitd recreated");
            return Ok(Woken {
                pod: pod_name,
                address: daemon::address(namespace, &settings.namespace),
                ready: false,
                client: client_pems,
            });
        }
        applied => applied?,
    }
    let pod = Api::<Pod>::namespaced(client.clone(), &settings.namespace)
        .get_opt(&pod_name)
        .await?;
    let ready = pod.as_ref().is_some_and(pod_ready);
    let short_since = pod.as_ref().and_then(decide::short_of_resources);
    Ok(Woken {
        pod: pod_name,
        address: daemon::address(namespace, &settings.namespace),
        ready,
        short_since,
        client: client_pems,
    })
}

fn pod_ready(pod: &Pod) -> bool {
    pod.metadata.deletion_timestamp.is_none()
        && pod
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
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

async fn remove(
    client: &Client,
    settings: &Settings,
    set: &StatefulSet,
) -> Result<(), kube::Error> {
    let name = set.name_any();
    let ns = &settings.namespace;
    let gone = |e: kube::Error| {
        if let kube::Error::Api(status) = &e
            && status.code == 404
        {
            return Ok(());
        }
        Err(e)
    };
    let params = DeleteParams::background();
    Api::<StatefulSet>::namespaced(client.clone(), ns)
        .delete(&name, &params)
        .await
        .map(drop)
        .or_else(gone)?;
    Api::<Service>::namespaced(client.clone(), ns)
        .delete(&name, &params)
        .await
        .map(drop)
        .or_else(gone)?;
    Api::<NetworkPolicy>::namespaced(client.clone(), ns)
        .delete(&name, &params)
        .await
        .map(drop)
        .or_else(gone)?;
    Api::<Secret>::namespaced(client.clone(), ns)
        .delete(&format!("{name}-tls"), &params)
        .await
        .map(drop)
        .or_else(gone)
}

/// The names of all namespaces, from one paged LIST of their metadata.
async fn namespaces(client: &Client) -> Result<BTreeSet<String>, kube::Error> {
    let api: Api<Namespace> = Api::all(client.clone());
    let mut names = BTreeSet::new();
    let mut params = ListParams::default().limit(PAGE);
    loop {
        let page = api.list_metadata(&params).await?;
        names.extend(page.items.into_iter().filter_map(|n| n.metadata.name));
        match page.metadata.continue_ {
            Some(token) if !token.is_empty() => params = params.continue_token(&token),
            _ => return Ok(names),
        }
    }
}

/// One pass: scales idle daemons to zero and removes those whose namespace is gone. The runs come
/// from the `BuildRun` reconciler's cache.
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
    let existing = namespaces(client).await?;
    let now = Timestamp::now();
    for set in &sets.items {
        let Some(namespace) = set.labels().get(labels::NAMESPACE) else {
            continue;
        };
        if !existing.contains(namespace) {
            remove(client, settings, set).await?;
            tracing::info!(%namespace, statefulset = %set.name_any(), "namespace gone; buildkitd removed");
            continue;
        }
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
