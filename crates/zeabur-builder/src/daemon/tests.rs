use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use http::{Method, Request, Response};
use http_body_util::{BodyExt as _, Full};
use k8s_openapi::ByteString;
use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec, StatefulSetStatus};
use k8s_openapi::api::core::v1::{Namespace, Pod, PodCondition, PodStatus, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{
    DeleteOptions, ObjectMeta, OwnerReference, Status, Time,
};
use kube::{Client, ResourceExt as _};
use serde::Serialize;
use zeabur_builder_api::annotations;
use zeabur_builder_core::archive::sha256;
use zeabur_builder_jobspec::buildkitd::Resources;
use zeabur_builder_jobspec::{JobMemory, Placement};

use super::{Settings, Wake, ready};
use crate::decide;

const DAEMONS: &str = "builder-system";
const NAMESPACE: &str = "team-a";
const NAME: &str = "buildkitd-team-a";
const POD: &str = "buildkitd-team-a-0";

/// One request the fake API server answered.
#[derive(Debug, Clone)]
struct Call {
    method: Method,
    body: Bytes,
}

/// An object the fake API server stores.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
enum Object {
    Namespace(Box<Namespace>),
    StatefulSet(Box<StatefulSet>),
    Service(Box<Service>),
    NetworkPolicy(Box<NetworkPolicy>),
    Secret(Box<Secret>),
}

impl Object {
    fn parse(resource: &str, body: &[u8]) -> Option<Self> {
        Some(match resource {
            "namespaces" => Self::Namespace(Box::new(serde_json::from_slice(body).ok()?)),
            "statefulsets" => Self::StatefulSet(Box::new(serde_json::from_slice(body).ok()?)),
            "services" => Self::Service(Box::new(serde_json::from_slice(body).ok()?)),
            "networkpolicies" => Self::NetworkPolicy(Box::new(serde_json::from_slice(body).ok()?)),
            "secrets" => Self::Secret(Box::new(serde_json::from_slice(body).ok()?)),
            _ => return None,
        })
    }

    const fn meta(&self) -> &ObjectMeta {
        match self {
            Self::Namespace(o) => &o.metadata,
            Self::StatefulSet(o) => &o.metadata,
            Self::Service(o) => &o.metadata,
            Self::NetworkPolicy(o) => &o.metadata,
            Self::Secret(o) => &o.metadata,
        }
    }

    const fn meta_mut(&mut self) -> &mut ObjectMeta {
        match self {
            Self::Namespace(o) => &mut o.metadata,
            Self::StatefulSet(o) => &mut o.metadata,
            Self::Service(o) => &mut o.metadata,
            Self::NetworkPolicy(o) => &mut o.metadata,
            Self::Secret(o) => &mut o.metadata,
        }
    }
}

/// An in-memory API server for the objects [`wake`](super::wake) reads and writes. It keeps `uid`,
/// `resourceVersion` and delete preconditions as kube-apiserver does, refuses an update to
/// `volumeClaimTemplates` with 422, and deletes in the foreground: the object stays with a
/// `deletionTimestamp` until [`Cluster::collect`].
#[derive(Debug, Default)]
struct Cluster {
    /// By `<resource>/<name>`.
    objects: BTreeMap<String, Object>,
    serial: u64,
    /// The answer to every apply of a `StatefulSet` instead of the normal handling.
    apply: Option<(u16, &'static str, &'static str)>,
    /// Another wake scales the `StatefulSet` up just before the next delete reaches it.
    scale_up_before_delete: bool,
    calls: Vec<Call>,
}

type Shared = Arc<Mutex<Cluster>>;

fn lock(cluster: &Shared) -> std::sync::MutexGuard<'_, Cluster> {
    cluster.lock().unwrap_or_else(PoisonError::into_inner)
}

fn object(cluster: &Shared, key: &str) -> Object {
    lock(cluster)
        .objects
        .get(key)
        .cloned()
        .unwrap_or_else(|| panic!("no {key}"))
}

fn stored_set(cluster: &Shared) -> StatefulSet {
    let Object::StatefulSet(set) = object(cluster, &set_key()) else {
        panic!("{} is not a StatefulSet", set_key());
    };
    *set
}

