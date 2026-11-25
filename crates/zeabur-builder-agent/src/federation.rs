//! Workload identity federation: the run exchanges a service account token, such as the build
//! Job's projected token, for a short-lived access token, so no long-lived model key is stored.
//!
//! Each vendor is one file with a [`TokenSource`] that reads the variables its own SDKs read; an
//! API key in the vendor's key variable comes first. An access token can last less than a run,
//! so [`Renewing`] exchanges the service account token again once three quarters of the access
//! token's lifetime have passed.

mod anthropic;
mod azure;
mod openai;

use std::fmt::Debug;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::StatusCode;
use rig_core::DynModel;
use rig_core::catalog::{Catalog, ModelSpec};
use rig_core::http_client::{
    DynHttpClient, Error, HttpClientExt, LazyBody, MultipartForm, Request, Response,
    StreamingResponse,
};
use rig_core::operation::Completion;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::time::Instant;
use zeabur_builder_core::refusal::Refusal;

use crate::http::{Body, HyperClient};
use crate::plan::Reason;

/// The most of an error reply a detail keeps.
const MAX_DETAIL: usize = 1024;
/// The longest one exchange may take.
const EXCHANGE_TIME: Duration = Duration::from_secs(30);

/// A lookup of non-empty variables.
type Vars<'a> = dyn Fn(&str) -> Option<String> + 'a;

/// Reads a vendor's variables: `None` when its API key is set or its federation is not.
type Read = fn(&Vars<'_>) -> Result<Option<Arc<dyn TokenSource>>, Refusal<Reason>>;

/// The vendors with federation, by Rig's vendor name.
const SOURCES: [(&str, Read); 3] = [
    ("anthropic", anthropic::read),
    ("azure.openai", azure::read),
    ("openai", openai::read),
];

/// A vendor's token endpoint and the model it unlocks.
pub(crate) trait TokenSource: Debug + Send + Sync {
    /// The vendor, for messages.
    fn name(&self) -> &'static str;
    /// The token endpoint.
    fn url(&self) -> &str;
    /// The service account token.
    fn token_file(&self) -> &Path;
    /// The request that exchanges `jwt`.
    fn request(&self, jwt: &str) -> Body;
    /// What the user checks when the vendor refuses the service account token.
    fn check(&self) -> &'static str;
    /// The model `spec` names, sending `token` through `http`.
    fn model(
        &self,
        token: String,
        catalog: &Catalog,
        spec: &ModelSpec,
        http: DynHttpClient,
        base_url: Option<&str>,
    ) -> Result<DynModel<Completion>, Refusal<Reason>>;
}

/// The federation `vendor`'s variables set up, from the process environment.
pub(crate) fn from_env(vendor: &str) -> Result<Option<Arc<dyn TokenSource>>, Refusal<Reason>> {
    from_vars(vendor, |name| std::env::var(name).ok())
}

fn from_vars(
    vendor: &str,
    var: impl Fn(&str) -> Option<String>,
) -> Result<Option<Arc<dyn TokenSource>>, Refusal<Reason>> {
    let var = |name: &str| var(name).filter(|value| !value.is_empty());
    SOURCES
        .iter()
        .find(|(name, _)| *name == vendor)
        .map_or(Ok(None), |(_, read)| read(&var))
}

/// A variable a vendor's federation needs once it is set up.
fn required(var: &Vars<'_>, name: &str) -> Result<String, Refusal<Reason>> {
    var(name).ok_or_else(|| Refusal::new(Reason::ModelKeyMissing, format!("set {name}")))
}

/// The model `spec` names, with an access token exchanged for the service account token.
pub(crate) async fn connect(
    source: Arc<dyn TokenSource>,
    catalog: &Catalog,
    spec: &ModelSpec,
    http: HyperClient,
    base_url: Option<&str>,
) -> Result<DynModel<Completion>, Refusal<Reason>> {
    let access = exchange(source.as_ref(), &http, source.url(), EXCHANGE_TIME).await?;
    let token = access.token.clone();
    let renewing = Renewing {
        source: Arc::clone(&source),
        http,
        access: Arc::new(Mutex::new(access)),
    };
    source.model(token, catalog, spec, DynHttpClient::new(renewing), base_url)
}

/// An access token for the service account token, from the token endpoint at `url`, in at most
/// `limit`.
async fn exchange(
    source: &dyn TokenSource,
    http: &HyperClient,
    url: &str,
    limit: Duration,
) -> Result<Access, Refusal<Reason>> {
    let token_file = source.token_file();
    let jwt = tokio::fs::read_to_string(token_file).await.map_err(|e| {
        Refusal::new(
            Reason::ModelKeyMissing,
            format!(
                "cannot read the service account token {}: {e}",
                token_file.display()
            ),
        )
    })?;
    let started = Instant::now();
    let reply = tokio::time::timeout(limit, http.post(url, source.request(jwt.trim())))
        .await
        .map_err(|_elapsed| {
            Refusal::new(
                Reason::ModelFailed,
                format!("{url} did not answer in {} s", limit.as_secs()),
            )
        })?;
    let (status, reply) =
        reply.map_err(|e| Refusal::new(Reason::ModelFailed, format!("{url}: {e}")))?;
    let token = access_token(source, status, &reply)?;
    Ok(Access {
        renew_at: started + Duration::from_secs(token.expires_in.saturating_mul(3) / 4),
        token: token.access_token,
    })
}

/// The access token in a reply, or what the user checks.
fn access_token(
    source: &dyn TokenSource,
    status: StatusCode,
    reply: &[u8],
) -> Result<Token, Refusal<Reason>> {
    let vendor = source.name();
    if status.is_success() {
        return serde_json::from_slice::<Token>(reply).map_err(|e| {
            Refusal::new(Reason::ModelFailed, format!("{vendor}'s token reply: {e}"))
        });
    }
    let text = String::from_utf8_lossy(reply);
    let end = text
        .char_indices()
        .nth(MAX_DETAIL)
        .map_or(text.len(), |(i, _)| i);
    let text = text.get(..end).unwrap_or_default().trim();
    let detail = format!("{vendor} refused the service account token ({status}): {text}");
    if status.is_client_error() {
        Err(Refusal::new(
            Reason::ModelKeyMissing,
            format!("{detail}; {}", source.check()),
        ))
    } else {
        Err(Refusal::new(Reason::ModelFailed, detail))
    }
}

/// The part of an RFC 6749 token reply the run uses.
#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Token {
    access_token: String,
    /// Seconds the access token lasts.
    expires_in: u64,
}

