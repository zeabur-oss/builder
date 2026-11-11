use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use http::{Method, Request, Response};
use http_body_util::{BodyExt as _, Full};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use serde_json::{Value, json};
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
    body: Value,
}

/// An in-memory API server for the objects [`wake`](super::wake) reads and writes. It keeps `uid`,
/// `resourceVersion` and delete preconditions as kube-apiserver does, refuses an update to
/// `volumeClaimTemplates` with 422, and deletes in the foreground: the object stays with a
/// `deletionTimestamp` until [`Cluster::collect`].
#[derive(Debug, Default)]
struct Cluster {
    /// By `<resource>/<name>`.
    objects: BTreeMap<String, Value>,
    serial: u64,
    /// The answer to every apply of a `StatefulSet` instead of the normal handling.
    apply: Option<(u16, &'static str, &'static str)>,
    calls: Vec<Call>,
}

type Shared = Arc<Mutex<Cluster>>;

fn lock(cluster: &Shared) -> std::sync::MutexGuard<'_, Cluster> {
    cluster.lock().unwrap_or_else(PoisonError::into_inner)
}

fn object(cluster: &Shared, key: &str) -> Value {
    lock(cluster).objects.get(key).cloned().unwrap_or_default()
}

fn status(code: u16, reason: &str, message: &str) -> (u16, Value) {
    let body = json!({
        "kind": "Status", "apiVersion": "v1", "metadata": {},
        "status": "Failure", "reason": reason, "message": message, "code": code,
    });
    (code, body)
}

impl Cluster {
    fn next(&mut self) -> String {
        self.serial += 1;
        self.serial.to_string()
    }

    fn put(&mut self, key: &str, mut object: Value) -> Value {
        let rv = self.next();
        object["metadata"]["resourceVersion"] = json!(rv);
        if object["metadata"]["uid"].is_null() {
            object["metadata"]["uid"] = json!(format!("uid-{}", self.next()));
        }
        self.objects.insert(key.to_owned(), object.clone());
        object
    }

    fn uid(&self, key: &str) -> Option<String> {
        self.objects
            .get(key)
            .and_then(|o| o["metadata"]["uid"].as_str())
            .map(str::to_owned)
    }

    /// The garbage collector finishes deleting `key`.
    fn collect(&mut self, key: &str) {
        self.objects.remove(key);
    }

    fn answer(&mut self, method: &Method, path: &str, body: Value) -> (u16, Value) {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let (resource, name) = match parts.as_slice() {
            [.., "namespaces", _, resource, name] => (*resource, Some(*name)),
            [.., "namespaces", _, resource] => (*resource, None),
            ["api", "v1", "namespaces", name] => ("namespaces", Some(*name)),
            _ => return status(404, "NotFound", "unknown path"),
        };
        let key = name.map(|n| format!("{resource}/{n}"));
        match (method.as_str(), key) {
            ("GET", Some(key)) => self.objects.get(&key).map_or_else(
                || status(404, "NotFound", "not found"),
                |object| (200, object.clone()),
            ),
            ("PATCH", Some(key)) => self.apply(&key, body),
            ("POST", None) => {
                let key = format!(
                    "{resource}/{}",
                    body["metadata"]["name"].as_str().unwrap_or("")
                );
                if self.objects.contains_key(&key) {
                    return status(409, "AlreadyExists", "already exists");
                }
                (201, self.put(&key, body))
            }
            ("PUT", Some(key)) => (200, self.put(&key, body)),
            ("DELETE", Some(key)) => self.delete(&key, &body),
            _ => status(405, "MethodNotAllowed", "not handled by the fake"),
        }
    }

    fn apply(&mut self, key: &str, body: Value) -> (u16, Value) {
        if let (Some((code, reason, message)), true) =
            (self.apply, key.starts_with("statefulsets/"))
        {
            return status(code, reason, message);
        }
        match self.objects.get(key).cloned() {
            None => {
                let created = self.put(key, body);
                (201, created)
            }
            Some(old) => {
                if old["spec"]["volumeClaimTemplates"] != body["spec"]["volumeClaimTemplates"] {
                    return status(
                        422,
                        "Invalid",
                        "StatefulSet.apps \"buildkitd-team-a\" is invalid: spec: Forbidden: \
                         updates to statefulset spec for fields other than ... are forbidden",
                    );
                }
                let mut updated = body;
                updated["metadata"]["uid"] = old["metadata"]["uid"].clone();
                updated["metadata"]["deletionTimestamp"] =
                    old["metadata"]["deletionTimestamp"].clone();
                let updated = self.put(key, updated);
                (200, updated)
            }
        }
    }

    fn delete(&mut self, key: &str, body: &Value) -> (u16, Value) {
        let Some(object) = self.objects.get(key).cloned() else {
            return status(404, "NotFound", "not found");
        };
        let want = &body["preconditions"];
        for field in ["uid", "resourceVersion"] {
            if !want[field].is_null() && want[field] != object["metadata"][field] {
                return status(409, "Conflict", "Precondition failed");
            }
        }
        let mut deleting = object;
        deleting["metadata"]["deletionTimestamp"] = json!("2026-11-11T00:00:00Z");
        deleting["metadata"]["finalizers"] = json!(["foregroundDeletion"]);
        let deleting = self.put(key, deleting);
        (200, deleting)
    }
}