fn status(code: u16, reason: &str, message: &str) -> Box<Status> {
    Box::new(Status {
        code: Some(code.into()),
        message: Some(message.to_owned()),
        reason: Some(reason.to_owned()),
        status: Some("Failure".to_owned()),
        ..Status::default()
    })
}

type Answer = Result<(u16, Object), Box<Status>>;

impl Cluster {
    fn next(&mut self) -> String {
        self.serial += 1;
        self.serial.to_string()
    }

    fn put(&mut self, key: &str, mut object: Object) -> Object {
        let rv = self.next();
        object.meta_mut().resource_version = Some(rv);
        if object.meta().uid.is_none() {
            object.meta_mut().uid = Some(format!("uid-{}", self.next()));
        }
        self.objects.insert(key.to_owned(), object.clone());
        object
    }

    fn uid(&self, key: &str) -> Option<String> {
        self.objects.get(key).and_then(|o| o.meta().uid.clone())
    }

    /// The garbage collector finishes deleting `key`.
    fn collect(&mut self, key: &str) {
        self.objects.remove(key);
    }

    fn answer(&mut self, method: &Method, path: &str, body: &[u8]) -> Answer {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let (resource, name) = match parts.as_slice() {
            [.., "namespaces", _, resource, name] => (*resource, Some(*name)),
            [.., "namespaces", _, resource] => (*resource, None),
            ["api", "v1", "namespaces", name] => ("namespaces", Some(*name)),
            _ => return Err(status(404, "NotFound", "unknown path")),
        };
        let key = name.map(|n| format!("{resource}/{n}"));
        let object =
            || Object::parse(resource, body).ok_or_else(|| status(400, "BadRequest", "unreadable"));
        match (method.as_str(), key) {
            ("GET", Some(key)) => self
                .objects
                .get(&key)
                .map(|object| (200, object.clone()))
                .ok_or_else(|| status(404, "NotFound", "not found")),
            ("PATCH", Some(key)) => self.apply(&key, object()?),
            ("POST", None) => {
                let object = object()?;
                let key = format!(
                    "{resource}/{}",
                    object.meta().name.as_deref().unwrap_or_default()
                );
                if self.objects.contains_key(&key) {
                    return Err(status(409, "AlreadyExists", "already exists"));
                }
                Ok((201, self.put(&key, object)))
            }
            ("PUT", Some(key)) => Ok((200, self.put(&key, object()?))),
            ("DELETE", Some(key)) => {
                let options = serde_json::from_slice(body)
                    .map_err(|e| status(400, "BadRequest", &e.to_string()))?;
                self.delete(&key, &options)
            }
            _ => Err(status(405, "MethodNotAllowed", "not handled by the fake")),
        }
    }

    fn apply(&mut self, key: &str, body: Object) -> Answer {
        if let (Some((code, reason, message)), true) =
            (self.apply, key.starts_with("statefulsets/"))
        {
            return Err(status(code, reason, message));
        }
        let Some(old) = self.objects.get(key).cloned() else {
            return Ok((201, self.put(key, body)));
        };
        if let (Object::StatefulSet(old), Object::StatefulSet(new)) = (&old, &body)
            && claims(old) != claims(new)
        {
            return Err(status(
                422,
                "Invalid",
                "StatefulSet.apps \"buildkitd-team-a\" is invalid: spec: Forbidden: \
                 updates to statefulset spec for fields other than ... are forbidden",
            ));
        }
        let mut updated = body;
        updated.meta_mut().uid.clone_from(&old.meta().uid);
        updated
            .meta_mut()
            .deletion_timestamp
            .clone_from(&old.meta().deletion_timestamp);
        Ok((200, self.put(key, updated)))
    }

