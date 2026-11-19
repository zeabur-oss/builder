//! Workload identity federation: the run exchanges a service account token, such as the build
//! Job's projected token, for a short-lived access token, so no long-lived model key is stored.
//!
//! Only Anthropic and OpenAI have it. Each reads the variables its own SDKs read, and an API key
//! in the vendor's key variable comes first.
//!
//! - Anthropic: `POST https://api.anthropic.com/v1/oauth/token` with the RFC 7523 `jwt-bearer`
//!   grant. The access token goes in `Authorization: Bearer`, not `x-api-key` ([guide],
//!   [reference]).
//! - OpenAI: `POST https://auth.openai.com/oauth/token` with the RFC 8693 token exchange grant
//!   ([OpenAI guide], [OpenAI reference]). The access token takes the API key's place.
//!
//! An access token can last less than a run, so [`Federated`] exchanges the service account token
//! again once three quarters of the access token's lifetime have passed.
//!
//! [guide]: https://platform.claude.com/docs/en/manage-claude/workload-identity-federation
//! [reference]: https://platform.claude.com/docs/en/manage-claude/wif-reference
//! [OpenAI guide]: https://developers.openai.com/api/docs/guides/workload-identity-federation
//! [OpenAI reference]: https://developers.openai.com/api/reference/workload-identity-federation

use std::path::PathBuf;
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
use rig_core::providers::anthropic::wire::{ANTHROPIC, AnthropicConfig, Auth};
use rig_core::providers::registry::{ConnectOptions, ProviderConfig};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::http::HyperClient;
use crate::plan::Reason;

const ANTHROPIC_TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// The most of an error reply a detail keeps.
const MAX_DETAIL: usize = 1024;
/// The longest one exchange may take.
const EXCHANGE_TIME: Duration = Duration::from_secs(30);

/// How a run gets its access token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Federation {
    /// Anthropic's `ANTHROPIC_*` variables.
    Anthropic {
        /// `ANTHROPIC_FEDERATION_RULE_ID`, `fdrl_…`.
        rule: String,
        /// `ANTHROPIC_ORGANIZATION_ID`.
        organization: String,
        /// `ANTHROPIC_WORKSPACE_ID`, `wrkspc_…`, needed when the rule has several workspaces.
        workspace: Option<String>,
        /// `ANTHROPIC_SERVICE_ACCOUNT_ID`, `svac_…`; the rule names one already.
        service_account: Option<String>,
        /// `ANTHROPIC_IDENTITY_TOKEN_FILE`.
        token_file: PathBuf,
    },
    /// OpenAI's `OPENAI_*` variables.
    OpenAi {
        /// `OPENAI_IDENTITY_PROVIDER_ID`.
        provider: String,
        /// `OPENAI_SERVICE_ACCOUNT_ID`.
        service_account: String,
        /// `OPENAI_IDENTITY_TOKEN_FILE`.
        token_file: PathBuf,
    },
}

impl Federation {
    /// The federation `vendor`'s variables set up, from the process environment.
    pub(crate) fn from_env(vendor: &str) -> Result<Option<Self>, (Reason, String)> {
        Self::from_vars(vendor, |name| std::env::var(name).ok())
    }

