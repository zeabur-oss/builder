//! Azure OpenAI: the client credentials grant with a federated credential at
//! `POST https://login.microsoftonline.com/<tenant>/oauth2/v2.0/token` ([grant]). The service
//! account token is the `client_assertion`; the Microsoft Entra access token goes in
//! `Authorization: Bearer`, not `api-key` ([Azure OpenAI]).
//!
//! [grant]: https://learn.microsoft.com/en-us/entra/identity-platform/v2-oauth2-client-creds-grant-flow
//! [Azure OpenAI]: https://learn.microsoft.com/en-us/azure/ai-foundry/openai/how-to/managed-identity

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rig_core::DynModel;
use rig_core::catalog::{Catalog, ModelSpec};
use rig_core::http_client::DynHttpClient;
use rig_core::operation::Completion;
use rig_core::providers::registry::ConnectOptions;
use zeabur_builder_core::refusal::Refusal;

use super::{TokenSource, Vars, required};
use crate::http::Body;
use crate::plan::Reason;

/// Microsoft Entra in the public cloud.
const AUTHORITY: &str = "https://login.microsoftonline.com/";
/// The OAuth 2.0 scope of the Azure OpenAI data plane ([REST reference]).
///
/// [REST reference]: https://learn.microsoft.com/en-us/rest/api/microsoft-foundry/azureopenai/chat
const SCOPE: &str = "https://cognitiveservices.azure.com/.default";

/// The `AZURE_*` variables of the Azure SDKs and azure-workload-identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Azure {
    /// The token endpoint of `AZURE_TENANT_ID` at `AZURE_AUTHORITY_HOST`.
    url: String,
    /// `AZURE_CLIENT_ID`: the app registration or the user-assigned managed identity.
    client: String,
    /// `AZURE_FEDERATED_TOKEN_FILE`.
    token_file: PathBuf,
}

/// The federation, or `None` when `AZURE_API_KEY` or `AZURE_TOKEN` is set or the token file is
/// not.
pub(super) fn read(var: &Vars<'_>) -> Result<Option<Arc<dyn TokenSource>>, Refusal<Reason>> {
    Ok(from(var)?.map(|source| -> Arc<dyn TokenSource> { Arc::new(source) }))
}

fn from(var: &Vars<'_>) -> Result<Option<Azure>, Refusal<Reason>> {
    if var("AZURE_API_KEY").is_some() || var("AZURE_TOKEN").is_some() {
        return Ok(None);
    }
    let Some(token_file) = var("AZURE_FEDERATED_TOKEN_FILE") else {
        return Ok(None);
    };
    let tenant = required(var, "AZURE_TENANT_ID")?;
    let authority = var("AZURE_AUTHORITY_HOST").unwrap_or_else(|| AUTHORITY.to_owned());
    Ok(Some(Azure {
        url: format!(
            "{}/{tenant}/oauth2/v2.0/token",
            authority.trim_end_matches('/')
        ),
        client: required(var, "AZURE_CLIENT_ID")?,
        token_file: token_file.into(),
    }))
}