    fn delete(&mut self, key: &str, options: &DeleteOptions) -> Answer {
        if std::mem::take(&mut self.scale_up_before_delete)
            && let Some(Object::StatefulSet(mut set)) = self.objects.get(key).cloned()
        {
            set.spec.get_or_insert_default().replicas = Some(1);
            self.put(key, Object::StatefulSet(set));
        }
        let Some(mut deleting) = self.objects.get(key).cloned() else {
            return Err(status(404, "NotFound", "not found"));
        };
        if let Some(want) = &options.preconditions {
            let meta = deleting.meta();
            if want.uid.is_some() && want.uid != meta.uid
                || want.resource_version.is_some() && want.resource_version != meta.resource_version
            {
                return Err(status(409, "Conflict", "Precondition failed"));
            }
        }
        let meta = deleting.meta_mut();
        meta.deletion_timestamp = Some(deleted_at());
        meta.finalizers = Some(vec!["foregroundDeletion".to_owned()]);
        Ok((200, self.put(key, deleting)))
    }
}

fn claims(set: &StatefulSet) -> Option<&Vec<k8s_openapi::api::core::v1::PersistentVolumeClaim>> {
    set.spec.as_ref()?.volume_claim_templates.as_ref()
}

fn client(cluster: &Shared) -> Client {
    let cluster = Arc::clone(cluster);
    let service = tower::service_fn(move |request: Request<kube::client::Body>| {
        let cluster = Arc::clone(&cluster);
        async move {
            let method = request.method().clone();
            let path = request.uri().path().to_owned();
            let body = request.into_body().collect().await.unwrap().to_bytes();
            let answer = {
                let mut cluster = lock(&cluster);
                cluster.calls.push(Call {
                    method: method.clone(),
                    body: body.clone(),
                });
                cluster.answer(&method, &path, &body)
            };
            let (code, answer) = match answer {
                Ok((code, object)) => (code, serde_json::to_vec(&object)),
                Err(status) => (
                    status
                        .code
                        .and_then(|c| u16::try_from(c).ok())
                        .unwrap_or(500),
                    serde_json::to_vec(&status),
                ),
            };
            let response = Response::builder()
                .status(code)
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(answer.unwrap())))
                .unwrap_or_default();
            Ok::<_, Infallible>(response)
        }
    });
    Client::new(service, "default")
}

fn deleted_at() -> Time {
    Time("2026-11-11T00:00:00Z".parse().unwrap())
}

fn settings() -> Settings {
    Settings {
        namespace: DAEMONS.to_owned(),
        image: "moby/buildkit".to_owned(),
        config_map: "buildkitd".to_owned(),
        cache_size: "20Gi".to_owned(),
        storage_class: None,
        runtime_class: None,
        resources: Resources::default(),
        idle_after: Duration::from_mins(10),
        placement: Placement::default(),
    }
}

fn limits() -> decide::Limits {
    decide::Limits {
        concurrent: 1,
        resources: Resources::default(),
        job_memory: JobMemory {
            request: "128Mi".to_owned(),
            limit: "512Mi".to_owned(),
        },
        clamped: Vec::new(),
    }
}

fn meta(name: &str) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_owned()),
        ..ObjectMeta::default()
    }
}

/// A cluster with the namespace `team-a` of UID `uid`.
fn with_namespace(uid: &str) -> Shared {
    let mut cluster = Cluster::default();
    cluster.put(
        &format!("namespaces/{NAMESPACE}"),
        Object::Namespace(Box::new(Namespace {
            metadata: ObjectMeta {
                uid: Some(uid.to_owned()),
                ..meta(NAMESPACE)
            },
            ..Namespace::default()
        })),
    );
    Arc::new(Mutex::new(cluster))
}

fn namespace_owner(uid: &str) -> Vec<OwnerReference> {
    vec![OwnerReference {
        api_version: "v1".to_owned(),
        kind: "Namespace".to_owned(),
        name: NAMESPACE.to_owned(),
        uid: uid.to_owned(),
        block_owner_deletion: Some(false),
        controller: None,
    }]
}

fn writes(cluster: &Shared) -> Vec<Call> {
    lock(cluster)
        .calls
        .iter()
        .filter(|c| c.method != Method::GET)
        .cloned()
        .collect()
}

async fn wake_namespace(cluster: &Shared) -> Result<Wake, kube::Error> {
    super::wake(&client(cluster), &settings(), NAMESPACE, &limits(), None).await
}

