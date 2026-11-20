//! The receiver against hostile senders, without a cluster.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use http::HeaderValue;
use serde_json::json;

use super::*;

const SECRET: &[u8] = b"team-a-webhook-secret";
const OTHER: &[u8] = b"team-b-webhook-secret";
const APP: &[u8] = b"the-app-secret";

struct Fake {
    builds: Vec<Arc<Build>>,
    secrets: HashMap<(String, String, String), Vec<u8>>,
    app: Option<Vec<u8>>,
    touched: Arc<Mutex<Vec<String>>>,
    reads: Arc<AtomicUsize>,
}

impl Lookup for Fake {
    fn builds(&self) -> Vec<Arc<Build>> {
        self.builds.clone()
    }

    fn secret<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Option<Vec<u8>>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let found = self
            .secrets
            .get(&(namespace.to_owned(), name.to_owned(), key.to_owned()))
            .cloned();
        Box::pin(async move { found })
    }

    fn app_secret(&self) -> BoxFuture<'_, Option<Vec<u8>>> {
        let found = self.app.clone();
        Box::pin(async move { found })
    }

    fn touch<'a>(&'a self, namespace: &'a str, name: &'a str, _: Timestamp) -> BoxFuture<'a, ()> {
        self.touched
            .lock()
            .unwrap()
            .push(format!("{namespace}/{name}"));
        Box::pin(async {})
    }
}

fn build(
    namespace: &str,
    name: &str,
    url: &str,
    git_ref: &str,
    source: &serde_json::Value,
) -> Arc<Build> {
    let mut spec = json!({
        "source": {"type": "git", "git": {"url": url, "ref": git_ref}},
        "builderRef": "dockerfile",
        "triggers": {"webhook": {"secretRef": {"name": "hook", "key": "secret"}}},
    });
    if let Some(extra) = source.as_object() {
        for (k, v) in extra {
            match k.as_str() {
                "githubApp" => {
                    spec["source"]["git"]["githubApp"] = v.clone();
                    spec["triggers"] = json!({});
                }
                _ => spec[k] = v.clone(),
            }
        }
    }
    Arc::new(
        serde_json::from_value(json!({
            "apiVersion": "builder.zeabur.com/v1alpha1",
            "kind": "Build",
            "metadata": {"name": name, "namespace": namespace},
            "spec": spec,
        }))
        .unwrap(),
    )
}

fn settings(max_body: usize, burst: u32) -> Settings {
    Settings {
        addr: "127.0.0.1:0".parse().unwrap(),
        prefix: "/hooks".to_owned(),
        max_body,
        rate: 1,
        burst,
    }
}

struct World {
    receiver: Receiver,
    touched: Arc<Mutex<Vec<String>>>,
}

fn world_with(max_body: usize, burst: u32) -> World {
    let builds = vec![
        build(
            "team-a",
            "api",
            "https://github.com/acme/api.git",
            "main",
            &json!({}),
        ),
        build(
            "team-b",
            "api",
            "git@github.com:Acme/API.git",
            "main",
            &json!({}),
        ),
        build(
            "team-a",
            "docs",
            "https://github.com/acme/api",
            "gh-pages",
            &json!({}),
        ),
        build(
            "team-a",
            "head",
            "https://github.com/acme/api",
            "HEAD",
            &json!({}),
        ),
        build(
            "team-a",
            "paused",
            "https://github.com/acme/api",
            "main",
            &json!({"suspend": true}),
        ),
        build(
            "team-c",
            "via-app",
            "https://github.com/acme/api",
            "main",
            &json!({"githubApp": {"installationId": 7}}),
        ),
        build(
            "team-a",
            "lab",
            "https://gitlab.example.com/Group/App.git",
            "main",
            &json!({}),
        ),
        build(
            "team-a",
            "tea",
            "https://git.example.com/team/app.git",
            "main",
            &json!({}),
        ),
        build(
            "team-a",
            "bucket",
            "https://bitbucket.org/team/app.git",
            "main",
            &json!({}),
        ),
    ];
    let mut secrets = HashMap::new();
    for (namespace, value) in [("team-a", SECRET), ("team-b", OTHER)] {
        secrets.insert(
            (namespace.to_owned(), "hook".to_owned(), "secret".to_owned()),
            value.to_vec(),
        );
    }
    let touched = Arc::new(Mutex::new(Vec::new()));
    World {
        receiver: Receiver::new(
            Box::new(Fake {
                builds,
                secrets,
                app: Some(APP.to_vec()),
                touched: Arc::clone(&touched),
                reads: Arc::default(),
            }),
            settings(max_body, burst),
        ),
        touched,
    }
}