/// An access token and when to exchange again.
#[derive(Debug)]
struct Access {
    token: String,
    renew_at: Instant,
}

/// Sends model requests with an access token that it renews before it expires.
#[derive(Debug, Clone)]
struct Renewing {
    source: Arc<dyn TokenSource>,
    http: HyperClient,
    access: Arc<Mutex<Access>>,
}

impl Renewing {
    /// `req` with the current access token as its bearer token.
    #[expect(
        clippy::result_large_err,
        reason = "Rig's HTTP error type is what the trait returns"
    )]
    async fn authorize<B>(&self, mut req: Request<B>) -> Result<Request<B>, Error> {
        let token = {
            let mut access = self.access.lock().await;
            if Instant::now() >= access.renew_at {
                *access = exchange(
                    self.source.as_ref(),
                    &self.http,
                    self.source.url(),
                    EXCHANGE_TIME,
                )
                .await
                .map_err(Error::instance)?;
            }
            access.token.clone()
        };
        let mut value = http::HeaderValue::try_from(format!("Bearer {token}"))?;
        value.set_sensitive(true);
        req.headers_mut().insert(http::header::AUTHORIZATION, value);
        req.headers_mut().remove("x-api-key");
        req.headers_mut().remove("api-key");
        Ok(req)
    }
}

impl HttpClientExt for Renewing {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>, Error>> + Send + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        let this = self.clone();
        let req = req.map(Into::<Bytes>::into);
        async move { this.http.send(this.authorize(req).await?).await }
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>, Error>> + Send + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        let this = self.clone();
        async move { this.http.send_multipart(this.authorize(req).await?).await }
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<StreamingResponse, Error>> + Send
    where
        T: Into<Bytes> + Send,
    {
        let this = self.clone();
        let req = req.map(Into::<Bytes>::into);
        async move { this.http.send_streaming(this.authorize(req).await?).await }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    /// A vendor whose service account token is `token_file`.
    #[derive(Debug)]
    struct Vendor {
        token_file: PathBuf,
    }

    impl TokenSource for Vendor {
        fn name(&self) -> &'static str {
            "Vendor"
        }

        fn url(&self) -> &'static str {
            "https://vendor.example/token"
        }

