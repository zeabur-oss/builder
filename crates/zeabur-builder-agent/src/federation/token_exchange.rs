//! A token endpoint that the `ClusterBuilder` names: `POST <url>` with the service account token in
//! one form field and fixed fields, as `application/x-www-form-urlencoded` ([RFC 6749, section
//! 3.2][token-endpoint], [appendix B][form]). Microsoft Entra's client credentials with a
//! federated assertion and Google's Security Token Service take this shape. The access token
//! takes the API key's place.
//!
//! [token-endpoint]: https://www.rfc-editor.org/rfc/rfc6749#section-3.2
//! [form]: https://www.rfc-editor.org/rfc/rfc6749#appendix-B

use std::collections::BTreeMap;
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

/// The `ZB_TOKEN_EXCHANGE_*` variables the build Job sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TokenExchange {
    /// `ZB_TOKEN_EXCHANGE_URL`.
    url: String,
    /// `ZB_TOKEN_EXCHANGE_SUBJECT_FIELD`.
    subject_field: String,
    /// `ZB_TOKEN_EXCHANGE_FORM`, a JSON object of strings.
    form: BTreeMap<String, String>,
    /// `ZB_TOKEN_EXCHANGE_TOKEN_FILE`.
    token_file: PathBuf,
}

/// The token exchange, or `None` when `ZB_TOKEN_EXCHANGE_URL` is not set.
pub(super) fn read(var: &Vars<'_>) -> Result<Option<Arc<dyn TokenSource>>, Refusal<Reason>> {
    Ok(from(var)?.map(|source| -> Arc<dyn TokenSource> { Arc::new(source) }))
}

fn from(var: &Vars<'_>) -> Result<Option<TokenExchange>, Refusal<Reason>> {
    let Some(url) = var("ZB_TOKEN_EXCHANGE_URL") else {
        return Ok(None);
    };
    let subject_field = required(var, "ZB_TOKEN_EXCHANGE_SUBJECT_FIELD")?;
    let form: BTreeMap<String, String> = match var("ZB_TOKEN_EXCHANGE_FORM") {
        Some(json) => serde_json::from_str(&json).map_err(|e| {
            Refusal::new(
                Reason::ModelKeyMissing,
                format!("ZB_TOKEN_EXCHANGE_FORM is not a JSON object of strings: {e}"),
            )
        })?,
        None => BTreeMap::new(),
    };
    if form.contains_key(&subject_field) {
        return Err(Refusal::new(
            Reason::ModelKeyMissing,
            format!("ZB_TOKEN_EXCHANGE_FORM must not hold {subject_field}"),
        ));
    }
    Ok(Some(TokenExchange {
        url,
        subject_field,
        form,
        token_file: required(var, "ZB_TOKEN_EXCHANGE_TOKEN_FILE")?.into(),
    }))
}

