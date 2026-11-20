//! The webhook receiver: `POST <prefix>/<provider>` wakes the `Build`s a push concerns.
//!
//! A delivery is read for one thing only: which repository and refs it names
//! ([`zeabur_builder_core::hook`]). The `Build`s that follow that repository and name a webhook
//! secret are candidates; those whose secret verifies the signature, and whose ref was pushed,
//! are woken and ask the Git server themselves. Unknown repositories and bad signatures get the
//! same `401`, so the receiver does not tell which repositories it builds. Candidates that name
//! the same Secret key share one read, and one delivery reads at most [`MAX_SECRETS`] keys.
//!
//! Signatures, compared in constant time:
//!
//! - GitHub: `X-Hub-Signature-256: sha256=<hex>`, an HMAC-SHA256 of the body ([GitHub]).
//! - Gitea and Forgejo: `X-Gitea-Signature` or `X-Forgejo-Signature`, the same HMAC in hex without
//!   a prefix; `X-Hub-Signature-256` is accepted too ([Gitea], [Forgejo]).
//! - Bitbucket Cloud: `X-Hub-Signature: sha256=<hex>`, the same HMAC ([Bitbucket]).
//! - GitLab: `webhook-signature` when the webhook has a signing token, as Standard Webhooks;
//!   otherwise the secret token, sent as is in `X-Gitlab-Token` ([GitLab]).
//! - Generic: [Standard Webhooks]: `webhook-signature: v1,<base64>`, an HMAC-SHA256 of
//!   `<webhook-id>.<webhook-timestamp>.<body>` with the key a `whsec_` secret encodes; the spec
//!   names no tolerance, so the timestamp may be [`TOLERANCE`] away from the controller's clock.
//! - The GitHub App: as GitHub, with the App's webhook secret instead of a `Build`'s.
//!
//! A woken `Build` gets `status.lastWebhookAt`; the leader's reconcile acts on it
//! ([`crate::decide::wake`]). Nothing else is shared, so any replica can receive, and what each
//! keeps in memory is per replica on purpose:
//!
//! - Replays: a delivery id seen for a `Build` in the replica's last [`SEEN`] verified deliveries
//!   is dropped. A replay that reaches another replica, or comes after a restart, only causes one
//!   more poll, at least [`crate::decide::WAKE_GAP`] after the last, which asks the Git server; it
//!   can never build a commit the ref has moved away from.
//! - A token bucket bounds deliveries per second per replica, so the whole receiver admits the rate
//!   times the replicas. Bodies over the limit are refused before they are read in full.
//!
//! [GitHub]: https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries
//! [Gitea]: https://docs.gitea.com/usage/webhooks
//! [Forgejo]: https://forgejo.org/docs/latest/user/webhooks/
//! [Bitbucket]: https://support.atlassian.com/bitbucket-cloud/docs/manage-webhooks/
//! [GitLab]: https://docs.gitlab.com/user/project/integrations/webhooks/
//! [Standard Webhooks]: https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md

use std::collections::{HashSet, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use aws_lc_rs::constant_time::verify_slices_are_equal;
use aws_lc_rs::{digest, hmac};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;
use http::{HeaderMap, Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::body::Body;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use jiff::Timestamp;
use kube::ResourceExt as _;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use zeabur_builder_api::build::Build;
use zeabur_builder_core::hook::{self, Delivery, Event, Provider};
use zeabur_builder_core::repo::RepoKey;

/// How far a Standard Webhooks timestamp may be from the controller's clock.
pub(crate) const TOLERANCE: Duration = Duration::from_mins(5);
/// Verified delivery ids remembered, to drop replays.
pub(crate) const SEEN: usize = 4096;
/// Most webhook Secrets one delivery reads. A delivery is not yet verified, so this bounds the
/// reads a sender can cause. `Build`s that name the same Secret key share one read.
pub(crate) const MAX_SECRETS: usize = 32;
/// Wakes written at once.
const MAX_TOUCHES: usize = 8;
/// How long a client may take to send its headers, and then its body.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Connections served at once.
const MAX_CONNECTIONS: usize = 64;

/// Why the receiver stopped.
#[derive(Debug, Error)]
pub(crate) enum WebhookError {
    /// The address is taken or not ours.
    #[error("cannot listen on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
}

/// The receiver's settings, from the command line.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// Where to listen.
    pub(crate) addr: SocketAddr,
    /// The path deliveries arrive under, such as `/hooks`.
    pub(crate) prefix: String,
    /// The largest body read.
    pub(crate) max_body: usize,
    /// Deliveries per second, on average.
    pub(crate) rate: u32,
    /// Deliveries at once, above the average.
    pub(crate) burst: u32,
}

/// What the receiver reads from the cluster.
pub(crate) trait Lookup: Send + Sync {
    /// Every `Build`.
    fn builds(&self) -> Vec<Arc<Build>>;
    /// One key of a Secret.
    fn secret<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Option<Vec<u8>>>;
    /// The GitHub App webhook's secret.
    fn app_secret(&self) -> BoxFuture<'_, Option<Vec<u8>>>;
    /// Sets a `Build`'s `status.lastWebhookAt`, which its reconciler watches.
    fn touch<'a>(&'a self, namespace: &'a str, name: &'a str, at: Timestamp) -> BoxFuture<'a, ()>;
}