fn world() -> World {
    world_with(64 * 1024, 1000)
}

impl World {
    fn woken(&self) -> Vec<String> {
        let mut touched: Vec<String> = self.touched.lock().unwrap().drain(..).collect();
        touched.sort();
        touched
    }
}

fn sign_hex(key: &[u8], body: &[u8]) -> String {
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), body);
    tag.as_ref()
        .iter()
        .flat_map(|b| [b >> 4, b & 15])
        .filter_map(|n| char::from_digit(n.into(), 16))
        .collect()
}

fn github_push(git_ref: &str) -> Vec<u8> {
    json!({
        "ref": git_ref,
        "after": "3e1a9c2d4f0b5a6c7d8e9f0a1b2c3d4e5f6a7b8c",
        "repository": {
            "clone_url": "https://github.com/acme/api.git",
            "ssh_url": "git@github.com:acme/api.git",
            "html_url": "https://github.com/acme/api"
        }
    })
    .to_string()
    .into_bytes()
}

fn github_headers(key: &[u8], body: &[u8], delivery: &str, event: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("x-github-event", HeaderValue::from_str(event).unwrap());
    h.insert(
        "x-github-delivery",
        HeaderValue::from_str(delivery).unwrap(),
    );
    h.insert(
        "x-hub-signature-256",
        HeaderValue::from_str(&format!("sha256={}", sign_hex(key, body))).unwrap(),
    );
    h
}

fn now() -> Timestamp {
    "2026-10-23T00:00:00Z".parse().unwrap()
}

async fn post(
    w: &World,
    provider: Provider,
    headers: HeaderMap,
    body: &[u8],
) -> (StatusCode, String) {
    w.receiver.receive(provider, &headers, body, now()).await
}