        fn token_file(&self) -> &Path {
            &self.token_file
        }

        fn request(&self, jwt: &str) -> Body {
            Body::json(&json!({"assertion": jwt}))
        }

        fn check(&self) -> &'static str {
            "check the vendor's rule"
        }

        fn model(
            &self,
            _token: String,
            _catalog: &Catalog,
            _spec: &ModelSpec,
            _http: DynHttpClient,
            _base_url: Option<&str>,
        ) -> Result<DynModel<Completion>, Refusal<Reason>> {
            Err(Refusal::new(Reason::ModelFailed, "no model"))
        }
    }

    fn vendor() -> Vendor {
        Vendor {
            token_file: concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/token").into(),
        }
    }

    #[test]
    fn a_vendor_without_federation_has_no_token_source() {
        let vars = |_: &str| Some("set".to_owned());
        assert!(matches!(from_vars("gcp.gemini", vars), Ok(None)));
        assert!(matches!(from_vars("anthropic", |_| None), Ok(None)));
    }

    #[test]
    fn reads_the_access_token() {
        let reply = json!({
            "access_token": "sk-ant-oat01-abc",
            "token_type": "Bearer",
            "expires_in": 600,
            "scope": "workspace:inference",
        });
        assert_eq!(
            access_token(&vendor(), StatusCode::OK, reply.to_string().as_bytes()),
            Ok(Token {
                access_token: "sk-ant-oat01-abc".to_owned(),
                expires_in: 600,
            })
        );
    }

    fn renewing(source: Vendor, renew_at: Instant) -> Renewing {
        Renewing {
            source: Arc::new(source),
            http: HyperClient::new().unwrap(),
            access: Arc::new(Mutex::new(Access {
                token: "sk-ant-oat01-abc".to_owned(),
                renew_at,
            })),
        }
    }

    #[tokio::test]
    async fn a_fresh_access_token_replaces_the_key() {
        let req = Request::post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", "placeholder")
            .header("api-key", "placeholder")
            .body(Bytes::new())
            .unwrap();
        let renewing = renewing(vendor(), Instant::now() + Duration::from_secs(60));

        let req = renewing.authorize(req).await.unwrap();

        assert_eq!(req.headers()["authorization"], "Bearer sk-ant-oat01-abc");
        assert!(req.headers().get("x-api-key").is_none());
        assert!(req.headers().get("api-key").is_none());
    }

    #[tokio::test]
    async fn an_aging_access_token_is_exchanged_again() {
        let req = Request::post("https://api.anthropic.com/v1/messages")
            .body(Bytes::new())
            .unwrap();
        let missing = Vendor {
            token_file: "/nonexistent/token".into(),
        };
        let renewing = renewing(missing, Instant::now());

        let error = renewing.authorize(req).await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("cannot read the service account token"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_silent_token_endpoint_ends_the_exchange() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());

        let result = exchange(
            &vendor(),
            &HyperClient::new().unwrap(),
            &url,
            Duration::from_millis(200),
        )
        .await;
        drop(listener);

        let Err(Refusal {
            reason,
            message: detail,
        }) = result
        else {
            panic!("exchanged");
        };
        assert_eq!(reason, Reason::ModelFailed);
        assert!(detail.contains("did not answer"), "{detail}");
    }

    #[test]
    fn a_refused_token_says_what_to_check() {
        let Err(Refusal {
            reason,
            message: detail,
        }) = access_token(
            &vendor(),
            StatusCode::UNAUTHORIZED,
            b"Authentication failed",
        )
        else {
            panic!("accepted");
        };
        assert_eq!(reason, Reason::ModelKeyMissing);
        assert!(detail.contains("401 Unauthorized"), "{detail}");
        assert!(detail.contains("Authentication failed"), "{detail}");
        assert!(detail.contains("the vendor's rule"), "{detail}");

        let Err(Refusal { reason, .. }) = access_token(&vendor(), StatusCode::BAD_GATEWAY, b"")
        else {
            panic!("accepted");
        };
        assert_eq!(reason, Reason::ModelFailed);
    }
}
