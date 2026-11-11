use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use http::{Method, Request, Response};
use http_body_util::{BodyExt as _, Full};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{Pod, Toleration};
use kube::Client;
use serde_json::{Value, json};

use super::{Applied, ready, stateful_set};

const DAEMONS: &str = "builder-system";
const NAMESPACE: &str = "team-a";
const NAME: &str = "buildkitd-team-a";
const POD: &str = "buildkitd-team-a-0";
const CLAIM: &str = "cache-buildkitd-team-a-0";

/// One request the fake API server answered.
#[derive(Debug, Clone)]
struct Call {
    method: Method,
    path: String,
    body: Value,
}

/// An in-memory API server for the objects [`stateful_set`] reads and writes. It keeps `uid`,
/// `resourceVersion` and delete preconditions as kube-apiserver does, refuses an update to
/// `volumeClaimTemplates` with 422, and deletes a `StatefulSet` in the foreground: the object stays
/// with a `deletionTimestamp` until [`Cluster::collect`].
#[derive(Debug, Default)]
struct Cluster {
    /// By `<resource>/<name>`.
    objects: BTreeMap<String, Value>,
    serial: u64,
    /// The answer to a dry-run create instead of success: code, reason, message.
    dry_run: Option<(u16, &'static str, &'static str)>,
    /// The answer to every apply instead of the normal handling.
    apply: Option<(u16, &'static str, &'static str)>,
    /// Another writer replaces the `StatefulSet` just before the next delete arrives.
    replace_before_delete: bool,
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

    fn deletes(&self) -> Vec<Call> {
        self.calls
            .iter()
            .filter(|c| c.method == Method::DELETE)
            .cloned()
            .collect()
    }

    fn answer(&mut self, method: &Method, path: &str, query: &str, body: Value) -> (u16, Value) {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let (resource, name) = match parts.as_slice() {
            [.., "namespaces", _, resource, name] => (*resource, Some(*name)),
            [.., "namespaces", _, resource] => (*resource, None),
            _ => return status(404, "NotFound", "unknown path"),
        };
        let key = name.map(|n| format!("{resource}/{n}"));
        match (method.as_str(), key) {
            ("GET", Some(key)) => self.objects.get(&key).map_or_else(
                || status(404, "NotFound", "not found"),
                |object| (200, object.clone()),
            ),
            ("GET", None) => self.list(resource, query),
            ("PATCH", Some(key)) => self.apply(&key, body),
            ("POST", None) if query.contains("dryRun=All") => {
                if let Some((code, reason, message)) = self.dry_run {
                    return status(code, reason, message);
                }
                let mut object = body;
                let base = object["metadata"]["generateName"].as_str().unwrap_or("");
                object["metadata"]["name"] = json!(format!("{base}x7k2p"));
                (201, object)
            }
            ("DELETE", Some(key)) => self.delete(&key, &body),
            _ => status(405, "MethodNotAllowed", "not handled by the fake"),
        }
    }

    fn list(&self, resource: &str, query: &str) -> (u16, Value) {
        let finished = ["Succeeded", "Failed", "Cancelled"];
        let items: Vec<Value> = self
            .objects
            .iter()
            .filter(|(key, _)| key.starts_with(&format!("{resource}/")))
            .map(|(_, object)| object.clone())
            .filter(|o| {
                !query.contains("fieldSelector")
                    || !finished.contains(&o["status"]["phase"].as_str().unwrap_or(""))
            })
            .collect();
        (200, json!({"metadata": {}, "items": items}))
    }