/// A token bucket.
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    rate: f64,
    burst: f64,
    at: Instant,
}

impl Bucket {
    fn new(rate: u32, burst: u32) -> Self {
        Self {
            tokens: f64::from(burst),
            rate: f64::from(rate),
            burst: f64::from(burst),
            at: Instant::now(),
        }
    }

    fn take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = elapsed.mul_add(self.rate, self.tokens).min(self.burst);
        self.at = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Delivery ids already acted on, oldest first.
#[derive(Debug, Default)]
struct Seen {
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl Seen {
    /// Records `id`; false when it was there already.
    fn insert(&mut self, id: String) -> bool {
        if self.set.contains(&id) {
            return false;
        }
        if self.order.len() >= SEEN
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        self.order.push_back(id.clone());
        self.set.insert(id);
        true
    }
}

/// The receiver.
pub(crate) struct Receiver {
    lookup: Box<dyn Lookup>,
    settings: Settings,
    bucket: Mutex<Bucket>,
    seen: Mutex<Seen>,
    turn: AtomicUsize,
}

impl std::fmt::Debug for Receiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

fn reply(status: StatusCode, text: &str) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(format!("{text}\n"))));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

fn hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Whether `signature`, hex with an optional `prefix`, is the HMAC-SHA256 of `body` under `key`.
fn hmac_hex(key: &[u8], body: &[u8], signature: Option<&str>, prefix: &str) -> bool {
    let Some(tag) = signature.and_then(|s| s.strip_prefix(prefix)).and_then(hex) else {
        return false;
    };
    hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, key), body, &tag).is_ok()
}

/// The HMAC key a Standard Webhooks secret encodes; a secret without `whsec_` is used as is.
fn standard_key(secret: &[u8]) -> Option<Vec<u8>> {
    secret.strip_prefix(b"whsec_").map_or_else(
        || Some(secret.to_vec()),
        |encoded| STANDARD.decode(encoded).ok(),
    )
}