    /// The federation `vendor`'s variables set up, or `None` when its API key is set or its
    /// first federation variable is not.
    fn from_vars(
        vendor: &str,
        var: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, (Reason, String)> {
        let var = |name: &str| var(name).filter(|value| !value.is_empty());
        let required =
            |name: &str| var(name).ok_or_else(|| (Reason::ModelKeyMissing, format!("set {name}")));
        match vendor {
            "anthropic" if var("ANTHROPIC_API_KEY").is_none() => {
                let Some(rule) = var("ANTHROPIC_FEDERATION_RULE_ID") else {
                    return Ok(None);
                };
                Ok(Some(Self::Anthropic {
                    rule,
                    organization: required("ANTHROPIC_ORGANIZATION_ID")?,
                    workspace: var("ANTHROPIC_WORKSPACE_ID"),
                    service_account: var("ANTHROPIC_SERVICE_ACCOUNT_ID"),
                    token_file: required("ANTHROPIC_IDENTITY_TOKEN_FILE")?.into(),
                }))
            }
            "openai" if var("OPENAI_API_KEY").is_none() => {
                let Some(provider) = var("OPENAI_IDENTITY_PROVIDER_ID") else {
                    return Ok(None);
                };
                Ok(Some(Self::OpenAi {
                    provider,
                    service_account: required("OPENAI_SERVICE_ACCOUNT_ID")?,
                    token_file: required("OPENAI_IDENTITY_TOKEN_FILE")?.into(),
                }))
            }
            _ => Ok(None),
        }
    }

    /// The model `spec` names, with an access token exchanged for the service account token.
    pub(crate) async fn connect(
        &self,
        catalog: &Catalog,
        spec: &ModelSpec,
        http: HyperClient,
        base_url: Option<&str>,
    ) -> Result<DynModel<Completion>, (Reason, String)> {
        let access = self.exchange(&http, token_url(self), EXCHANGE_TIME).await?;
        let token = access.token.clone();
        let client = Federated {
            federation: Arc::new(self.clone()),
            http,
            access: Arc::new(Mutex::new(access)),
        };
        self.model(token, catalog, spec, client, base_url)
    }

    /// An access token for the service account token, from the token endpoint at `url`, in at
    /// most `limit`.
    async fn exchange(
        &self,
        http: &HyperClient,
        url: &str,
        limit: Duration,
    ) -> Result<Access, (Reason, String)> {
        let token_file = match self {
            Self::Anthropic { token_file, .. } | Self::OpenAi { token_file, .. } => token_file,
        };
        let jwt = tokio::fs::read_to_string(token_file).await.map_err(|e| {
            (
                Reason::ModelKeyMissing,
                format!(
                    "cannot read the service account token {}: {e}",
                    token_file.display()
                ),
            )
        })?;
        let started = Instant::now();
        let reply = tokio::time::timeout(limit, http.post_json(url, self.request(jwt.trim())))
            .await
            .map_err(|_elapsed| {
                (
                    Reason::ModelFailed,
                    format!("{url} did not answer in {} s", limit.as_secs()),
                )
            })?;
        let (status, reply) = reply.map_err(|e| (Reason::ModelFailed, format!("{url}: {e}")))?;
        let token = self.access_token(status, &reply)?;
        Ok(Access {
            renew_at: started + Duration::from_secs(token.expires_in.saturating_mul(3) / 4),
            token: token.access_token,
        })
    }

    /// The model `spec` names, sending `token` as the credential.
    fn model(
        &self,
        token: String,
        catalog: &Catalog,
        spec: &ModelSpec,
        http: impl HttpClientExt + 'static,
        base_url: Option<&str>,
    ) -> Result<DynModel<Completion>, (Reason, String)> {
        match self {
            Self::Anthropic { .. } => {
                // Rig's Anthropic dialect sends its key as `x-api-key`; an access token is a
                // bearer token.
                let mut dialect = ANTHROPIC;
                dialect.quirks.auth = Auth::OptionalBearer;
                let mut config =
                    ProviderConfig::Anthropic(AnthropicConfig::with_key(&dialect, token));
                if let Some(base_url) = base_url {
                    config = config.with_base_url(base_url);
                }
                let facts = catalog.facts(spec.provider, &spec.id);
                Ok(config.completion_model(&spec.id, DynHttpClient::new(http), facts))
            }
            Self::OpenAi { .. } => {
                let mut options = ConnectOptions::new().http(http).api_key(token);
                if let Some(base_url) = base_url {
                    options = options.base_url(base_url);
                }
                catalog
                    .connect_with(spec, options)
                    .map_err(|e| (Reason::ModelFailed, e.to_string()))
            }
        }
    }

    /// The JSON body that exchanges `jwt`.
    fn request(&self, jwt: &str) -> Vec<u8> {
        let body = match self {
            Self::Anthropic {
                rule,
                organization,
                workspace,
                service_account,
                ..
            } => serde_json::to_vec(&JwtBearer {
                grant_type: "urn:ietf:params:oauth:grant-type:jwt-bearer",
                assertion: jwt,
                federation_rule_id: rule,
                organization_id: organization,
                workspace_id: workspace.as_deref(),
                service_account_id: service_account.as_deref(),
            }),
            Self::OpenAi {
                provider,
                service_account,
                ..
            } => serde_json::to_vec(&TokenExchange {
                grant_type: "urn:ietf:params:oauth:grant-type:token-exchange",
                subject_token_type: "urn:ietf:params:oauth:token-type:jwt",
                subject_token: jwt,
                identity_provider_id: provider,
                service_account_id: service_account,
            }),
        };
        // Strings only, so serialization cannot fail.
        body.unwrap_or_default()
    }

    /// The access token in a reply, or what the user checks.
    fn access_token(&self, status: StatusCode, reply: &[u8]) -> Result<Token, (Reason, String)> {
        let (vendor, check) = match self {
            Self::Anthropic { .. } => (
                "Anthropic",
                "check the federation rule, its audience and the issuer's keys; the Console's \
                 authentication history has the reason",
            ),
            Self::OpenAi { .. } => (
                "OpenAI",
                "check the identity provider, its audience, its uploaded keys and the service \
                 account's mapping",
            ),
        };
        if status.is_success() {
            return serde_json::from_slice::<Token>(reply)
                .map_err(|e| (Reason::ModelFailed, format!("{vendor}'s token reply: {e}")));
        }
        let text = String::from_utf8_lossy(reply);
        let end = text
            .char_indices()
            .nth(MAX_DETAIL)
            .map_or(text.len(), |(i, _)| i);
        let text = text.get(..end).unwrap_or_default().trim();
        let detail = format!("{vendor} refused the service account token ({status}): {text}");
        if status.is_client_error() {
            Err((Reason::ModelKeyMissing, format!("{detail}; {check}")))
        } else {
            Err((Reason::ModelFailed, detail))
        }
    }
}

/// Anthropic's RFC 7523 request.
#[derive(Debug, Serialize)]
struct JwtBearer<'a> {
    grant_type: &'static str,
    assertion: &'a str,
    federation_rule_id: &'a str,
    organization_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_account_id: Option<&'a str>,
}

