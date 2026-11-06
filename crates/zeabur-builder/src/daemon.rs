//! The buildkitd of each build namespace: applied when a run starts, scaled to zero when the
//! namespace has been idle a while, and deleted when the namespace is gone.
//!
//! The objects are [`zeabur_builder_jobspec::buildkitd`]'s. The Service, `NetworkPolicy` and TLS
//! Secret are applied on every wake. The `StatefulSet` is created once and then only scaled,
//! through the `scale` subresource. One whose spec hash differs from the current one is deleted,
//! with its cache volume, once it is at zero replicas, by the sweep or by the next wake, and
//! created again from the current settings.
//!
//! The sweep and a wake may act on the same daemon at once. A deletion carries the UID and
//! `resourceVersion` it was decided on, so it fails once a wake has scaled the daemon or created a
//! new one. A wake creates no `StatefulSet` while the old one's cache volume is still there, which
//! its Pod would bind to.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{Namespace, PersistentVolumeClaim, Pod, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, Preconditions};
use kube::runtime::reflector::Store;
use kube::{Client, ResourceExt as _};
use zeabur_builder_api::build_run::BuildRun;
use zeabur_builder_api::labels;
use zeabur_builder_core::daemon;
use zeabur_builder_jobspec::Placement;
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
    /// The nodes the daemons and build Jobs run on.
    pub(crate) placement: Placement,
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
        placement: &settings.placement,
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
    let woken = ensure(
        client,
        &settings.namespace,
        namespace,
        &objects.stateful_set,
    )
    .await?;
    let pod = if woken {
        Api::<Pod>::namespaced(client.clone(), &settings.namespace)
            .get_opt(&pod_name)
            .await?
    } else {
        None
    };
    let ready = pod.is_some_and(|p| {
        p.metadata.deletion_timestamp.is_none()
            && p.status
                .and_then(|s| s.conditions)
                .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
    });
    Ok(Woken {
        pod: pod_name,
        address: daemon::address(namespace, &settings.namespace),
        ready,
        client: client_pems,
    })
}

/// What a wake does with the daemon's `StatefulSet`.
#[derive(Debug, PartialEq, Eq)]
enum Wake {
    /// None exists: create `want`.
    Create,
    /// Scale it to one replica, as it is.
    Scale,
    /// It is idle and its spec changed: delete it, and create it on a later wake.
    Replace,
    /// The old one, or its cache volume, is still being deleted.
    Wait,
}

fn wake_step(
    found: Option<&StatefulSet>,
    volume: Option<&PersistentVolumeClaim>,
    want: &StatefulSet,
) -> Wake {
    let Some(set) = found else {
        if volume.is_some_and(leaving) {
            return Wake::Wait;
        }
        return Wake::Create;
    };
    if set.metadata.deletion_timestamp.is_some() {
        return Wake::Wait;
    }
    if replicas(set) == 0 && buildkitd::spec_hash(set) != buildkitd::spec_hash(want) {
        return Wake::Replace;
    }
    Wake::Scale
}

/// What the sweep does with a namespace's daemon that still has its namespace.
#[derive(Debug, PartialEq, Eq)]
enum Sweep {
    Keep,
    ScaleToZero,
    /// At zero replicas with a changed spec: delete it, and the next wake creates it.
    Replace,
}

fn sweep_step(set: &StatefulSet, want: &StatefulSet, idle: bool) -> Sweep {
    if set.metadata.deletion_timestamp.is_some() {
        return Sweep::Keep;
    }
    if replicas(set) > 0 {
        return if idle {
            Sweep::ScaleToZero
        } else {
            Sweep::Keep
        };
    }
    if buildkitd::spec_hash(set) != buildkitd::spec_hash(want) {
        return Sweep::Replace;
    }
    Sweep::Keep
}

fn replicas(set: &StatefulSet) -> i32 {
    set.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0)
}

/// Whether a cache volume belongs to a deleted `StatefulSet`: it is being deleted, or still names
/// a `StatefulSet` as its owner, which the garbage collector has yet to act on. A volume left by
/// an orphaning delete names none and is used again.
fn leaving(volume: &PersistentVolumeClaim) -> bool {
    if volume.metadata.deletion_timestamp.is_some() {
        return true;
    }
    let owners = volume
        .metadata
        .owner_references
        .as_deref()
        .unwrap_or_default();
    for owner in owners {
        if owner.kind == "StatefulSet" {
            return true;
        }
    }
    false
}

