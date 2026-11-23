//! Posts a notification with retries.
//!
//! Basic auth is what a `keel` target's `secretRef` gives: Keel checks it on its webhooks when
//! `BASIC_AUTH_USER` and `BASIC_AUTH_PASSWORD` are set (not verified).

use std::fmt;
use std::time::Duration;

use backon::{ExponentialBuilder, Retryable as _};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use http::{Method, Request, header};
use http_body_util::Full;
use thiserror::Error;

use crate::remote::HttpClient;

const TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = concat!("zeabur-builder/", env!("CARGO_PKG_VERSION"));

/// Why a notification was not sent.
#[derive(Debug, Error)]
pub(crate) enum NotifyError {
    /// The run recorded no tag to send.
    #[error("the run recorded no tag")]
    NoTag,
    /// The target's Secret could not be read.
    #[error("cannot read Secret `{name}`: {source}")]
    ReadSecret {
        name: String,
        #[source]
        source: kube::Error,
    },
    /// The target's Secret is not in the `Build`'s namespace.
    #[error("no Secret `{name}`")]
    NoSecret { name: String },
    /// The target's Secret lacks a key, or the key is not text.
    #[error("Secret `{name}` has no `{key}`")]
    SecretKey { name: String, key: &'static str },
    /// The payload does not encode.
    #[error("cannot encode the payload: {0}")]
    Payload(#[from] serde_json::Error),
    /// The request could not be built.
    #[error(transparent)]
    Request(#[from] http::Error),
    /// The target did not answer in time.
    #[error("timed out")]
    TimedOut(#[from] tokio::time::error::Elapsed),
    /// The target could not be reached.
    #[error(transparent)]
    Transport(#[from] hyper_util::client::legacy::Error),
    /// The target answered with an error.
    #[error("answered {0}")]
    Answered(http::StatusCode),
}

/// HTTP basic auth.
pub(crate) struct Credentials {
    pub(crate) username: String,
    pub(crate) password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

/// POSTs `body` as JSON to `url`, at most three times, waiting 1 s and then 4 s, each plus a
/// random part up to as long again; the last error when none got a 2xx.
pub(crate) async fn post(
    client: &HttpClient,
    url: &str,
    credentials: Option<&Credentials>,
    body: &[u8],
) -> Result<(), NotifyError> {
    (|| attempt(client, url, credentials, body))
        .retry(backoff())
        .notify(|error, wait| tracing::info!(url, %error, ?wait, "notify failed; retrying"))
        .await
}

/// Two retries after the first attempt.
fn backoff() -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_min_delay(Duration::from_secs(1))
        .with_factor(4.0)
        .with_max_times(2)
        .with_jitter()
}

async fn attempt(
    client: &HttpClient,
    url: &str,
    credentials: Option<&Credentials>,
    body: &[u8],
) -> Result<(), NotifyError> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(url)
        .header(header::USER_AGENT, USER_AGENT)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(c) = credentials {
        let token = STANDARD.encode(format!("{}:{}", c.username, c.password));
        request = request.header(header::AUTHORIZATION, format!("Basic {token}"));
    }
    let request = request.body(Full::new(Bytes::copy_from_slice(body)))?;
    let response = tokio::time::timeout(TIMEOUT, client.request(request)).await??;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(NotifyError::Answered(response.status()))
    }
}