#[tokio::test]
async fn the_namespace_owns_every_object_of_its_daemon() {
    let cluster = with_namespace("ns-1");

    let woken = wake_namespace(&cluster).await;

    assert!(matches!(woken, Ok(Wake::Starting(_))), "{woken:?}");
    for key in [
        set_key(),
        format!("services/{NAME}"),
        format!("networkpolicies/{NAME}"),
        format!("secrets/{NAME}-tls"),
    ] {
        let stored = object(&cluster, &key);
        assert_eq!(
            stored.meta().owner_references,
            Some(namespace_owner("ns-1")),
            "{key}"
        );
    }
}

#[tokio::test]
async fn a_secret_made_before_owners_gets_the_namespace_as_owner() {
    let cluster = with_namespace("ns-1");
    let data = [
        ("ca.pem", "ca"),
        ("client-cert.pem", "cert"),
        ("client-key.pem", "key"),
    ]
    .map(|(k, v)| (k.to_owned(), ByteString(v.into())));
    lock(&cluster).put(
        &format!("secrets/{NAME}-tls"),
        Object::Secret(Box::new(Secret {
            metadata: meta(&format!("{NAME}-tls")),
            data: Some(data.into()),
            ..Secret::default()
        })),
    );

    let woken = wake_namespace(&cluster).await;

    assert!(matches!(woken, Ok(Wake::Starting(_))), "{woken:?}");
    let Object::Secret(secret) = object(&cluster, &format!("secrets/{NAME}-tls")) else {
        panic!("not a Secret");
    };
    assert_eq!(
        secret.metadata.owner_references,
        Some(namespace_owner("ns-1"))
    );
    assert_eq!(
        secret.data.unwrap_or_default().get("ca.pem"),
        Some(&ByteString(b"ca".into())),
        "the CA stays"
    );
}

#[tokio::test]
async fn a_daemon_of_a_deleted_namespace_with_the_same_name_waits_for_garbage_collection() {
    let cluster = with_namespace("ns-2");
    let owned = |name: String| ObjectMeta {
        owner_references: Some(namespace_owner("ns-1")),
        ..meta(&name)
    };
    lock(&cluster).put(
        &set_key(),
        Object::StatefulSet(Box::new(StatefulSet {
            metadata: owned(NAME.to_owned()),
            ..StatefulSet::default()
        })),
    );
    lock(&cluster).put(
        &format!("secrets/{NAME}-tls"),
        Object::Secret(Box::new(Secret {
            metadata: owned(format!("{NAME}-tls")),
            ..Secret::default()
        })),
    );

    for gone in [set_key(), format!("secrets/{NAME}-tls")] {
        let woken = wake_namespace(&cluster).await;
        assert!(
            matches!(&woken, Ok(Wake::Starting(m)) if m.contains("garbage collection")),
            "{woken:?}"
        );
        assert!(writes(&cluster).is_empty(), "{:?}", writes(&cluster));
        lock(&cluster).collect(&gone);
    }

    let woken = wake_namespace(&cluster).await;
    assert!(matches!(woken, Ok(Wake::Starting(_))), "{woken:?}");
    for key in [set_key(), format!("secrets/{NAME}-tls")] {
        let stored = object(&cluster, &key);
        assert_eq!(
            stored.meta().owner_references,
            Some(namespace_owner("ns-2")),
            "{key}"
        );
    }
}

fn set_key() -> String {
    format!("statefulsets/{NAME}")
}

fn deletes(cluster: &Shared) -> Vec<Call> {
    writes(cluster)
        .into_iter()
        .filter(|c| c.method == Method::DELETE)
        .collect()
}

/// A cluster where `team-a` has its daemon at `replicas`, made with other settings when `changed`.
async fn daemon_at(replicas: i32, changed: bool) -> Shared {
    let cluster = with_namespace("ns-1");
    let woken = wake_namespace(&cluster).await;
    assert!(matches!(woken, Ok(Wake::Starting(_))), "{woken:?}");
    let mut set = stored_set(&cluster);
    let spec = set.spec.get_or_insert_default();
    spec.replicas = Some(replicas);
    if changed {
        let claim = spec
            .volume_claim_templates
            .get_or_insert_default()
            .first_mut()
            .expect("a cache claim");
        claim.spec.get_or_insert_default().storage_class_name = Some("old".to_owned());
        set.annotations_mut()
            .insert("builder.zeabur.com/spec-hash".to_owned(), "old".to_owned());
    }
    let mut cluster_lock = lock(&cluster);
    cluster_lock.put(&set_key(), Object::StatefulSet(Box::new(set)));
    cluster_lock.calls.clear();
    drop(cluster_lock);
    cluster
}