/// The cache volume of a namespace's daemon: `<claim template>-<pod>`.
fn cache_volume(namespace: &str) -> String {
    format!("cache-{}", daemon::pod(namespace))
}

fn is_status(e: &kube::Error, codes: &[u16]) -> bool {
    matches!(e, kube::Error::Api(status) if codes.contains(&status.code))
}

/// Brings the daemon's `StatefulSet` to one replica; `false` when there is none to wake yet.
async fn ensure(
    client: &Client,
    daemon_namespace: &str,
    namespace: &str,
    want: &StatefulSet,
) -> Result<bool, kube::Error> {
    let sets: Api<StatefulSet> = Api::namespaced(client.clone(), daemon_namespace);
    let found = sets.get_opt(&want.name_any()).await?;
    let volume = if found.is_none() {
        Api::<PersistentVolumeClaim>::namespaced(client.clone(), daemon_namespace)
            .get_opt(&cache_volume(namespace))
            .await?
    } else {
        None
    };
    match wake_step(found.as_ref(), volume.as_ref(), want) {
        Wake::Create => {
            apply(client, daemon_namespace, want).await?;
            Ok(true)
        }
        Wake::Scale => {
            let scaled = sets
                .patch_scale(
                    &want.name_any(),
                    &PatchParams::default(),
                    &Patch::Merge(serde_json::json!({"spec": {"replicas": 1}})),
                )
                .await;
            match scaled {
                Ok(_) => Ok(true),
                Err(e) if is_status(&e, &[404]) => Ok(false),
                Err(e) => Err(e),
            }
        }
        Wake::Replace => {
            if let Some(set) = &found
                && replace(&sets, set).await?
            {
                tracing::info!(%namespace, statefulset = %set.name_any(), "spec changed; buildkitd deleted to be created again");
            }
            Ok(false)
        }
        Wake::Wait => Ok(false),
    }
}

