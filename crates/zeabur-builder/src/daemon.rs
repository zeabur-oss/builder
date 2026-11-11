//! The buildkitd of each build namespace: applied when a run starts, scaled to zero when the
//! namespace has been idle a while, and deleted when the namespace is gone.
//!
//! The objects are [`zeabur_builder_jobspec::buildkitd`]'s, applied with server-side apply, so a
//! changed setting reaches each daemon the next time it is woken. Scaling down goes through the
//! `scale` subresource; the next apply takes `replicas` back. A change apply cannot make, such as
//! the cache volume's size or storage class, replaces the `StatefulSet` and its cache once no
//! build uses the daemon ([`stateful_set`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{Namespace, PersistentVolumeClaim, Pod, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams, Preconditions};
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
    /// The API server rejects the new `StatefulSet` itself. The old one stays.
    Invalid(String),
}

/// Makes the namespace's certificates and applies its daemon with one replica.
pub(crate) async fn wake(
    client: &Client,
    settings: &Settings,
    namespace: &str,
    limits: &decide::Limits,
) -> Result<Wake, kube::Error> {
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
    let set = match stateful_set(
        client,
        &settings.namespace,
        namespace,
        &objects.stateful_set,
    )
    .await?
    {
        Applied::Set(set) => set,
        Applied::Waiting(message) => return Ok(Wake::Starting(message)),
        Applied::Invalid(message) => return Ok(Wake::Invalid(message)),
    };
    let pod_name = daemon::pod(namespace);
    let pod = Api::<Pod>::namespaced(client.clone(), &settings.namespace)
        .get_opt(&pod_name)
        .await?;
    if !pod.is_some_and(|p| ready(&p, &set)) {
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

/// Whether `pod` is ready and belongs to `set`, not to a `StatefulSet` of the same name that was
/// deleted.
fn ready(pod: &Pod, set: &StatefulSet) -> bool {
    let owned = pod
        .owner_references()
        .iter()
        .any(|o| o.controller == Some(true) && Some(&o.uid) == set.metadata.uid.as_ref());
    owned
        && pod.metadata.deletion_timestamp.is_none()
        && pod
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
}

/// The outcome of applying a daemon's `StatefulSet`.
#[derive(Debug)]
enum Applied {
    /// The `StatefulSet` as the API server holds it.
    Set(Box<StatefulSet>),
    /// Not applied yet; why.
    Waiting(String),
    /// The API server rejects `desired` as a new object; the message says why.
    Invalid(String),
}

/// Applies `desired`, the daemon of `namespace` in `daemon_namespace`, and replaces the
/// `StatefulSet` when an update cannot make the change.
///
/// - An update the API server refuses with `422 Unprocessable Entity`, such as a change to
///   `volumeClaimTemplates`, is checked with a dry-run create under a generated name ([Dry-run]).
///   If the dry run is also refused with 422, `desired` is invalid and the old `StatefulSet` stays.
///   Any other dry-run error is returned as it is.
/// - Otherwise the old `StatefulSet` is read again and deleted with its `uid` and `resourceVersion`
///   as preconditions ([`Preconditions`]), so an object that changed or was replaced after the read
///   survives: the API server answers `409 Conflict` ([`InterpretDeleteError`]). Deletion is
///   foreground ([Foreground cascading deletion]).
/// - A replacement waits while a build of the namespace has a Job on the daemon.
/// - The new `StatefulSet` is created only after the old one, its Pod and its cache claim are gone.
///   The `StatefulSet` controller reuses any claim of the same name that is not being deleted
///   ([`createPersistentVolumeClaims`]), so a new storage class would not take effect.
/// - Any other apply error, a `409 Conflict` included, is returned without a delete.
///
/// [Dry-run]: https://kubernetes.io/docs/reference/using-api/api-concepts/#dry-run
/// [`Preconditions`]: https://github.com/kubernetes/kubernetes/blob/v1.34.0/staging/src/k8s.io/apiserver/pkg/storage/interfaces.go#L137-L162
/// [`InterpretDeleteError`]: https://github.com/kubernetes/kubernetes/blob/v1.34.0/staging/src/k8s.io/apiserver/pkg/storage/errors/storage.go#L97-L112
/// [Foreground cascading deletion]: https://kubernetes.io/docs/concepts/architecture/garbage-collection/#foreground-deletion
/// [`createPersistentVolumeClaims`]: https://github.com/kubernetes/kubernetes/blob/v1.34.0/pkg/controller/statefulset/stateful_pod_control.go#L337-L366
async fn stateful_set(
    client: &Client,
    daemon_namespace: &str,
    namespace: &str,
    desired: &StatefulSet,
) -> Result<Applied, kube::Error> {
    let sets: Api<StatefulSet> = Api::namespaced(client.clone(), daemon_namespace);
    let name = desired.name_any();
    match sets.get_opt(&name).await? {
        Some(old) if old.metadata.deletion_timestamp.is_some() => {
            return Ok(Applied::Waiting(format!(
                "waiting for the old {name} to be deleted"
            )));
        }
        Some(_) => {}
        None => {
            if let Some(left) = leftover(client, daemon_namespace, namespace).await? {
                return Ok(Applied::Waiting(format!(
                    "waiting for the old {name}'s {left} to be deleted"
                )));
            }
        }
    }
    let refused = match apply(client, daemon_namespace, desired).await {
        Ok(set) => return Ok(Applied::Set(Box::new(set))),
        Err(kube::Error::Api(status)) if status.code == 422 => status.message,
        Err(e) => return Err(e),
    };

    let mut probe = desired.clone();
    probe.metadata.name = None;
    probe.metadata.generate_name = Some(format!("{name}-"));
    let dry_run = PostParams {
        dry_run: true,
        field_manager: Some(MANAGER.to_owned()),
    };
    match sets.create(&dry_run, &probe).await {
        Ok(_) => {}
        Err(kube::Error::Api(status)) if status.code == 422 => {
            return Ok(Applied::Invalid(format!(
                "the buildkitd StatefulSet is invalid: {}",
                status.message
            )));
        }
        Err(e) => return Err(e),
    }

    let building = building(client, namespace).await?;
    if building > 0 {
        return Ok(Applied::Waiting(format!(
            "waiting for {building} running build(s) to finish before {name} is replaced"
        )));
    }
    let Some(old) = sets.get_opt(&name).await? else {
        return Ok(Applied::Waiting(format!("{name} is being replaced")));
    };
    if old.metadata.deletion_timestamp.is_some() {
        return Ok(Applied::Waiting(format!(
            "waiting for the old {name} to be deleted"
        )));
    }
    let delete = DeleteParams {
        preconditions: Some(Preconditions {
            uid: old.metadata.uid.clone(),
            resource_version: old.metadata.resource_version.clone(),
        }),
        ..DeleteParams::foreground()
    };
    match sets.delete(&name, &delete).await {
        Ok(_) => {
            tracing::info!(%namespace, statefulset = %name, reason = %refused, "buildkitd replaced");
            Ok(Applied::Waiting(format!(
                "{name} is being replaced: {refused}"
            )))
        }
        Err(kube::Error::Api(status)) if status.code == 404 || status.code == 409 => Ok(
            Applied::Waiting(format!("{name} changed while being replaced")),
        ),
        Err(e) => Err(e),
    }
}

/// What is left of a deleted daemon of `namespace`: its Pod, or a cache claim that is being
/// deleted or still names a `StatefulSet` as its owner.
async fn leftover(
    client: &Client,
    daemon_namespace: &str,
    namespace: &str,
) -> Result<Option<&'static str>, kube::Error> {
    let pod = Api::<Pod>::namespaced(client.clone(), daemon_namespace)
        .get_opt(&daemon::pod(namespace))
        .await?;
    if pod.is_some() {
        return Ok(Some("Pod"));
    }
    let claim = Api::<PersistentVolumeClaim>::namespaced(client.clone(), daemon_namespace)
        .get_opt(&buildkitd::cache_claim(namespace))
        .await?;
    let old = claim.is_some_and(|c| {
        c.metadata.deletion_timestamp.is_some()
            || c.owner_references().iter().any(|o| o.kind == "StatefulSet")
    });
    Ok(old.then_some("cache volume"))
}

/// The runs of `namespace` whose Job uses the daemon: unfinished, with a Job.
async fn building(client: &Client, namespace: &str) -> Result<usize, kube::Error> {
    let runs: Api<BuildRun> = Api::namespaced(client.clone(), namespace);
    let mut params = ListParams::default().fields(decide::UNFINISHED).limit(PAGE);
    let mut count = 0;
    loop {
        let page = runs.list(&params).await?;
        count += page
            .items
            .iter()
            .filter(|r| r.status.as_ref().is_some_and(|s| s.job_ref.is_some()))
            .count();
        match page.metadata.continue_ {
            Some(token) if !token.is_empty() => params = params.continue_token(&token),
            _ => return Ok(count),
        }
    }
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
    let selector = format!(
        "{}={},app.kubernetes.io/name={}",
        labels::MANAGED_BY,
        labels::MANAGER,
        buildkitd::APP
    );
    let sets = sets.list(&ListParams::default().labels(&selector)).await?;
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
            Api::<StatefulSet>::namespaced(client.clone(), &settings.namespace)
                .patch_scale(
                    &set.name_any(),
                    &PatchParams::default(),
                    &Patch::Merge(serde_json::json!({"spec": {"replicas": 0}})),
                )
                .await?;
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