impl TokenSource for TokenExchange {
    fn name(&self) -> &'static str {
        "the token endpoint"
    }

    fn url(&self) -> &str {
        &self.url
    }

    fn token_file(&self) -> &Path {
        &self.token_file
    }

    fn request(&self, jwt: &str) -> Body {
        let fields = std::iter::once((self.subject_field.as_str(), jwt))
            .chain(self.form.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        Body::form(fields)
    }

    fn check(&self) -> &'static str {
        "check that the token endpoint trusts the cluster's service account issuer, the token's \
         audience and subject, and the form fields"
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
    use std::time::Duration;

    use bytes::Bytes;
    use pretty_assertions::assert_eq;
    use rig_core::http_client::Request;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    use super::super::{Renewing, exchange};
    use super::*;
    use crate::http::HyperClient;

    fn from_vars(vars: &[(&str, &str)]) -> Result<Option<TokenExchange>, Refusal<Reason>> {
        let vars: HashMap<_, _> = vars.iter().copied().collect();
        from(&|name| vars.get(name).map(|v| (*v).to_owned()))
    }

    const URL: (&str, &str) = ("ZB_TOKEN_EXCHANGE_URL", "https://login.example/token");
    const FIELD: (&str, &str) = ("ZB_TOKEN_EXCHANGE_SUBJECT_FIELD", "client_assertion");
    const FILE: (&str, &str) = ("ZB_TOKEN_EXCHANGE_TOKEN_FILE", "/var/run/token");

    #[test]
    fn posts_the_token_in_its_field_with_the_fixed_fields() {
        let form = (
            "ZB_TOKEN_EXCHANGE_FORM",
            r#"{"scope":"a b/.default","grant_type":"x&y=z"}"#,
        );
        let exchange = from_vars(&[URL, FIELD, FILE, form]).unwrap().unwrap();
        let body = exchange.request("e30.e30.sig+/=");
        assert_eq!(body.content_type, "application/x-www-form-urlencoded");
        let fields: Vec<(String, String)> =
            form_urlencoded::parse(&body.bytes).into_owned().collect();
        assert_eq!(
            fields,
            [
                ("client_assertion", "e30.e30.sig+/="),
                ("grant_type", "x&y=z"),
                ("scope", "a b/.default"),
            ]
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
        );
    }

    #[test]
    fn a_form_that_sets_the_subject_field_is_refused() {
        let form = ("ZB_TOKEN_EXCHANGE_FORM", r#"{"client_assertion":"forged"}"#);
        let Err(refusal) = from_vars(&[URL, FIELD, FILE, form]) else {
            panic!("accepted");
        };
        assert_eq!(refusal.reason, Reason::ModelKeyMissing);
    }

    #[test]
    fn a_form_that_is_not_an_object_of_strings_is_refused() {
        let form = ("ZB_TOKEN_EXCHANGE_FORM", r#"{"n":1}"#);
        assert!(from_vars(&[URL, FIELD, FILE, form]).is_err());
    }

    #[test]
    fn a_missing_variable_is_named() {
        assert_eq!(
            from_vars(&[URL, FILE]),
            Err(Refusal::new(
                Reason::ModelKeyMissing,
                "set ZB_TOKEN_EXCHANGE_SUBJECT_FIELD"
            ))
        );
        assert_eq!(from_vars(&[FIELD, FILE]), Ok(None));
    }

    /// A token endpoint on 127.0.0.1 that answers each request with the next of `replies` and
    /// returns the form fields of the requests it got.
    async fn token_endpoint(
        replies: Vec<String>,
    ) -> (String, tokio::task::JoinHandle<Vec<Vec<(String, String)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for reply in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let body = loop {
                    let mut chunk = [0; 4096];
                    let n = stream.read(&mut chunk).await.unwrap();
                    request.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&request).into_owned();
                    let Some((head, body)) = text.split_once("\r\n\r\n") else {
                        continue;
                    };
                    let head = head.to_ascii_lowercase();
                    assert!(
                        head.contains("content-type: application/x-www-form-urlencoded"),
                        "{head}"
                    );
                    let length: usize = head
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if body.len() >= length {
                        break body.to_owned();
                    }
                };
                requests.push(
                    form_urlencoded::parse(body.as_bytes())
                        .into_owned()
                        .collect(),
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (url, server)
    }

    fn source(url: String) -> TokenExchange {
        TokenExchange {
            url,
            subject_field: "subject_token".to_owned(),
            form: BTreeMap::from([(
                "grant_type".to_owned(),
                "urn:ietf:params:oauth:grant-type:token-exchange".to_owned(),
            )]),
            token_file: concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/token").into(),
        }
    }

    async fn bearer(renewing: &Renewing) -> String {
        let req = Request::post("https://model.example/v1")
            .body(Bytes::new())
            .unwrap();
        let req = renewing.authorize(req).await.unwrap();
        req.headers()["authorization"].to_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn exchanges_the_token_and_renews_it_before_it_expires() {
        let (url, server) = token_endpoint(vec![
            r#"{"access_token":"at-1","token_type":"Bearer","expires_in":0}"#.to_owned(),
            r#"{"access_token":"at-2","token_type":"bearer","expires_in":3600}"#.to_owned(),
        ])
        .await;
        let source = Arc::new(source(url));
        let http = HyperClient::new().unwrap();
        let access = exchange(source.as_ref(), &http, source.url(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(access.token, "at-1");
        let renewing = Renewing {
            source,
            http,
            access: Arc::new(Mutex::new(access)),
        };

        // `at-1` has no lifetime left, so the first request renews it; `at-2` lasts.
        assert_eq!(bearer(&renewing).await, "Bearer at-2");
        assert_eq!(bearer(&renewing).await, "Bearer at-2");

        let requests = server.await.unwrap();
        let sent = [
            ("subject_token", "jwt"),
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:token-exchange",
            ),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        assert_eq!(requests, [sent.to_vec(), sent.to_vec()]);
    }

    #[tokio::test]
    async fn a_token_that_is_not_a_bearer_token_is_refused() {
        let (url, _server) = token_endpoint(vec![
            r#"{"access_token":"at-1","token_type":"N_A","expires_in":3600}"#.to_owned(),
        ])
        .await;
        let source = source(url);
        let http = HyperClient::new().unwrap();

        let Err(refusal) = exchange(&source, &http, source.url(), Duration::from_secs(5)).await
        else {
            panic!("accepted");
        };

        assert_eq!(refusal.reason, Reason::ModelKeyMissing);
        assert!(refusal.message.contains("N_A"), "{}", refusal.message);
    }

    #[tokio::test]
    async fn a_reply_without_expires_in_is_refused() {
        let (url, _server) = token_endpoint(vec![
            r#"{"access_token":"at-1","token_type":"Bearer"}"#.to_owned(),
        ])
        .await;
        let source = source(url);
        let http = HyperClient::new().unwrap();

        let Err(refusal) = exchange(&source, &http, source.url(), Duration::from_secs(5)).await
        else {
            panic!("accepted");
        };

        assert_eq!(refusal.reason, Reason::ModelFailed);
        assert!(
            refusal.message.contains("expires_in"),
            "{}",
            refusal.message
        );
    }
}