/// Deletes `set`, and with it its cache volume, if it is still the object that was read: same UID
/// and `resourceVersion`. `false` when it changed or is gone.
async fn replace(sets: &Api<StatefulSet>, set: &StatefulSet) -> Result<bool, kube::Error> {
    let params = DeleteParams {
        preconditions: Some(Preconditions {
            uid: set.metadata.uid.clone(),
            resource_version: set.metadata.resource_version.clone(),
        }),
        ..DeleteParams::background()
    };
    match sets.delete(&set.name_any(), &params).await {
        Ok(_) => Ok(true),
        Err(e) if is_status(&e, &[404, 409]) => Ok(false),
        Err(e) => Err(e),
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

/// The annotations of all namespaces, by name, from one paged LIST of their metadata.
async fn namespaces(
    client: &Client,
) -> Result<BTreeMap<String, BTreeMap<String, String>>, kube::Error> {
    let api: Api<Namespace> = Api::all(client.clone());
    let mut names = BTreeMap::new();
    let mut params = ListParams::default().limit(PAGE);
    loop {
        let page = api.list_metadata(&params).await?;
        for item in page.items {
            if let Some(name) = item.metadata.name {
                names.insert(name, item.metadata.annotations.unwrap_or_default());
            }
        }
        match page.metadata.continue_ {
            Some(token) if !token.is_empty() => params = params.continue_token(&token),
            _ => return Ok(names),
        }
    }
}

/// One pass: scales idle daemons to zero, deletes those at zero whose spec changed, and removes
/// those whose namespace is gone. The runs come from the `BuildRun` reconciler's cache.
async fn sweep(
    client: &Client,
    settings: &Settings,
    max_concurrent: u32,
    runs: &Store<BuildRun>,
) -> Result<(), kube::Error> {
    let api: Api<StatefulSet> = Api::namespaced(client.clone(), &settings.namespace);
    let selector = format!(
        "{}={},app.kubernetes.io/name={}",
        labels::MANAGED_BY,
        labels::MANAGER,
        buildkitd::APP
    );
    let sets = api.list(&ListParams::default().labels(&selector)).await?;
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
        let Some(annotations) = existing.get(namespace) else {
            remove(client, settings, set).await?;
            tracing::info!(%namespace, statefulset = %set.name_any(), "namespace gone; buildkitd removed");
            continue;
        };
        let limits = decide::limits(annotations, max_concurrent, &settings.resources);
        let want = buildkitd::daemon(&inputs(settings, namespace, &limits)).stateful_set;
        let runs = runs_by_namespace
            .get(namespace)
            .map_or(&[][..], Vec::as_slice);
        let idle = decide::idle(runs.iter().map(AsRef::as_ref), now, settings.idle_after);
        match sweep_step(set, &want, idle) {
            Sweep::Keep => {}
            Sweep::ScaleToZero => {
                api.patch_scale(
                    &set.name_any(),
                    &PatchParams::default(),
                    &Patch::Merge(serde_json::json!({"spec": {"replicas": 0}})),
                )
                .await?;
                tracing::info!(%namespace, statefulset = %set.name_any(), "idle; buildkitd scaled to zero");
            }
            Sweep::Replace => {
                if replace(&api, set).await? {
                    tracing::info!(%namespace, statefulset = %set.name_any(), "spec changed; idle buildkitd deleted to be created again");
                }
            }
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
    max_concurrent: u32,
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
        if let Err(e) = Box::pin(sweep(&client, &settings, max_concurrent, &runs)).await {
            tracing::warn!(error = %e, "buildkitd sweep");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};

    use bytes::Bytes;
    use http_body_util::{BodyExt as _, Full};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
    use kube::client::Body;

    use super::*;

    const DAEMONS: &str = "builder-system";

    fn settings(placement: Placement) -> Settings {
        Settings {
            namespace: DAEMONS.to_owned(),
            image: "docker.io/moby/buildkit:v0.34.1".to_owned(),
            config_map: "buildkitd".to_owned(),
            cache_size: "20Gi".to_owned(),
            storage_class: None,
            runtime_class: None,
            resources: Resources::default(),
            idle_after: Duration::from_mins(10),
            placement,
        }
    }

    fn build_node() -> Placement {
        Placement {
            node_selector: BTreeMap::from([(
                "builder.zeabur.com/builds".to_owned(),
                "true".to_owned(),
            )]),
            tolerations: Vec::new(),
        }
    }

    /// The `StatefulSet` a wake in `team-a` would create.
    fn render(settings: &Settings) -> StatefulSet {
        let limits = decide::Limits {
            concurrent: 1,
            resources: Resources::default(),
        };
        buildkitd::daemon(&inputs(settings, "team-a", &limits)).stateful_set
    }

    fn scaled(set: &StatefulSet, replicas: i32) -> StatefulSet {
        let mut set = set.clone();
        if let Some(spec) = &mut set.spec {
            spec.replicas = Some(replicas);
        }
        set
    }

    fn owned_volume() -> PersistentVolumeClaim {
        let mut volume = PersistentVolumeClaim::default();
        volume.metadata.name = Some(cache_volume("team-a"));
        volume.metadata.owner_references = Some(vec![OwnerReference {
            api_version: "apps/v1".to_owned(),
            kind: "StatefulSet".to_owned(),
            name: daemon::name("team-a"),
            uid: "old".to_owned(),
            ..OwnerReference::default()
        }]);
        volume
    }

    #[test]
    fn the_sweep_replaces_a_changed_daemon_only_once_idle() {
        let old = render(&settings(Placement::default()));
        let new = render(&settings(build_node()));
        let running = scaled(&old, 1);
        assert_eq!(sweep_step(&running, &new, false), Sweep::Keep);
        assert_eq!(sweep_step(&running, &new, true), Sweep::ScaleToZero);
        assert_eq!(sweep_step(&scaled(&old, 0), &new, true), Sweep::Replace);
        assert_eq!(sweep_step(&scaled(&new, 0), &new, true), Sweep::Keep);

        let mut bigger = settings(Placement::default());
        bigger.cache_size = "40Gi".to_owned();
        assert_eq!(
            sweep_step(&scaled(&old, 0), &render(&bigger), true),
            Sweep::Replace
        );
    }

    #[test]
    fn a_wake_keeps_a_changed_daemon_that_is_running() {
        let old = render(&settings(Placement::default()));
        let new = render(&settings(build_node()));
        assert_eq!(wake_step(Some(&scaled(&old, 1)), None, &new), Wake::Scale);
        assert_eq!(wake_step(Some(&scaled(&old, 0)), None, &new), Wake::Replace);
        assert_eq!(wake_step(Some(&scaled(&new, 0)), None, &new), Wake::Scale);
        assert_eq!(wake_step(None, Some(&owned_volume()), &new), Wake::Wait);
        assert_eq!(
            wake_step(None, Some(&PersistentVolumeClaim::default()), &new),
            Wake::Create
        );
        assert_eq!(wake_step(None, None, &new), Wake::Create);
    }

    /// The `StatefulSet`s and cache volumes of [`DAEMONS`], with the API server's preconditions
    /// on delete. Deleting a `StatefulSet` leaves its volume, as the garbage collector does until
    /// it gets to it.
    #[derive(Debug, Default)]
    struct State {
        sets: BTreeMap<String, StatefulSet>,
        volumes: BTreeMap<String, PersistentVolumeClaim>,
        version: u64,
    }

    impl State {
        fn store(&mut self, mut set: StatefulSet) {
            self.version += 1;
            set.metadata.resource_version = Some(self.version.to_string());
            if set.metadata.uid.is_none() {
                set.metadata.uid = Some(format!("uid-{}", self.version));
            }
            self.sets.insert(set.name_any(), set);
        }

        fn handle(&mut self, method: &http::Method, path: &str, body: &[u8]) -> (u16, String) {
            let sets = format!("/apis/apps/v1/namespaces/{DAEMONS}/statefulsets/");
            let volumes = format!("/api/v1/namespaces/{DAEMONS}/persistentvolumeclaims/");
            if let Some(name) = path.strip_prefix(&volumes) {
                return self.volumes.get(name).map_or_else(not_found, |volume| {
                    (200, serde_json::to_string(volume).unwrap())
                });
            }
            let Some(rest) = path.strip_prefix(&sets) else {
                return not_found();
            };
            let scale = rest.ends_with("/scale");
            let name = rest.trim_end_matches("/scale").to_owned();
            let request: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
            match (method.as_str(), scale) {
                ("GET", false) => self
                    .sets
                    .get(&name)
                    .map_or_else(not_found, |set| (200, serde_json::to_string(set).unwrap())),
                ("PATCH", false) => {
                    let mut set: StatefulSet = serde_json::from_value(request).unwrap();
                    if let Some(found) = self.sets.get(&name) {
                        set.metadata.uid = found.metadata.uid.clone();
                    }
                    self.store(set);
                    (200, serde_json::to_string(&self.sets[&name]).unwrap())
                }
                ("PATCH", true) => {
                    let Some(found) = self.sets.get(&name) else {
                        return not_found();
                    };
                    let replicas = request["spec"]["replicas"].as_i64().unwrap();
                    let set = scaled(found, i32::try_from(replicas).unwrap());
                    self.store(set);
                    let scale = serde_json::json!({
                        "apiVersion": "autoscaling/v1",
                        "kind": "Scale",
                        "metadata": {"name": name, "namespace": DAEMONS},
                        "spec": {"replicas": replicas},
                    });
                    (200, scale.to_string())
                }
                ("DELETE", false) => {
                    let Some(found) = self.sets.get(&name) else {
                        return not_found();
                    };
                    let want = &request["preconditions"];
                    let uid = found.metadata.uid.as_deref();
                    let version = found.metadata.resource_version.as_deref();
                    if want["uid"].as_str().is_some_and(|u| Some(u) != uid)
                        || want["resourceVersion"]
                            .as_str()
                            .is_some_and(|v| Some(v) != version)
                    {
                        return status(409, "Conflict");
                    }
                    let set = self.sets.remove(&name).unwrap();
                    (200, serde_json::to_string(&set).unwrap())
                }
                _ => status(405, "MethodNotAllowed"),
            }
        }
    }

    fn not_found() -> (u16, String) {
        status(404, "NotFound")
    }

    fn status(code: u16, reason: &str) -> (u16, String) {
        let body = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Status",
            "status": "Failure",
            "message": reason,
            "reason": reason,
            "code": code,
        });
        (code, body.to_string())
    }

    #[derive(Debug, Clone, Default)]
    struct Fake(Arc<Mutex<State>>);

    impl tower_service::Service<http::Request<Body>> for Fake {
        type Response = http::Response<Full<Bytes>>;
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: http::Request<Body>) -> Self::Future {
            let state = Arc::clone(&self.0);
            Box::pin(async move {
                let (parts, body) = request.into_parts();
                let body = body.collect().await.unwrap().to_bytes();
                let (code, json) =
                    state
                        .lock()
                        .unwrap()
                        .handle(&parts.method, parts.uri.path(), &body);
                Ok(http::Response::builder()
                    .status(code)
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from(json)))
                    .unwrap())
            })
        }
    }

    impl Fake {
        fn client(&self) -> Client {
            Client::new(self.clone(), DAEMONS)
        }

        fn set(&self) -> Option<StatefulSet> {
            self.0
                .lock()
                .unwrap()
                .sets
                .get(&daemon::name("team-a"))
                .cloned()
        }

        /// The garbage collector deletes the volume of a deleted `StatefulSet`.
        fn collect_garbage(&self) {
            self.0.lock().unwrap().volumes.clear();
        }
    }

    /// A daemon at zero replicas, created from `settings`, with its cache volume.
    fn idle_daemon(settings: &Settings) -> Fake {
        let fake = Fake::default();
        let mut state = fake.0.lock().unwrap();
        state.store(scaled(&render(settings), 0));
        state.volumes.insert(cache_volume("team-a"), owned_volume());
        drop(state);
        fake
    }

    async fn wake_set(fake: &Fake, want: &StatefulSet) -> bool {
        ensure(&fake.client(), DAEMONS, "team-a", want)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_unchanged_idle_daemon_wakes_as_it_is() {
        let settings = settings(Placement::default());
        let fake = idle_daemon(&settings);
        let before = fake.set().unwrap();
        assert!(wake_set(&fake, &render(&settings)).await);
        let after = fake.set().unwrap();
        assert_eq!(after.metadata.uid, before.metadata.uid);
        assert_eq!(replicas(&after), 1);
    }

    #[tokio::test]
    async fn a_changed_idle_daemon_is_created_again_on_the_build_node() {
        let fake = idle_daemon(&settings(Placement::default()));
        let old = fake.set().unwrap();
        let want = render(&settings(build_node()));

        assert!(!wake_set(&fake, &want).await, "the old daemon is deleted");
        assert!(fake.set().is_none());
        assert!(
            !wake_set(&fake, &want).await,
            "no daemon while the old cache volume is there"
        );
        assert!(fake.set().is_none());

        fake.collect_garbage();
        assert!(wake_set(&fake, &want).await);
        let set = fake.set().unwrap();
        assert_ne!(set.metadata.uid, old.metadata.uid);
        assert_eq!(replicas(&set), 1);
        assert_eq!(buildkitd::spec_hash(&set), buildkitd::spec_hash(&want));
        let pod = set.spec.unwrap().template.spec.unwrap();
        assert_eq!(pod.node_selector, Some(build_node().node_selector));
    }

    #[tokio::test]
    async fn a_run_woken_while_the_sweep_replaces_its_daemon_still_gets_one() {
        let fake = idle_daemon(&settings(Placement::default()));
        let want = render(&settings(build_node()));
        let listed = fake.set().unwrap();
        let api: Api<StatefulSet> = Api::namespaced(fake.client(), DAEMONS);

        // The sweep deletes first; the wake waits for the volume, then creates.
        assert!(replace(&api, &listed).await.unwrap());
        assert!(!wake_set(&fake, &want).await);
        fake.collect_garbage();
        assert!(wake_set(&fake, &want).await);

        // A sweep still holding the old object leaves the new one alone.
        assert!(!replace(&api, &listed).await.unwrap());
        let set = fake.set().unwrap();
        assert_eq!(replicas(&set), 1);
        assert_eq!(buildkitd::spec_hash(&set), buildkitd::spec_hash(&want));
    }

    #[tokio::test]
    async fn the_sweep_leaves_a_daemon_that_a_wake_got_to_first() {
        let settings = settings(Placement::default());
        let fake = idle_daemon(&settings);
        let listed = fake.set().unwrap();
        let api: Api<StatefulSet> = Api::namespaced(fake.client(), DAEMONS);

        assert!(wake_set(&fake, &render(&settings)).await);
        assert!(!replace(&api, &listed).await.unwrap());
        assert_eq!(replicas(&fake.set().unwrap()), 1);
    }

    #[tokio::test]
    async fn a_sweep_that_finds_the_daemon_gone_is_not_an_error() {
        let fake = idle_daemon(&settings(Placement::default()));
        let want = render(&settings(build_node()));
        let listed = fake.set().unwrap();
        let api: Api<StatefulSet> = Api::namespaced(fake.client(), DAEMONS);

        assert!(!wake_set(&fake, &want).await);
        assert!(!replace(&api, &listed).await.unwrap());
    }
}