#[tokio::test]
async fn a_running_daemon_keeps_its_old_settings() {
    let cluster = daemon_at(1, true).await;
    let before = object(&cluster, &set_key());

    let woken = wake_namespace(&cluster).await;

    assert!(matches!(woken, Ok(Wake::Starting(_))), "{woken:?}");
    assert_eq!(object(&cluster, &set_key()), before);
    assert!(deletes(&cluster).is_empty());
}

#[tokio::test]
async fn a_running_daemon_restarts_with_a_new_registry_ca_and_keeps_its_cache() {
    let cluster = daemon_at(1, false).await;
    let ca_of = |cluster: &Shared| {
        stored_set(cluster)
            .spec
            .and_then(|s| s.template.metadata)
            .and_then(|m| m.annotations)
            .and_then(|a| a.get(annotations::REGISTRY_CA_HASH).cloned())
    };
    assert_eq!(ca_of(&cluster), None);

    for ca in ["first CA", "second CA"] {
        let woken = super::wake(
            &client(&cluster),
            &settings(),
            NAMESPACE,
            &limits(),
            Some(ca),
        )
        .await;
        assert!(matches!(woken, Ok(Wake::Starting(_))), "{woken:?}");
        assert_eq!(ca_of(&cluster), Some(sha256(ca.as_bytes())));
    }
    assert!(deletes(&cluster).is_empty());
}

#[tokio::test]
async fn a_running_daemon_without_the_registry_ca_volume_is_replaced() {
    let cluster = daemon_at(1, true).await;
    let mut set = stored_set(&cluster);
    if let Some(pod) = set.spec.as_mut().and_then(|s| s.template.spec.as_mut()) {
        pod.volumes
            .get_or_insert_default()
            .retain(|v| v.name != "registry-ca");
    }
    lock(&cluster).put(&set_key(), Object::StatefulSet(Box::new(set)));

    let woken = super::wake(&client(&cluster), &settings(), NAMESPACE, &limits(), None).await;
    assert!(matches!(woken, Ok(Wake::Starting(_))), "{woken:?}");
    assert!(deletes(&cluster).is_empty(), "no CA: it keeps running");

    let woken = super::wake(
        &client(&cluster),
        &settings(),
        NAMESPACE,
        &limits(),
        Some("CA"),
    )
    .await;
    assert!(
        matches!(&woken, Ok(Wake::Starting(m)) if m.contains("new settings")),
        "{woken:?}"
    );
    assert_eq!(deletes(&cluster).len(), 1);
}

#[tokio::test]
async fn a_daemon_scaled_to_zero_with_old_settings_is_replaced() {
    let cluster = daemon_at(0, true).await;
    let old = lock(&cluster).uid(&set_key());

    let woken = wake_namespace(&cluster).await;
    assert!(
        matches!(&woken, Ok(Wake::Starting(m)) if m.contains("new settings")),
        "{woken:?}"
    );
    let [delete] = deletes(&cluster)
        .try_into()
        .unwrap_or_else(|d| panic!("{d:?}"));
    let options: DeleteOptions = serde_json::from_slice(&delete.body).unwrap();
    assert_eq!(options.preconditions.and_then(|p| p.uid), old);
    assert_eq!(options.propagation_policy.as_deref(), Some("Foreground"));

    let woken = wake_namespace(&cluster).await;
    assert!(
        matches!(&woken, Ok(Wake::Starting(m)) if m.contains("to be deleted")),
        "{woken:?}"
    );
    assert_eq!(deletes(&cluster).len(), 1, "no second delete");

    lock(&cluster).collect(&set_key());
    let woken = wake_namespace(&cluster).await;
    assert!(matches!(woken, Ok(Wake::Starting(_))), "{woken:?}");
    let new = stored_set(&cluster);
    assert_ne!(new.metadata.uid, old);
    let spec = new.spec.unwrap_or_default();
    assert_eq!(spec.replicas, Some(1));
    assert!(
        spec.volume_claim_templates
            .unwrap_or_default()
            .iter()
            .all(|c| c
                .spec
                .as_ref()
                .is_none_or(|s| s.storage_class_name.is_none()))
    );
}