/// Standard Webhooks: any `v1,` signature over `id.timestamp.body`, within [`TOLERANCE`].
fn standard(secret: &[u8], headers: &HeaderMap, body: &[u8], now: Timestamp) -> bool {
    let (Some(id), Some(timestamp), Some(signatures), Some(key)) = (
        header_str(headers, "webhook-id"),
        header_str(headers, "webhook-timestamp"),
        header_str(headers, "webhook-signature"),
        standard_key(secret),
    ) else {
        return false;
    };
    let Ok(seconds) = timestamp.parse::<i64>() else {
        return false;
    };
    let skew = now.as_second().abs_diff(seconds);
    if skew > TOLERANCE.as_secs() {
        return false;
    }
    let mut signed = Vec::with_capacity(id.len() + timestamp.len() + body.len() + 2);
    signed.extend_from_slice(id.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(timestamp.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(body);
    let key = hmac::Key::new(hmac::HMAC_SHA256, &key);
    signatures
        .split(' ')
        .filter_map(|s| s.strip_prefix("v1,"))
        .filter_map(|s| STANDARD.decode(s).ok())
        .any(|tag| hmac::verify(&key, &signed, &tag).is_ok())
}

/// Whether `secret` vouches for the delivery.
pub(crate) fn verify(
    provider: Provider,
    secret: &[u8],
    headers: &HeaderMap,
    body: &[u8],
    now: Timestamp,
) -> bool {
    if secret.is_empty() {
        return false;
    }
    let get = |name| header_str(headers, name);
    match provider {
        Provider::GitHub | Provider::GitHubApp => {
            hmac_hex(secret, body, get("x-hub-signature-256"), "sha256=")
        }
        Provider::Gitea => {
            hmac_hex(secret, body, get("x-gitea-signature"), "")
                || hmac_hex(secret, body, get("x-forgejo-signature"), "")
                || hmac_hex(secret, body, get("x-hub-signature-256"), "sha256=")
        }
        Provider::Bitbucket => hmac_hex(secret, body, get("x-hub-signature"), "sha256="),
        Provider::GitLab if headers.contains_key("webhook-signature") => {
            standard(secret, headers, body, now)
        }
        Provider::GitLab => get("x-gitlab-token").is_some_and(|token| {
            // Digests, so the comparison takes as long whatever the token's length.
            let a = digest::digest(&digest::SHA256, token.as_bytes());
            let b = digest::digest(&digest::SHA256, secret);
            verify_slices_are_equal(a.as_ref(), b.as_ref()).is_ok()
        }),
        Provider::Generic => standard(secret, headers, body, now),
    }
}

/// The `Build`s a delivery may concern: they follow one of its repositories and listen to its
/// kind of webhook, and are not suspended.
pub(crate) fn candidates(
    builds: &[Arc<Build>],
    provider: Provider,
    repositories: &[RepoKey],
) -> Vec<Arc<Build>> {
    builds
        .iter()
        .filter(|b| !b.spec.suspend)
        .filter(|b| match provider {
            Provider::GitHubApp => b.spec.source.github_app.is_some(),
            Provider::GitHub
            | Provider::GitLab
            | Provider::Gitea
            | Provider::Bitbucket
            | Provider::Generic => b.spec.triggers.webhook.is_some(),
        })
        .filter(|b| {
            b.spec.source.git.as_ref().is_some_and(|git| {
                RepoKey::parse(&git.url).is_ok_and(|k| repositories.contains(&k))
            })
        })
        .cloned()
        .collect()
}

/// The webhook Secret key a `Build` names.
type SecretKey = (String, String, String);

fn secret_key(build: &Build) -> Option<SecretKey> {
    let webhook = build.spec.triggers.webhook.as_ref()?;
    Some((
        build.namespace().unwrap_or_default(),
        webhook.secret_ref.name.clone(),
        webhook.secret_ref.key.clone(),
    ))
}

/// The Secret keys to read for `candidates`, at most [`MAX_SECRETS`]. If there are more, the
/// keys start at `turn`, so each delivery reads the next keys and no `Build` is left out every
/// time.
fn secrets_to_read(candidates: &[Arc<Build>], turn: usize) -> Vec<SecretKey> {
    let mut keys: Vec<SecretKey> = candidates.iter().filter_map(|b| secret_key(b)).collect();
    keys.sort();
    keys.dedup();
    if keys.len() > MAX_SECRETS {
        tracing::warn!(
            secrets = keys.len(),
            read = MAX_SECRETS,
            "a delivery concerns more webhook Secrets than it reads; the others wait for their poll"
        );
        let start = turn % keys.len();
        keys.rotate_left(start);
        keys.truncate(MAX_SECRETS);
    }
    keys
}

/// Whether a push of `refs` concerns `build`.
pub(crate) fn pushed(build: &Build, refs: &[String]) -> bool {
    let resolved = build
        .status
        .as_ref()
        .and_then(|s| s.resolved_ref.as_deref());
    let Some(git) = &build.spec.source.git else {
        return false;
    };
    refs.iter()
        .any(|r| hook::ref_matches(&git.git_ref, resolved, r))
}

impl Receiver {
    /// A receiver that reads and wakes through `lookup`.
    pub(crate) fn new(lookup: Box<dyn Lookup>, settings: Settings) -> Self {
        Self {
            bucket: Mutex::new(Bucket::new(settings.rate, settings.burst)),
            seen: Mutex::new(Seen::default()),
            turn: AtomicUsize::new(0),
            lookup,
            settings,
        }
    }

    /// Answers one request.
    pub(crate) async fn handle<B>(&self, request: Request<B>) -> Response<Full<Bytes>>
    where
        B: Body<Data = Bytes> + Send,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let path = request.uri().path();
        if path == "/healthz" {
            return reply(StatusCode::OK, "ok");
        }
        let Some(provider) = path
            .strip_prefix(self.settings.prefix.trim_end_matches('/'))
            .and_then(|p| p.strip_prefix('/'))
            .and_then(Provider::from_segment)
        else {
            return reply(StatusCode::NOT_FOUND, "not found");
        };
        if request.method() != Method::POST {
            return reply(StatusCode::METHOD_NOT_ALLOWED, "POST only");
        }
        let admitted = self
            .bucket
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take(Instant::now());
        if !admitted {
            return reply(StatusCode::TOO_MANY_REQUESTS, "too many deliveries");
        }
        let declared = request
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if declared.is_some_and(|n| n > self.settings.max_body as u64) {
            return reply(StatusCode::PAYLOAD_TOO_LARGE, "body too large");
        }
        let (parts, body) = request.into_parts();
        let body = match tokio::time::timeout(
            READ_TIMEOUT,
            Limited::new(body, self.settings.max_body).collect(),
        )
        .await
        {
            Ok(Ok(body)) => body.to_bytes(),
            Ok(Err(e)) if e.is::<http_body_util::LengthLimitError>() => {
                return reply(StatusCode::PAYLOAD_TOO_LARGE, "body too large");
            }
            Ok(Err(_)) | Err(_) => return reply(StatusCode::BAD_REQUEST, "body not read"),
        };
        let (status, text) = self
            .receive(provider, &parts.headers, &body, Timestamp::now())
            .await;
        reply(status, &text)
    }

    /// Acts on a delivery whose body has been read.
    pub(crate) async fn receive(
        &self,
        provider: Provider,
        headers: &HeaderMap,
        body: &[u8],
        now: Timestamp,
    ) -> (StatusCode, String) {
        let refused = (
            StatusCode::UNAUTHORIZED,
            "signature not accepted".to_owned(),
        );
        let event = provider.event_header().and_then(|h| header_str(headers, h));
        let delivery: Delivery = match hook::parse(provider, event, body) {
            Ok(d) => d,
            Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()),
        };
        let candidates = candidates(&self.lookup.builds(), provider, &delivery.repositories);
        let verified: Vec<Arc<Build>> = if provider == Provider::GitHubApp {
            match self.lookup.app_secret().await {
                Some(secret) if verify(provider, &secret, headers, body, now) => candidates,
                _ => return refused,
            }
        } else {
            let turn = self.turn.fetch_add(MAX_SECRETS, Ordering::Relaxed);
            let mut vouching = Vec::new();
            for key in secrets_to_read(&candidates, turn) {
                let (namespace, name, item) = &key;
                let secret = self.lookup.secret(namespace, name, item).await;
                if secret.is_some_and(|s| verify(provider, &s, headers, body, now)) {
                    vouching.push(key);
                }
            }
            let verified: Vec<Arc<Build>> = candidates
                .into_iter()
                .filter(|b| secret_key(b).is_some_and(|k| vouching.contains(&k)))
                .collect();
            if verified.is_empty() {
                return refused;
            }
            verified
        };
        // Per Build: GitLab gives every webhook of one event the same id.
        let verified: Vec<Arc<Build>> =
            match header_str(headers, provider.delivery_header()).filter(|i| !i.is_empty()) {
                None => verified,
                Some(id) => {
                    let any = !verified.is_empty();
                    let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
                    let fresh: Vec<Arc<Build>> = verified
                        .into_iter()
                        .filter(|b| {
                            let namespace = b.namespace().unwrap_or_default();
                            seen.insert(format!("{provider:?}/{id}/{namespace}/{}", b.name_any()))
                        })
                        .collect();
                    drop(seen);
                    if any && fresh.is_empty() {
                        return (StatusCode::ACCEPTED, "already received".to_owned());
                    }
                    fresh
                }
            };
        if delivery.event != Event::Push {
            return (StatusCode::ACCEPTED, "ignored".to_owned());
        }
        let woken: Vec<&Arc<Build>> = verified
            .iter()
            .filter(|b| pushed(b, &delivery.refs))
            .collect();
        futures_util::stream::iter(&woken)
            .for_each_concurrent(MAX_TOUCHES, |build| async move {
                let namespace = build.namespace().unwrap_or_default();
                tracing::info!(build = %build.name_any(), %namespace, ?provider, "woken by a webhook");
                self.lookup.touch(&namespace, &build.name_any(), now).await;
            })
            .await;
        (StatusCode::ACCEPTED, format!("{} woken", woken.len()))
    }
}

/// Serves `receiver` until the process ends.
pub(crate) async fn serve(receiver: Arc<Receiver>) -> Result<(), WebhookError> {
    let addr = receiver.settings.addr;
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| WebhookError::Bind { addr, source })?;
    tracing::info!(%addr, prefix = %receiver.settings.prefix, "webhook receiver listening");
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok(permit) = Arc::clone(&slots).acquire_owned().await else {
            return Ok(());
        };
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::warn!(error = %e, "webhook accept");
                continue;
            }
        };
        let receiver = Arc::clone(&receiver);
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let receiver = Arc::clone(&receiver);
                async move { Ok::<_, Infallible>(receiver.handle(request).await) }
            });
            let served = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(READ_TIMEOUT)
                .keep_alive(false)
                .serve_connection(TokioIo::new(stream), service)
                .await;
            if let Err(e) = served {
                tracing::debug!(error = %e, "webhook connection");
            }
            drop(permit);
        });
    }
}

#[cfg(test)]
mod tests;