#[tokio::test]
async fn a_signed_push_wakes_only_the_build_whose_secret_signed_it() {
    let w = world();
    let body = github_push("refs/heads/main");
    let (status, text) = post(
        &w,
        Provider::GitHub,
        github_headers(SECRET, &body, "d1", "push"),
        &body,
    )
    .await;
    assert_eq!((status, text.as_str()), (StatusCode::ACCEPTED, "2 woken"));
    // team-b follows the same repository and branch, under another secret; `docs` follows
    // another branch; `paused` is suspended; `via-app` listens to the App only.
    assert_eq!(w.woken(), ["team-a/api", "team-a/head"]);

    let (status, _) = post(
        &w,
        Provider::GitHub,
        github_headers(OTHER, &body, "d2", "push"),
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(w.woken(), ["team-b/api"]);
}

#[tokio::test]
async fn bad_signatures_are_refused() {
    let w = world();
    let body = github_push("refs/heads/main");
    let cases: Vec<HeaderMap> = vec![
        github_headers(b"guessed", &body, "d1", "push"),
        github_headers(SECRET, b"another body", "d2", "push"),
        {
            let mut h = github_headers(SECRET, &body, "d3", "push");
            h.remove("x-hub-signature-256");
            h
        },
        {
            let mut h = github_headers(SECRET, &body, "d4", "push");
            let value = h
                .get("x-hub-signature-256")
                .unwrap()
                .to_str()
                .unwrap()
                .replace("sha256=", "sha1=");
            h.insert(
                "x-hub-signature-256",
                HeaderValue::from_str(&value).unwrap(),
            );
            h
        },
        {
            let mut h = github_headers(SECRET, &body, "d5", "push");
            h.insert("x-hub-signature-256", HeaderValue::from_static("sha256=zz"));
            h
        },
    ];
    for headers in cases {
        let (status, _) = post(&w, Provider::GitHub, headers, &body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    // A signature over a changed body: the ref moved to a branch the Build follows.
    let signed = github_push("refs/heads/gh-pages");
    let tampered = github_push("refs/heads/main");
    let (status, _) = post(
        &w,
        Provider::GitHub,
        github_headers(SECRET, &signed, "d6", "push"),
        &tampered,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(w.woken(), Vec::<String>::new());
}

#[tokio::test]
async fn unknown_repositories_look_like_bad_signatures() {
    let w = world();
    let body = json!({"ref": "refs/heads/main", "repository": {"clone_url": "https://github.com/acme/secret.git"}})
        .to_string()
        .into_bytes();
    let unknown = post(
        &w,
        Provider::GitHub,
        github_headers(SECRET, &body, "d1", "push"),
        &body,
    )
    .await;
    let good = github_push("refs/heads/main");
    let forged = post(
        &w,
        Provider::GitHub,
        github_headers(b"x", &good, "d2", "push"),
        &good,
    )
    .await;
    assert_eq!(unknown, forged);
    assert_eq!(unknown.0, StatusCode::UNAUTHORIZED);
    assert_eq!(w.woken(), Vec::<String>::new());
}

#[tokio::test]
async fn a_push_to_another_branch_wakes_nothing() {
    let w = world();
    let body = github_push("refs/heads/feature/main");
    let (status, text) = post(
        &w,
        Provider::GitHub,
        github_headers(SECRET, &body, "d1", "push"),
        &body,
    )
    .await;
    assert_eq!((status, text.as_str()), (StatusCode::ACCEPTED, "1 woken"));
    // Only the Build that follows the default branch, which it has not resolved yet.
    assert_eq!(w.woken(), ["team-a/head"]);

    let tag = github_push("refs/tags/main");
    let (_, text) = post(
        &w,
        Provider::GitHub,
        github_headers(SECRET, &tag, "d2", "push"),
        &tag,
    )
    .await;
    assert_eq!(
        text, "2 woken",
        "a tag named like the branch matches a short ref, as Git resolves it"
    );
    w.woken();
}

#[tokio::test]
async fn replays_are_dropped() {
    let w = world();
    let body = github_push("refs/heads/main");
    let headers = github_headers(SECRET, &body, "d1", "push");
    assert_eq!(
        post(&w, Provider::GitHub, headers.clone(), &body).await.1,
        "2 woken"
    );
    assert_eq!(w.woken(), ["team-a/api", "team-a/head"]);
    assert_eq!(
        post(&w, Provider::GitHub, headers, &body).await,
        (StatusCode::ACCEPTED, "already received".to_owned())
    );
    assert_eq!(w.woken(), Vec::<String>::new());
}

#[tokio::test]
async fn deliveries_without_a_push_wake_nothing() {
    let w = world();
    let ping = json!({"zen": "Design for failure.", "hook_id": 1,
        "repository": {"clone_url": "https://github.com/acme/api.git"}})
    .to_string()
    .into_bytes();
    assert_eq!(
        post(
            &w,
            Provider::GitHub,
            github_headers(SECRET, &ping, "p1", "ping"),
            &ping
        )
        .await,
        (StatusCode::ACCEPTED, "ignored".to_owned())
    );
    let deleted = json!({"ref": "refs/heads/main", "deleted": true,
        "after": "0000000000000000000000000000000000000000",
        "repository": {"clone_url": "https://github.com/acme/api.git"}})
    .to_string()
    .into_bytes();
    assert_eq!(
        post(
            &w,
            Provider::GitHub,
            github_headers(SECRET, &deleted, "p2", "push"),
            &deleted
        )
        .await
        .1,
        "0 woken"
    );
    assert_eq!(w.woken(), Vec::<String>::new());
}

/// GitHub's "Redeliver" sends the delivery again with its id; only the first one wakes. A copy
/// that reaches another replica only asks the Git server once more, and
/// [`crate::decide::next_run`] builds no commit twice.
#[tokio::test]
async fn app_redeliveries_wake_once() {
    let w = world();
    let body = github_push("refs/heads/main");
    let headers = github_headers(APP, &body, "app-d1", "push");
    assert_eq!(
        post(&w, Provider::GitHubApp, headers.clone(), &body)
            .await
            .1,
        "1 woken"
    );
    assert_eq!(w.woken(), ["team-c/via-app"]);
    assert_eq!(
        post(&w, Provider::GitHubApp, headers, &body).await,
        (StatusCode::ACCEPTED, "already received".to_owned())
    );
    assert_eq!(w.woken(), Vec::<String>::new());
}

#[tokio::test]
async fn the_app_wakes_its_builds_with_its_own_secret() {
    let w = world();
    let body = github_push("refs/heads/main");
    assert_eq!(
        post(
            &w,
            Provider::GitHubApp,
            github_headers(APP, &body, "a1", "push"),
            &body
        )
        .await
        .1,
        "1 woken"
    );
    assert_eq!(w.woken(), ["team-c/via-app"]);
    // A Build's secret does not sign for the App, nor the App's for a Build.
    assert_eq!(
        post(
            &w,
            Provider::GitHubApp,
            github_headers(SECRET, &body, "a2", "push"),
            &body
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post(
            &w,
            Provider::GitHub,
            github_headers(APP, &body, "a3", "push"),
            &body
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    // GitHub pings the App's webhook when the App is created; it names no repository.
    let ping = br#"{"zen": "Keep it logically awesome.", "hook_id": 9}"#;
    assert_eq!(
        post(
            &w,
            Provider::GitHubApp,
            github_headers(APP, ping, "a4", "ping"),
            ping
        )
        .await,
        (StatusCode::ACCEPTED, "ignored".to_owned())
    );
    assert_eq!(w.woken(), Vec::<String>::new());
}

fn gitlab_body() -> Vec<u8> {
    json!({"ref": "refs/heads/main", "after": "3e1a9c2d4f0b5a6c7d8e9f0a1b2c3d4e5f6a7b8c",
        "project": {"git_http_url": "https://gitlab.example.com/Group/App.git"}})
    .to_string()
    .into_bytes()
}

fn standard_headers(key: &[u8], id: &str, at: i64, body: &[u8]) -> HeaderMap {
    let mut signed = format!("{id}.{at}.").into_bytes();
    signed.extend_from_slice(body);
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), &signed);
    let mut h = HeaderMap::new();
    h.insert("webhook-id", HeaderValue::from_str(id).unwrap());
    h.insert(
        "webhook-timestamp",
        HeaderValue::from_str(&at.to_string()).unwrap(),
    );
    h.insert(
        "webhook-signature",
        HeaderValue::from_str(&format!("v1,bm90IGl0 v1,{}", STANDARD.encode(tag.as_ref())))
            .unwrap(),
    );
    h
}

#[tokio::test]
async fn gitlab_tokens_and_signing_tokens() {
    let w = world();
    let body = gitlab_body();
    let mut headers = HeaderMap::new();
    headers.insert("x-gitlab-event", HeaderValue::from_static("Push Hook"));
    headers.insert("x-gitlab-event-uuid", HeaderValue::from_static("e1"));
    headers.insert("x-gitlab-token", HeaderValue::from_bytes(SECRET).unwrap());
    assert_eq!(
        post(&w, Provider::GitLab, headers.clone(), &body).await.1,
        "1 woken"
    );
    assert_eq!(w.woken(), ["team-a/lab"]);

    headers.insert("x-gitlab-event-uuid", HeaderValue::from_static("e2"));
    headers.insert(
        "x-gitlab-token",
        HeaderValue::from_static("team-a-webhook-secre"),
    );
    assert_eq!(
        post(&w, Provider::GitLab, headers.clone(), &body).await.0,
        StatusCode::UNAUTHORIZED
    );

    // A signing token: Standard Webhooks, and the plain token no longer counts.
    let key = b"0123456789abcdef0123456789abcdef";
    let whsec = format!("whsec_{}", STANDARD.encode(key));
    let fake = Fake {
        builds: w.receiver.lookup.builds(),
        secrets: HashMap::from([(
            ("team-a".to_owned(), "hook".to_owned(), "secret".to_owned()),
            whsec.into_bytes(),
        )]),
        app: None,
        touched: Arc::new(Mutex::new(Vec::new())),
        reads: Arc::default(),
    };
    let signed = Receiver::new(Box::new(fake), settings(1024, 10));
    let mut h = standard_headers(key, "msg_1", now().as_second(), &body);
    h.insert("x-gitlab-event", HeaderValue::from_static("Push Hook"));
    assert_eq!(
        signed.receive(Provider::GitLab, &h, &body, now()).await.1,
        "1 woken"
    );
    let stale = now().as_second() - 301;
    let mut h = standard_headers(key, "msg_2", stale, &body);
    h.insert("x-gitlab-event", HeaderValue::from_static("Push Hook"));
    assert_eq!(
        signed.receive(Provider::GitLab, &h, &body, now()).await.0,
        StatusCode::UNAUTHORIZED,
        "a timestamp outside the tolerance is a replay"
    );
}

#[tokio::test]
async fn gitea_and_forgejo_signatures() {
    let w = world();
    let gitea =
        json!({"ref": "refs/heads/main", "after": "3e1a9c2d4f0b5a6c7d8e9f0a1b2c3d4e5f6a7b8c",
        "repository": {"clone_url": "https://git.example.com/team/app.git"}})
        .to_string()
        .into_bytes();
    let mut h = HeaderMap::new();
    h.insert("x-gitea-event", HeaderValue::from_static("push"));
    h.insert("x-gitea-delivery", HeaderValue::from_static("g1"));
    h.insert(
        "x-forgejo-signature",
        HeaderValue::from_str(&sign_hex(SECRET, &gitea)).unwrap(),
    );
    assert_eq!(
        post(&w, Provider::Gitea, h.clone(), &gitea).await.1,
        "1 woken"
    );
    assert_eq!(w.woken(), ["team-a/tea"]);
    // Gitea sends the headers with an empty digest when the webhook has no secret.
    h.insert("x-gitea-delivery", HeaderValue::from_static("g2"));
    h.insert("x-forgejo-signature", HeaderValue::from_static(""));
    assert_eq!(
        post(&w, Provider::Gitea, h, &gitea).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn bitbucket_signature() {
    let w = world();
    let bucket = json!({"repository": {"full_name": "team/app"},
        "push": {"changes": [{"new": {"type": "branch", "name": "main"}}]}})
    .to_string()
    .into_bytes();
    let mut h = HeaderMap::new();
    h.insert("x-event-key", HeaderValue::from_static("repo:push"));
    h.insert("x-request-uuid", HeaderValue::from_static("b1"));
    h.insert(
        "x-hub-signature",
        HeaderValue::from_str(&format!("sha256={}", sign_hex(SECRET, &bucket))).unwrap(),
    );
    assert_eq!(post(&w, Provider::Bitbucket, h, &bucket).await.1, "1 woken");
    assert_eq!(w.woken(), ["team-a/bucket"]);
}

#[tokio::test]
async fn generic_standard_webhooks() {
    let w = world();
    let generic = br#"{"repository": "https://github.com/acme/api", "ref": "main"}"#;
    let h = standard_headers(SECRET, "msg_1", now().as_second(), generic);
    assert_eq!(
        post(&w, Provider::Generic, h.clone(), generic).await.1,
        "2 woken"
    );
    assert_eq!(w.woken(), ["team-a/api", "team-a/head"]);
    assert_eq!(
        post(&w, Provider::Generic, h, generic).await.1,
        "already received"
    );
    let future = standard_headers(SECRET, "msg_2", now().as_second() + 3600, generic);
    assert_eq!(
        post(&w, Provider::Generic, future, generic).await.0,
        StatusCode::UNAUTHORIZED
    );
}

/// A receiver for `count` `Build`s of `acme/api`, one per namespace, each with its own Secret
/// when `own_secrets` is set, else all naming the same one in `team-a`.
fn crowd(count: usize, own_secrets: bool) -> (Receiver, Arc<Mutex<Vec<String>>>, Arc<AtomicUsize>) {
    let mut builds = Vec::new();
    let mut secrets = HashMap::new();
    for i in 0..count {
        let namespace = if own_secrets {
            format!("team-{i}")
        } else {
            "team-a".to_owned()
        };
        builds.push(build(
            &namespace,
            &format!("api-{i}"),
            "https://github.com/acme/api",
            "main",
            &json!({}),
        ));
        secrets.insert(
            (namespace, "hook".to_owned(), "secret".to_owned()),
            SECRET.to_vec(),
        );
    }
    let touched = Arc::new(Mutex::new(Vec::new()));
    let reads = Arc::new(AtomicUsize::new(0));
    let fake = Fake {
        builds,
        secrets,
        app: None,
        touched: Arc::clone(&touched),
        reads: Arc::clone(&reads),
    };
    (
        Receiver::new(Box::new(fake), settings(64 * 1024, 1000)),
        touched,
        reads,
    )
}

#[tokio::test]
async fn builds_that_share_a_secret_are_all_woken_with_one_read() {
    let count = MAX_SECRETS + 8;
    let (receiver, touched, reads) = crowd(count, false);
    let body = github_push("refs/heads/main");
    let (_, text) = receiver
        .receive(
            Provider::GitHub,
            &github_headers(SECRET, &body, "c1", "push"),
            &body,
            now(),
        )
        .await;
    assert_eq!(text, format!("{count} woken"));
    assert_eq!(touched.lock().unwrap().len(), count);
    assert_eq!(reads.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn more_secrets_than_one_delivery_reads_are_read_in_turn() {
    let count = MAX_SECRETS + 8;
    let (receiver, touched, reads) = crowd(count, true);
    let body = github_push("refs/heads/main");
    for (delivery, expected_reads) in [("c1", MAX_SECRETS), ("c2", 2 * MAX_SECRETS)] {
        receiver
            .receive(
                Provider::GitHub,
                &github_headers(SECRET, &body, delivery, "push"),
                &body,
                now(),
            )
            .await;
        assert_eq!(
            reads.load(Ordering::Relaxed),
            expected_reads,
            "reads are bounded"
        );
    }
    let woken: HashSet<String> = touched.lock().unwrap().drain(..).collect();
    assert_eq!(
        woken.len(),
        count,
        "the next delivery reads the Secrets the first left out"
    );
}

#[tokio::test]
async fn malformed_bodies() {
    let w = world();
    let (status, _) = post(
        &w,
        Provider::GitHub,
        github_headers(SECRET, b"{", "m1", "push"),
        b"{",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(w.woken(), Vec::<String>::new());
}

fn request(path: &str, method: Method, body: Vec<u8>, length: bool) -> Request<Full<Bytes>> {
    let mut builder = Request::builder().method(method).uri(path);
    if length {
        builder = builder.header(header::CONTENT_LENGTH, body.len());
    }
    let mut request = builder.body(Full::new(Bytes::from(body))).unwrap();
    for (name, value) in &github_headers(SECRET, &github_push("refs/heads/main"), "h1", "push") {
        request.headers_mut().insert(name, value.clone());
    }
    request
}

#[tokio::test]
async fn http_limits() {
    let w = world_with(1024, 1000);
    let r = &w.receiver;
    let big = vec![b' '; 2048];
    assert_eq!(
        r.handle(request("/hooks/github", Method::POST, big.clone(), true))
            .await
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        r.handle(request("/hooks/github", Method::POST, big, false))
            .await
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        r.handle(request("/hooks/github", Method::GET, Vec::new(), false))
            .await
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        r.handle(request("/hooks/svn", Method::POST, Vec::new(), false))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        r.handle(request(
            "/elsewhere/github",
            Method::POST,
            Vec::new(),
            false
        ))
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        r.handle(request("/healthz", Method::GET, Vec::new(), false))
            .await
            .status(),
        StatusCode::OK
    );
    let ok = r
        .handle(request(
            "/hooks/github",
            Method::POST,
            github_push("refs/heads/main"),
            true,
        ))
        .await;
    assert_eq!(ok.status(), StatusCode::ACCEPTED);
    assert_eq!(w.woken(), ["team-a/api", "team-a/head"]);
}

#[tokio::test]
async fn deliveries_are_rate_limited() {
    let w = world_with(1024, 3);
    let mut statuses = Vec::new();
    for _ in 0..5 {
        statuses.push(
            w.receiver
                .handle(request("/hooks/github", Method::POST, b"{".to_vec(), true))
                .await
                .status(),
        );
    }
    assert_eq!(
        statuses,
        [
            StatusCode::BAD_REQUEST,
            StatusCode::BAD_REQUEST,
            StatusCode::BAD_REQUEST,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::TOO_MANY_REQUESTS
        ]
    );
}

#[test]
fn bucket_refills() {
    let start = Instant::now();
    let mut bucket = Bucket::new(2, 2);
    assert!(bucket.take(start) && bucket.take(start) && !bucket.take(start));
    assert!(bucket.take(start + Duration::from_millis(500)));
    assert!(!bucket.take(start + Duration::from_millis(500)));
}

#[test]
fn seen_forgets_the_oldest() {
    let mut seen = Seen::default();
    for i in 0..=SEEN {
        assert!(seen.insert(i.to_string()));
    }
    assert!(seen.insert("0".to_owned()), "evicted");
    assert!(!seen.insert(SEEN.to_string()));
    assert_eq!(seen.order.len(), SEEN);
}

#[tokio::test]
async fn each_new_delivery_writes_the_wake_again() {
    let w = world();
    let body = github_push("refs/heads/main");
    post(
        &w,
        Provider::GitHub,
        github_headers(SECRET, &body, "t1", "push"),
        &body,
    )
    .await;
    post(
        &w,
        Provider::GitHub,
        github_headers(SECRET, &body, "t2", "push"),
        &body,
    )
    .await;
    assert_eq!(
        w.woken(),
        ["team-a/api", "team-a/api", "team-a/head", "team-a/head"],
        "the wake lives in status, not in this replica"
    );
}

#[test]
fn empty_secrets_never_verify() {
    let mut h = HeaderMap::new();
    h.insert("x-gitlab-token", HeaderValue::from_static(""));
    assert!(!verify(Provider::GitLab, b"", &h, b"{}", now()));
    h.insert(
        "x-hub-signature-256",
        HeaderValue::from_str(&format!("sha256={}", sign_hex(b"", b"{}"))).unwrap(),
    );
    assert!(!verify(Provider::GitHub, b"", &h, b"{}", now()));
}