#[tokio::test]
async fn a_daemon_scaled_up_while_being_replaced_keeps_running() {
    let cluster = daemon_at(0, true).await;
    lock(&cluster).scale_up_before_delete = true;

    let woken = wake_namespace(&cluster).await;

    assert!(matches!(woken, Ok(Wake::Starting(_))), "{woken:?}");
    let set = stored_set(&cluster);
    assert!(set.metadata.deletion_timestamp.is_none(), "{set:?}");
    assert_eq!(set.spec.and_then(|s| s.replicas), Some(1));
}

#[tokio::test]
async fn a_daemon_scaled_to_zero_with_the_same_settings_scales_up() {
    let cluster = daemon_at(0, false).await;
    let old = lock(&cluster).uid(&set_key());

    let woken = wake_namespace(&cluster).await;

    assert!(matches!(woken, Ok(Wake::Starting(_))), "{woken:?}");
    assert!(deletes(&cluster).is_empty());
    assert_eq!(lock(&cluster).uid(&set_key()), old);
    assert_eq!(stored_set(&cluster).spec.and_then(|s| s.replicas), Some(1));
}

#[tokio::test]
async fn an_apply_conflict_waits() {
    let cluster = daemon_at(0, false).await;
    lock(&cluster).apply = Some((409, "Conflict", "Operation cannot be fulfilled"));

    let woken = wake_namespace(&cluster).await;

    assert!(
        matches!(&woken, Ok(Wake::Starting(m)) if m.contains("Operation cannot be fulfilled")),
        "{woken:?}"
    );
    assert!(deletes(&cluster).is_empty());
}

#[test]
fn only_a_ready_pod_of_the_current_statefulset_is_ready() {
    let set = StatefulSet {
        metadata: ObjectMeta {
            uid: Some("new".to_owned()),
            generation: Some(2),
            ..meta(NAME)
        },
        spec: Some(StatefulSetSpec::default()),
        status: Some(StatefulSetStatus {
            observed_generation: Some(2),
            update_revision: Some("r2".to_owned()),
            ..StatefulSetStatus::default()
        }),
    };
    let pod = |owner: &str, deleting: bool, ready: &str, revision: &str| Pod {
        metadata: ObjectMeta {
            labels: Some([("controller-revision-hash".to_owned(), revision.to_owned())].into()),
            owner_references: Some(vec![OwnerReference {
                api_version: "apps/v1".to_owned(),
                kind: "StatefulSet".to_owned(),
                name: NAME.to_owned(),
                uid: owner.to_owned(),
                controller: Some(true),
                block_owner_deletion: None,
            }]),
            deletion_timestamp: deleting.then(deleted_at),
            ..meta(POD)
        },
        status: Some(PodStatus {
            conditions: Some(vec![PodCondition {
                type_: "Ready".to_owned(),
                status: ready.to_owned(),
                ..PodCondition::default()
            }]),
            ..PodStatus::default()
        }),
        ..Pod::default()
    };
    let cases = [
        ("ready, current set", pod("new", false, "True", "r2"), true),
        ("ready, deleted set", pod("old", false, "True", "r2"), false),
        ("ready, terminating", pod("new", true, "True", "r2"), false),
        ("not ready", pod("new", false, "False", "r2"), false),
        (
            "ready, old revision",
            pod("new", false, "True", "r1"),
            false,
        ),
    ];
    for (case, pod, want) in cases {
        assert_eq!(ready(&pod, &set), want, "{case}");
    }
    let mut unseen = set;
    unseen.metadata.generation = Some(3);
    assert!(
        !ready(&pod("new", false, "True", "r2"), &unseen),
        "the update is not observed yet"
    );
}