/// OpenAI's RFC 8693 request.
#[derive(Debug, Serialize)]
struct TokenExchange<'a> {
    grant_type: &'static str,
    subject_token_type: &'static str,
    subject_token: &'a str,
    identity_provider_id: &'a str,
    service_account_id: &'a str,
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
struct Federated {
    federation: Arc<Federation>,
    http: HyperClient,
    access: Arc<Mutex<Access>>,
}

impl Federated {
    /// `req` with the current access token as its bearer token.
    #[expect(
        clippy::result_large_err,
        reason = "Rig's HTTP error type is what the trait returns"
    )]
    async fn authorize<B>(&self, mut req: Request<B>) -> Result<Request<B>, Error> {
        let token = {
            let mut access = self.access.lock().await;
            if Instant::now() >= access.renew_at {
                let url = token_url(&self.federation);
                *access = self
                    .federation
                    .exchange(&self.http, url, EXCHANGE_TIME)
                    .await
                    .map_err(|(_, detail)| Error::instance(std::io::Error::other(detail)))?;
            }
            access.token.clone()
        };
        let mut value = http::HeaderValue::try_from(format!("Bearer {token}"))?;
        value.set_sensitive(true);
        req.headers_mut().insert(http::header::AUTHORIZATION, value);
        req.headers_mut().remove("x-api-key");
        Ok(req)
    }
}

/// The token endpoint of `federation`.
const fn token_url(federation: &Federation) -> &'static str {
    match federation {
        Federation::Anthropic { .. } => ANTHROPIC_TOKEN_URL,
        Federation::OpenAi { .. } => OPENAI_TOKEN_URL,
    }
}

impl HttpClientExt for Federated {
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
    use std::collections::HashMap;

    use pretty_assertions::assert_eq;
    use rig_core::completion::CompletionRequest;
    use rig_core::message::Message;
    use serde_json::{Value, json};

    use super::*;
    use crate::testing::{Replay, text};

    fn from(vendor: &str, vars: &[(&str, &str)]) -> Result<Option<Federation>, (Reason, String)> {
        let vars: HashMap<_, _> = vars.iter().copied().collect();
        Federation::from_vars(vendor, |name| vars.get(name).map(|v| (*v).to_owned()))
    }

    fn anthropic() -> Federation {
        from(
            "anthropic",
            &[
                ("ANTHROPIC_FEDERATION_RULE_ID", "fdrl_1"),
                ("ANTHROPIC_ORGANIZATION_ID", "org"),
                ("ANTHROPIC_WORKSPACE_ID", "wrkspc_1"),
                (
                    "ANTHROPIC_IDENTITY_TOKEN_FILE",
                    "/var/run/secrets/anthropic.com/token",
                ),
            ],
        )
        .unwrap()
        .unwrap()
    }

    fn openai() -> Federation {
        from(
            "openai",
            &[
                ("OPENAI_IDENTITY_PROVIDER_ID", "idp_1"),
                ("OPENAI_SERVICE_ACCOUNT_ID", "svc_1"),
                (
                    "OPENAI_IDENTITY_TOKEN_FILE",
                    "/var/run/secrets/tokens/token",
                ),
            ],
        )
        .unwrap()
        .unwrap()
    }

    fn body(federation: &Federation) -> (&'static str, Value) {
        let body = federation.request("jwt");
        (
            token_url(federation),
            serde_json::from_slice(&body).unwrap(),
        )
    }

    #[test]
    fn anthropic_takes_a_jwt_bearer_grant() {
        assert_eq!(
            body(&anthropic()),
            (
                ANTHROPIC_TOKEN_URL,
                json!({
                    "grant_type": "urn:ietf:params:oauth:grant-type:jwt-bearer",
                    "assertion": "jwt",
                    "federation_rule_id": "fdrl_1",
                    "organization_id": "org",
                    "workspace_id": "wrkspc_1",
                })
            )
        );
    }