fn client(cluster: &Shared) -> Client {
    let cluster = Arc::clone(cluster);
    let service = tower::service_fn(move |request: Request<kube::client::Body>| {
        let cluster = Arc::clone(&cluster);
        async move {
            let method = request.method().clone();
            let path = request.uri().path().to_owned();
            let bytes = request.into_body().collect().await.unwrap().to_bytes();
            let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let (code, answer) = {
                let mut cluster = lock(&cluster);
                cluster.calls.push(Call {
                    method: method.clone(),
                    body: body.clone(),
                });
                cluster.answer(&method, &path, body)
            };
            let response = Response::builder()
                .status(code)
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(answer.to_string())))
                .unwrap_or_default();
            Ok::<_, Infallible>(response)
        }
    });
    Client::new(service, "default")
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

/// A cluster with the namespace `team-a` of UID `uid`.
fn with_namespace(uid: &str) -> Shared {
    let mut cluster = Cluster::default();
    cluster.put(
        &format!("namespaces/{NAMESPACE}"),
        json!({"metadata": {"name": NAMESPACE, "uid": uid}}),
    );
    Arc::new(Mutex::new(cluster))
}

fn namespace_owner(uid: &str) -> Value {
    json!([{
        "apiVersion": "v1", "kind": "Namespace", "name": NAMESPACE, "uid": uid,
        "blockOwnerDeletion": false,
    }])
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
    super::wake(&client(cluster), &settings(), NAMESPACE, &limits()).await
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
            stored["metadata"]["ownerReferences"],
            namespace_owner("ns-1"),
            "{key}"
        );
    }
}

#[tokio::test]
async fn a_secret_made_before_owners_gets_the_namespace_as_owner() {
    let cluster = with_namespace("ns-1");
    lock(&cluster).put(
        &format!("secrets/{NAME}-tls"),
        json!({"metadata": {"name": format!("{NAME}-tls")}, "data": {
            "ca.pem": "Y2E=", "client-cert.pem": "Y2VydA==", "client-key.pem": "a2V5",
        }}),
    );

    let woken = wake_namespace(&cluster).await;

    assert!(matches!(woken, Ok(Wake::Starting(_))), "{woken:?}");
    let secret = object(&cluster, &format!("secrets/{NAME}-tls"));
    assert_eq!(
        secret["metadata"]["ownerReferences"],
        namespace_owner("ns-1")
    );
    assert_eq!(secret["data"]["ca.pem"], "Y2E=", "the CA stays");
}

#[tokio::test]
async fn a_daemon_of_a_deleted_namespace_with_the_same_name_waits_for_garbage_collection() {
    let cluster = with_namespace("ns-2");
    for key in [set_key(), format!("secrets/{NAME}-tls")] {
        lock(&cluster).put(
            &key,
            json!({"metadata": {"name": key.split('/').nth(1),
                                "ownerReferences": namespace_owner("ns-1")}}),
        );
    }

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
            stored["metadata"]["ownerReferences"],
            namespace_owner("ns-2"),
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
    {
        let mut cluster = lock(&cluster);
        let mut set = cluster.objects.get(&set_key()).cloned().unwrap_or_default();
        set["spec"]["replicas"] = json!(replicas);
        if changed {
            set["metadata"]["annotations"]["builder.zeabur.com/spec-hash"] = json!("old");
            set["spec"]["volumeClaimTemplates"][0]["spec"]["storageClassName"] = json!("old");
        }
        cluster.put(&set_key(), set);
        cluster.calls.clear();
    }
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
    assert_eq!(delete.body["preconditions"]["uid"].as_str(), old.as_deref());
    assert_eq!(delete.body["propagationPolicy"], "Foreground");

    let woken = wake_namespace(&cluster).await;
    assert!(
        matches!(&woken, Ok(Wake::Starting(m)) if m.contains("to be deleted")),
        "{woken:?}"
    );
    assert_eq!(deletes(&cluster).len(), 1, "no second delete");

    lock(&cluster).collect(&set_key());
    let woken = wake_namespace(&cluster).await;
    assert!(matches!(woken, Ok(Wake::Starting(_))), "{woken:?}");
    let new = object(&cluster, &set_key());
    assert_ne!(new["metadata"]["uid"].as_str(), old.as_deref());
    assert_eq!(new["spec"]["replicas"], 1);
    assert!(new["spec"]["volumeClaimTemplates"][0]["spec"]["storageClassName"].is_null());
}

#[tokio::test]
async fn a_daemon_scaled_to_zero_with_the_same_settings_scales_up() {
    let cluster = daemon_at(0, false).await;
    let old = lock(&cluster).uid(&set_key());

    let woken = wake_namespace(&cluster).await;

    assert!(matches!(woken, Ok(Wake::Starting(_))), "{woken:?}");
    assert!(deletes(&cluster).is_empty());
    assert_eq!(lock(&cluster).uid(&set_key()), old);
    assert_eq!(object(&cluster, &set_key())["spec"]["replicas"], 1);
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
    let set: StatefulSet = serde_json::from_value(json!({
        "metadata": {"name": NAME, "uid": "new", "generation": 2},
        "spec": {"selector": {}, "template": {}},
        "status": {"observedGeneration": 2, "updateRevision": "r2"},
    }))
    .unwrap_or_default();
    let pod = |owner: &str, deleting: bool, ready: &str, revision: &str| -> Pod {
        let mut pod = json!({
            "metadata": {"name": POD, "labels": {"controller-revision-hash": revision},
                         "ownerReferences": [{
                "apiVersion": "apps/v1", "kind": "StatefulSet", "name": NAME,
                "uid": owner, "controller": true,
            }]},
            "status": {"conditions": [{"type": "Ready", "status": ready}]},
        });
        if deleting {
            pod["metadata"]["deletionTimestamp"] = json!("2026-11-11T00:00:00Z");
        }
        serde_json::from_value(pod).unwrap_or_default()
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