    fn apply(&mut self, key: &str, body: Value) -> (u16, Value) {
        if let Some((code, reason, message)) = self.apply {
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
        if std::mem::take(&mut self.replace_before_delete)
            && let Some(mut replaced) = self.objects.get(key).cloned()
        {
            replaced["metadata"]["uid"] = json!(format!("uid-{}", self.next()));
            self.put(key, replaced);
        }
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
            let query = request.uri().query().unwrap_or("").to_owned();
            let bytes = request.into_body().collect().await.unwrap().to_bytes();
            let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let (code, answer) = {
                let mut cluster = lock(&cluster);
                cluster.calls.push(Call {
                    method: method.clone(),
                    path: path.clone(),
                    body: body.clone(),
                });
                cluster.answer(&method, &path, &query, body)
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

/// The daemon's `StatefulSet` with its cache on `class`.
fn desired(class: &str) -> StatefulSet {
    serde_json::from_value(json!({
        "apiVersion": "apps/v1", "kind": "StatefulSet",
        "metadata": {"name": NAME, "namespace": DAEMONS},
        "spec": {
            "serviceName": NAME,
            "selector": {"matchLabels": {"app.kubernetes.io/name": "buildkitd"}},
            "template": {"spec": {"containers": [{"name": "buildkitd"}]}},
            "volumeClaimTemplates": [{
                "metadata": {"name": "cache"},
                "spec": {"storageClassName": class},
            }],
        },
    }))
    .unwrap_or_default()
}

/// A cluster with the daemon on `class`, its Pod and its cache claim.
fn running(class: &str) -> Shared {
    let mut cluster = Cluster::default();
    let set = cluster.put(
        &format!("statefulsets/{NAME}"),
        serde_json::to_value(desired(class)).unwrap_or_default(),
    );
    let owner = json!([{
        "apiVersion": "apps/v1", "kind": "StatefulSet", "name": NAME,
        "uid": set["metadata"]["uid"], "controller": true,
    }]);
    cluster.put(
        &format!("pods/{POD}"),
        json!({"metadata": {"name": POD, "ownerReferences": owner}}),
    );
    cluster.put(
        &format!("persistentvolumeclaims/{CLAIM}"),
        json!({"metadata": {"name": CLAIM, "ownerReferences": owner},
               "spec": {"storageClassName": class}}),
    );
    Arc::new(Mutex::new(cluster))
}

async fn wake(cluster: &Shared, class: &str) -> Result<Applied, kube::Error> {
    stateful_set(&client(cluster), DAEMONS, NAMESPACE, &desired(class)).await
}

fn set_key() -> String {
    format!("statefulsets/{NAME}")
}

#[tokio::test]
async fn invalid_new_object_keeps_the_old_daemon() {
    let cluster = running("standard");
    let old = lock(&cluster).uid(&set_key());
    lock(&cluster).dry_run = Some((422, "Invalid", "storageClassName: Invalid value"));

    let applied = wake(&cluster, "Not A Class").await;

    let Ok(Applied::Invalid(message)) = applied else {
        panic!("expected Invalid, got {applied:?}");
    };
    assert!(
        message.contains("storageClassName: Invalid value"),
        "{message}"
    );
    let cluster = lock(&cluster);
    assert!(cluster.deletes().is_empty());
    assert_eq!(cluster.uid(&set_key()), old);
}

#[tokio::test]
async fn a_failed_dry_run_keeps_the_daemon_and_returns_the_error() {
    let cases = [
        (403, "Forbidden", "cannot create statefulsets"),
        (401, "Unauthorized", "Unauthorized"),
        (429, "TooManyRequests", "slow down"),
        (503, "ServiceUnavailable", "etcd unavailable"),
    ];
    for (code, reason, message) in cases {
        let cluster = running("standard");
        let old = lock(&cluster).uid(&set_key());
        lock(&cluster).dry_run = Some((code, reason, message));

        let applied = wake(&cluster, "fast").await;

        assert!(
            matches!(&applied, Err(kube::Error::Api(s)) if s.code == code),
            "{reason}: {applied:?}"
        );
        let cluster = lock(&cluster);
        assert!(cluster.deletes().is_empty(), "{reason}");
        assert_eq!(cluster.uid(&set_key()), old, "{reason}");
    }
}

#[tokio::test]
async fn an_immutable_change_replaces_the_daemon_once_its_pod_and_claim_are_gone() {
    let cluster = running("standard");
    let set = object(&cluster, &set_key());
    let (old_uid, old_rv) = (
        set["metadata"]["uid"].clone(),
        set["metadata"]["resourceVersion"].clone(),
    );

    let first = wake(&cluster, "fast").await;
    assert!(matches!(first, Ok(Applied::Waiting(_))), "{first:?}");
    let deletes = lock(&cluster).deletes();
    let [delete] = deletes.as_slice() else {
        panic!("expected one delete, got {deletes:?}");
    };
    assert!(delete.path.ends_with(&format!("/statefulsets/{NAME}")));
    assert_eq!(delete.body["preconditions"]["uid"], old_uid);
    assert_eq!(delete.body["preconditions"]["resourceVersion"], old_rv);
    assert_eq!(delete.body["propagationPolicy"], "Foreground");

    // The old StatefulSet, then its Pod, then its claim are still there.
    let still = wake(&cluster, "fast").await;
    assert!(matches!(still, Ok(Applied::Waiting(_))), "{still:?}");
    lock(&cluster).collect(&set_key());
    let pod_left = wake(&cluster, "fast").await;
    assert!(matches!(pod_left, Ok(Applied::Waiting(_))), "{pod_left:?}");
    lock(&cluster).collect(&format!("pods/{POD}"));
    let claim_left = wake(&cluster, "fast").await;
    assert!(
        matches!(claim_left, Ok(Applied::Waiting(_))),
        "{claim_left:?}"
    );
    assert!(!lock(&cluster).objects.contains_key(&set_key()));
    assert_eq!(lock(&cluster).deletes().len(), 1, "no second delete");

    lock(&cluster).collect(&format!("persistentvolumeclaims/{CLAIM}"));
    let applied = wake(&cluster, "fast").await;
    let Ok(Applied::Set(set)) = applied else {
        panic!("expected the new StatefulSet, got {applied:?}");
    };
    assert_ne!(set.metadata.uid.as_deref(), old_uid.as_str());
    let stored = object(&cluster, &set_key());
    assert_eq!(
        stored["spec"]["volumeClaimTemplates"][0]["spec"]["storageClassName"],
        "fast"
    );
}

#[tokio::test]
async fn an_apply_conflict_deletes_nothing() {
    let cluster = running("standard");
    let old = lock(&cluster).uid(&set_key());
    lock(&cluster).apply = Some((409, "Conflict", "Apply failed with 1 conflict"));

    let applied = wake(&cluster, "fast").await;

    assert!(
        matches!(&applied, Err(kube::Error::Api(s)) if s.code == 409),
        "{applied:?}"
    );
    let cluster = lock(&cluster);
    assert!(cluster.deletes().is_empty());
    assert_eq!(cluster.uid(&set_key()), old);
}

#[tokio::test]
async fn an_object_replaced_between_read_and_delete_survives() {
    let cluster = running("standard");
    let fetched = lock(&cluster).uid(&set_key());
    lock(&cluster).replace_before_delete = true;

    let applied = wake(&cluster, "fast").await;

    assert!(matches!(applied, Ok(Applied::Waiting(_))), "{applied:?}");
    let deletes = lock(&cluster).deletes();
    let [delete] = deletes.as_slice() else {
        panic!("expected one delete, got {deletes:?}");
    };
    assert_eq!(
        delete.body["preconditions"]["uid"].as_str(),
        fetched.as_deref()
    );
    let survivor = object(&cluster, &set_key());
    assert_ne!(survivor["metadata"]["uid"].as_str(), fetched.as_deref());
    assert!(survivor["metadata"]["deletionTimestamp"].is_null());
}

#[tokio::test]
async fn a_replacement_waits_for_running_builds() {
    let cluster = running("standard");
    let old = lock(&cluster).uid(&set_key());
    for (name, phase, job) in [
        ("web-1", "Building", Some("web-1")),
        ("web-2", "Succeeded", Some("web-2")),
        ("web-3", "Pending", None),
    ] {
        lock(&cluster).put(
            &format!("buildruns/{name}"),
            json!({"metadata": {"name": name, "namespace": NAMESPACE},
                   "spec": {"buildRef": "web", "revision": "abc"},
                   "status": {"phase": phase, "jobRef": job}}),
        );
    }

    let waiting = wake(&cluster, "fast").await;
    assert!(
        matches!(&waiting, Ok(Applied::Waiting(m)) if m.contains("1 running build")),
        "{waiting:?}"
    );
    assert!(lock(&cluster).deletes().is_empty());
    assert_eq!(lock(&cluster).uid(&set_key()), old);

    lock(&cluster).collect("buildruns/web-1");
    let replacing = wake(&cluster, "fast").await;
    assert!(
        matches!(replacing, Ok(Applied::Waiting(_))),
        "{replacing:?}"
    );
    assert_eq!(lock(&cluster).deletes().len(), 1);
}

#[tokio::test]
async fn a_new_build_node_replaces_the_daemon_only_when_no_build_runs() {
    let cluster = running("standard");
    let mut moved = desired("standard");
    if let Some(pod) = moved.spec.as_mut().and_then(|s| s.template.spec.as_mut()) {
        pod.node_selector = Some(BTreeMap::from([(
            "builder.zeabur.com/builds".to_owned(),
            "true".to_owned(),
        )]));
    }
    let old = lock(&cluster).uid(&set_key());
    lock(&cluster).put(
        "buildruns/web-1",
        json!({"metadata": {"name": "web-1", "namespace": NAMESPACE},
               "spec": {"buildRef": "web", "revision": "abc"},
               "status": {"phase": "Building", "jobRef": "web-1"}}),
    );

    let client = client(&cluster);
    let waiting = stateful_set(&client, DAEMONS, NAMESPACE, &moved).await;
    assert!(
        matches!(&waiting, Ok(Applied::Waiting(m)) if m.contains("1 running build")),
        "{waiting:?}"
    );
    assert!(lock(&cluster).deletes().is_empty());
    assert_eq!(lock(&cluster).uid(&set_key()), old);

    lock(&cluster).collect("buildruns/web-1");
    let replacing = stateful_set(&client, DAEMONS, NAMESPACE, &moved).await;
    assert!(
        matches!(&replacing, Ok(Applied::Waiting(m)) if m.contains("node selector")),
        "{replacing:?}"
    );
    assert_eq!(lock(&cluster).deletes().len(), 1);
}

#[tokio::test]
async fn new_tolerations_wait_for_running_builds_and_then_apply() {
    let cluster = running("standard");
    let mut tainted = desired("standard");
    if let Some(pod) = tainted.spec.as_mut().and_then(|s| s.template.spec.as_mut()) {
        pod.tolerations = Some(vec![Toleration {
            key: Some("builder.zeabur.com/builds".to_owned()),
            operator: Some("Exists".to_owned()),
            ..Toleration::default()
        }]);
    }
    let old = lock(&cluster).uid(&set_key());
    lock(&cluster).put(
        "buildruns/web-1",
        json!({"metadata": {"name": "web-1", "namespace": NAMESPACE},
               "spec": {"buildRef": "web", "revision": "abc"},
               "status": {"phase": "Building", "jobRef": "web-1"}}),
    );

    let client = client(&cluster);
    let waiting = stateful_set(&client, DAEMONS, NAMESPACE, &tainted).await;
    assert!(
        matches!(&waiting, Ok(Applied::Waiting(m)) if m.contains("1 running build")),
        "{waiting:?}"
    );
    let stored = object(&cluster, &set_key());
    assert!(stored["spec"]["template"]["spec"]["tolerations"].is_null());

    lock(&cluster).collect("buildruns/web-1");
    let applied = stateful_set(&client, DAEMONS, NAMESPACE, &tainted).await;
    assert!(matches!(applied, Ok(Applied::Set(_))), "{applied:?}");
    assert!(lock(&cluster).deletes().is_empty());
    assert_eq!(lock(&cluster).uid(&set_key()), old);
    let stored = object(&cluster, &set_key());
    assert_eq!(
        stored["spec"]["template"]["spec"]["tolerations"][0]["key"],
        "builder.zeabur.com/builds"
    );
}

#[test]
fn only_a_ready_pod_of_the_current_statefulset_is_ready() {
    let set: StatefulSet = serde_json::from_value(json!({
        "metadata": {"name": NAME, "uid": "new"}, "spec": {"selector": {}, "template": {}}
    }))
    .unwrap_or_default();
    let pod = |owner: &str, deleting: bool, ready: &str| -> Pod {
        let mut pod = json!({
            "metadata": {"name": POD, "ownerReferences": [{
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
        ("ready, current set", pod("new", false, "True"), true),
        ("ready, deleted set", pod("old", false, "True"), false),
        ("ready, terminating", pod("new", true, "True"), false),
        ("not ready", pod("new", false, "False"), false),
    ];
    for (case, pod, want) in cases {
        assert_eq!(ready(&pod, &set), want, "{case}");
    }
}