impl TokenSource for Azure {
    fn name(&self) -> &'static str {
        "Microsoft Entra"
    }

    fn url(&self) -> &str {
        &self.url
    }

    fn token_file(&self) -> &Path {
        &self.token_file
    }

    fn request(&self, jwt: &str) -> Body {
        Body::form([
            ("grant_type", "client_credentials"),
            ("client_id", self.client.as_str()),
            ("scope", SCOPE),
            (
                "client_assertion_type",
                "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
            ),
            ("client_assertion", jwt),
        ])
    }

    fn check(&self) -> &'static str {
        "check the federated identity credential's issuer, subject and audience, and that the \
         issuer's discovery document is reachable from the internet"
    }

    fn model(
        &self,
        token: String,
        catalog: &Catalog,
        spec: &ModelSpec,
        http: DynHttpClient,
        base_url: Option<&str>,
    ) -> Result<DynModel<Completion>, Refusal<Reason>> {
        let mut options = ConnectOptions::new().http(http).api_key(token);
        if let Some(base_url) = base_url {
            options = options.base_url(base_url);
        }
        catalog
            .connect_with(spec, options)
            .map_err(|e| Refusal::new(Reason::ModelFailed, e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::convert::Infallible;
    use std::sync::Mutex;

    use bytes::Bytes;
    use http_body_util::{BodyExt as _, Full};
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use pretty_assertions::assert_eq;
    use rig_core::completion::CompletionRequest;
    use rig_core::message::Message;

    use super::*;
    use crate::http::HyperClient;

    /// A request the fake endpoints got.
    #[derive(Debug, Clone)]
    struct Seen {
        path: String,
        authorization: Option<String>,
        api_key: Option<String>,
        body: String,
    }

    /// Microsoft Entra and an Azure OpenAI resource on one local port: the token endpoint answers
    /// `token_reply` with `token_status`, every other path a chat completion.
    async fn fake_azure(
        token_status: u16,
        token_reply: &'static str,
    ) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let log = Arc::clone(&log);
                let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let log = Arc::clone(&log);
                    async move {
                        let header = |name: &str| {
                            req.headers()
                                .get(name)
                                .map(|v| v.to_str().unwrap().to_owned())
                        };
                        let mut seen = Seen {
                            path: req.uri().to_string(),
                            authorization: header("authorization"),
                            api_key: header("api-key"),
                            body: String::new(),
                        };
                        let body = req.into_body().collect().await.unwrap().to_bytes();
                        seen.body = String::from_utf8_lossy(&body).into_owned();
                        let (status, reply) = if seen.path.ends_with("/oauth2/v2.0/token") {
                            (token_status, token_reply)
                        } else {
                            (200, CHAT_REPLY)
                        };
                        log.lock().unwrap().push(seen);
                        Ok::<_, Infallible>(
                            hyper::Response::builder()
                                .status(status)
                                .header("content-type", "application/json")
                                .body(Full::new(Bytes::from_static(reply.as_bytes())))
                                .unwrap(),
                        )
                    }
                });
                tokio::spawn(
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service),
                );
            }
        });
        (url, seen)
    }

    const CHAT_REPLY: &str = r#"{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"gpt-4o","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;

    /// The source for a tenant at the fake authority `url`, with the test token file.
    fn azure_at(url: &str) -> Azure {
        from_vars(&[
            ("AZURE_TENANT_ID", "tenant-1"),
            ("AZURE_CLIENT_ID", "client-1"),
            (
                "AZURE_FEDERATED_TOKEN_FILE",
                concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/token"),
            ),
            ("AZURE_AUTHORITY_HOST", url),
        ])
        .unwrap()
        .unwrap()
    }

    async fn connect_at(url: &str) -> Result<DynModel<Completion>, Refusal<Reason>> {
        let catalog = Catalog::builtin();
        let provider = rig_core::providers::registry::ProviderId::catalog("azure.openai").unwrap();
        let spec = ModelSpec::new(provider, "gpt-4o");
        super::super::connect(
            Arc::new(azure_at(url)),
            catalog,
            &spec,
            HyperClient::new().unwrap(),
            Some(url),
        )
        .await
    }

    #[tokio::test]
    async fn the_model_call_carries_the_entra_token_as_bearer() {
        let (url, seen) = fake_azure(
            200,
            r#"{"token_type":"Bearer","expires_in":3599,"access_token":"entra-token"}"#,
        )
        .await;

        let model = connect_at(&url).await.unwrap();
        model
            .call(CompletionRequest::from(vec![Message::user("hi")]))
            .await
            .unwrap();

        let seen = seen.lock().unwrap().clone();
        let [token, chat] = seen.as_slice() else {
            panic!("{seen:?}");
        };
        assert_eq!(token.path, "/tenant-1/oauth2/v2.0/token");
        assert!(
            token.body.contains("client_assertion=jwt"),
            "{}",
            token.body
        );
        assert!(
            chat.path
                .starts_with("/openai/deployments/gpt-4o/chat/completions?api-version="),
            "{}",
            chat.path
        );
        assert_eq!(chat.authorization.as_deref(), Some("Bearer entra-token"));
        assert_eq!(chat.api_key, None);
    }

    #[tokio::test]
    async fn a_refused_token_says_what_to_check() {
        let (url, _) = fake_azure(
            400,
            r#"{"error":"invalid_client","error_description":"AADSTS700213: No matching federated identity record found"}"#,
        )
        .await;

        let Err(Refusal { reason, message }) = connect_at(&url).await else {
            panic!("connected");
        };

        assert_eq!(reason, Reason::ModelKeyMissing);
        assert!(message.contains("AADSTS700213"), "{message}");
        assert!(
            message.contains("federated identity credential"),
            "{message}"
        );
    }

    fn from_vars(vars: &[(&str, &str)]) -> Result<Option<Azure>, Refusal<Reason>> {
        let vars: HashMap<_, _> = vars.iter().copied().collect();
        from(&|name| vars.get(name).map(|v| (*v).to_owned()))
    }

    const WORKLOAD: [(&str, &str); 3] = [
        ("AZURE_TENANT_ID", "tenant-1"),
        ("AZURE_CLIENT_ID", "client-1"),
        (
            "AZURE_FEDERATED_TOKEN_FILE",
            "/var/run/secrets/tokens/token",
        ),
    ];

    #[test]
    fn takes_a_client_credentials_grant_with_the_token_as_assertion() {
        let azure = from_vars(&WORKLOAD).unwrap().unwrap();
        assert_eq!(
            azure.url(),
            "https://login.microsoftonline.com/tenant-1/oauth2/v2.0/token"
        );
        let body = azure.request("a.b+c");
        assert_eq!(body.content_type, "application/x-www-form-urlencoded");
        assert_eq!(
            String::from_utf8(body.bytes).unwrap(),
            "grant_type=client_credentials&client_id=client-1\
             &scope=https%3A%2F%2Fcognitiveservices.azure.com%2F.default\
             &client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer\
             &client_assertion=a.b%2Bc"
        );
    }

    #[test]
    fn the_authority_host_moves_the_token_endpoint() {
        let mut vars = WORKLOAD.to_vec();
        vars.push(("AZURE_AUTHORITY_HOST", "https://login.microsoftonline.us/"));
        assert_eq!(
            from_vars(&vars).unwrap().unwrap().url(),
            "https://login.microsoftonline.us/tenant-1/oauth2/v2.0/token"
        );
    }

    #[test]
    fn a_key_or_a_missing_variable_wins() {
        let mut keyed = WORKLOAD.to_vec();
        keyed.push(("AZURE_API_KEY", "key"));
        assert_eq!(from_vars(&keyed), Ok(None));
        assert_eq!(
            from_vars(&WORKLOAD[1..]),
            Err(Refusal::new(Reason::ModelKeyMissing, "set AZURE_TENANT_ID"))
        );
        assert_eq!(from_vars(&[]), Ok(None));
    }
}