    #[test]
    fn openai_takes_a_token_exchange() {
        assert_eq!(
            body(&openai()),
            (
                OPENAI_TOKEN_URL,
                json!({
                    "grant_type": "urn:ietf:params:oauth:grant-type:token-exchange",
                    "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
                    "subject_token": "jwt",
                    "identity_provider_id": "idp_1",
                    "service_account_id": "svc_1",
                })
            )
        );
    }

    #[test]
    fn an_api_key_comes_first() {
        let vars = [
            ("ANTHROPIC_API_KEY", "sk-ant"),
            ("ANTHROPIC_FEDERATION_RULE_ID", "fdrl_1"),
        ];
        assert_eq!(from("anthropic", &vars), Ok(None));
        assert_eq!(from("gcp.gemini", &vars), Ok(None));
        assert_eq!(from("openai", &[]), Ok(None));
    }

    #[test]
    fn a_missing_variable_is_named() {
        let vars = [
            ("OPENAI_IDENTITY_PROVIDER_ID", "idp_1"),
            ("OPENAI_SERVICE_ACCOUNT_ID", "svc_1"),
        ];
        assert_eq!(
            from("openai", &vars),
            Err((
                Reason::ModelKeyMissing,
                "set OPENAI_IDENTITY_TOKEN_FILE".to_owned()
            ))
        );
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
            anthropic().access_token(StatusCode::OK, reply.to_string().as_bytes()),
            Ok(Token {
                access_token: "sk-ant-oat01-abc".to_owned(),
                expires_in: 600,
            })
        );
        let reply = json!({
            "access_token": "eyJ",
            "issued_token_type": "urn:ietf:params:oauth:token-type:access_token",
            "token_type": "Bearer",
            "expires_in": 3600,
            "expires_at": 1_800_000_000,
        });
        assert_eq!(
            openai().access_token(StatusCode::OK, reply.to_string().as_bytes()),
            Ok(Token {
                access_token: "eyJ".to_owned(),
                expires_in: 3600,
            })
        );
    }

    #[tokio::test]
    async fn anthropic_sends_the_access_token_as_a_bearer_token() {
        let replay = Replay::new([text("1", "hi")]);
        let catalog = Catalog::builtin();
        let spec = catalog.resolve("anthropic/claude-opus-5-5").unwrap().spec;
        let model = anthropic()
            .model(
                "sk-ant-oat01-abc".to_owned(),
                catalog,
                spec,
                replay.clone(),
                None,
            )
            .unwrap();

        model
            .call(CompletionRequest::from(vec![Message::user("hi")]))
            .await
            .unwrap();

        let headers = replay.headers.lock().unwrap().clone();
        assert_eq!(headers[0]["authorization"], "Bearer sk-ant-oat01-abc");
        assert!(headers[0].get("x-api-key").is_none());
    }

    fn federated(renew_at: Instant) -> Federated {
        Federated {
            federation: Arc::new(anthropic()),
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
            .body(Bytes::new())
            .unwrap();
        let federated = federated(Instant::now() + Duration::from_secs(60));

        let req = federated.authorize(req).await.unwrap();

        assert_eq!(req.headers()["authorization"], "Bearer sk-ant-oat01-abc");
        assert!(req.headers().get("x-api-key").is_none());
    }

    #[tokio::test]
    async fn an_aging_access_token_is_exchanged_again() {
        let req = Request::post("https://api.anthropic.com/v1/messages")
            .body(Bytes::new())
            .unwrap();
        let federated = federated(Instant::now());

        // The token file does not exist here, so the renewal fails while reading it.
        let error = federated.authorize(req).await.unwrap_err();

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
        let federation = Federation::OpenAi {
            provider: "idp_1".to_owned(),
            service_account: "svc_1".to_owned(),
            token_file: concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/token").into(),
        };

        let result = federation
            .exchange(
                &HyperClient::new().unwrap(),
                &url,
                Duration::from_millis(200),
            )
            .await;
        drop(listener);

        let Err((reason, detail)) = result else {
            panic!("exchanged");
        };
        assert_eq!(reason, Reason::ModelFailed);
        assert!(detail.contains("did not answer"), "{detail}");
    }

    #[test]
    fn a_refused_token_says_what_to_check() {
        let Err((reason, detail)) =
            anthropic().access_token(StatusCode::UNAUTHORIZED, b"Authentication failed")
        else {
            panic!("accepted");
        };
        assert_eq!(reason, Reason::ModelKeyMissing);
        assert!(detail.contains("401 Unauthorized"), "{detail}");
        assert!(detail.contains("Authentication failed"), "{detail}");
        assert!(detail.contains("authentication history"), "{detail}");

        let Err((reason, _)) = openai().access_token(StatusCode::BAD_GATEWAY, b"") else {
            panic!("accepted");
        };
        assert_eq!(reason, Reason::ModelFailed);
    }
}
