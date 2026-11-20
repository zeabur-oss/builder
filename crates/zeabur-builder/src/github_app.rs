//! `zeabur-builder github-app`: creates the cluster's GitHub App with GitHub's manifest flow and
//! prints the Secret [`crate::github`] reads ([manifest flow]).
//!
//! 1. The browser POSTs the manifest, as the form field `manifest`, to `https://github.com/settings/apps/new`
//!    (or `/organizations/<org>/settings/apps/new`) with an unguessable `state`.
//! 2. GitHub sends the browser back to `redirect_url` with a `code` and the same `state`.
//! 3. `POST /app-manifests/{code}/conversions`, within an hour, returns the App's id, client id,
//!    private key and webhook secret ([conversion]). The example sends no token, and neither does
//!    this; GitHub's page does not say whether one is needed (not verified).
//!
//! `create` serves steps 1 and 2 on `127.0.0.1` and writes the Secret to stdout, for
//! `kubectl apply -f -`; nothing else keeps the key. `manifest` prints only the manifest, for
//! another front end such as Zeabur's.
//!
//! The App asks for the least it needs: `contents: read` to resolve and fetch, `metadata: read`
//! (which GitHub grants every App), `statuses: write` for commit statuses, and `push` events.
//!
//! [manifest flow]: https://docs.github.com/en/apps/sharing-github-apps/registering-a-github-app-from-a-manifest
//! [conversion]: https://docs.github.com/en/rest/apps/apps#create-a-github-app-from-a-manifest

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use aws_lc_rs::constant_time::verify_slices_are_equal;
use bytes::Bytes;
use clap::{Args, Subcommand};
use http::{Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::{output, remote};

/// Why the App could not be created.
#[derive(Debug, Error)]
pub(crate) enum GithubAppError {
    /// No HTTP client for GitHub.
    #[error(transparent)]
    Client(#[from] remote::ClientError),
    /// The manifest or the Secret does not encode.
    #[error(transparent)]
    Encode(#[from] serde_json::Error),
    /// The conversion request could not be built.
    #[error(transparent)]
    Request(#[from] http::Error),
    /// GitHub did not answer in time.
    #[error("timed out")]
    TimedOut(#[from] tokio::time::error::Elapsed),
    /// GitHub could not be reached.
    #[error(transparent)]
    Transport(#[from] hyper_util::client::legacy::Error),
    /// The answer's body could not be read.
    #[error("{0}")]
    Body(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// GitHub refused the conversion.
    #[error("GitHub answered {0} to the conversion")]
    Status(StatusCode),
    /// GitHub's answer is not the App's conversion.
    #[error("GitHub's answer is not the App's conversion: {0}")]
    Answer(#[source] serde_json::Error),
    /// The local callback server cannot listen.
    #[error("cannot listen on 127.0.0.1: {0}")]
    Listen(#[source] std::io::Error),
    /// No random state for the flow.
    #[error("no randomness")]
    NoRandomness(#[source] aws_lc_rs::error::Unspecified),
    /// The browser did not come back in time.
    #[error("GitHub's manifest flow takes at most an hour")]
    FlowTimedOut(#[source] tokio::time::error::Elapsed),
    /// The callback never delivered a conversion.
    #[error("the flow stopped")]
    FlowStopped(#[source] oneshot::error::RecvError),
    /// The callback server ended.
    #[error("the callback server stopped")]
    ServerStopped,
}

/// The manifest flow's time limit.
const FLOW_LIMIT: Duration = Duration::from_hours(1);
const USER_AGENT: &str = concat!("zeabur-builder/", env!("CARGO_PKG_VERSION"));

/// `github-app` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Create the App in a browser, then print its Secret for `kubectl apply -f -`.
    Create(CreateArgs),
    /// Print the App's manifest.
    Manifest(ManifestArgs),
}

/// What the manifest needs.
#[derive(Debug, Args)]
pub(crate) struct ManifestArgs {
    /// Where GitHub sends the App's events: the receiver's public URL ending in
    /// `/hooks/github-app`.
    #[arg(long)]
    webhook_url: String,
    /// The App's name, unique on GitHub.
    #[arg(long, default_value = "Zeabur Builder")]
    name: String,
    /// Where GitHub sends the browser back with the code.
    #[arg(long)]
    redirect_url: Option<String>,
}

/// `create`.
#[derive(Debug, Args)]
pub(crate) struct CreateArgs {
    #[command(flatten)]
    manifest: ManifestArgs,
    /// Create the App for this organization instead of your account.
    #[arg(long)]
    organization: Option<String>,
    /// The GitHub server.
    #[arg(long, default_value = "https://github.com")]
    github_url: String,
    /// Its REST API.
    #[arg(long, default_value = "https://api.github.com")]
    api_url: String,
    /// The Secret's name.
    #[arg(long, default_value = "github-app")]
    secret: String,
    /// The controller's namespace, where the Secret goes.
    #[arg(long, default_value = "builder-system")]
    namespace: String,
}

/// The App's manifest, as GitHub's manifest flow reads it.
#[derive(Debug, Serialize)]
pub(crate) struct Manifest<'a> {
    name: &'a str,
    url: &'static str,
    description: &'static str,
    hook_attributes: HookAttributes<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    redirect_url: Option<&'a str>,
    public: bool,
    default_permissions: BTreeMap<&'static str, &'static str>,
    default_events: [&'static str; 1],
}

#[derive(Debug, Serialize)]
struct HookAttributes<'a> {
    url: &'a str,
    active: bool,
}

/// The App's manifest.
pub(crate) fn manifest<'a>(
    name: &'a str,
    webhook_url: &'a str,
    redirect_url: Option<&'a str>,
) -> Manifest<'a> {
    Manifest {
        name,
        url: "https://github.com/zeabur-oss/builder",
        description: "Builds container images of pushed commits with Zeabur Builder.",
        hook_attributes: HookAttributes {
            url: webhook_url,
            active: true,
        },
        redirect_url,
        public: false,
        default_permissions: BTreeMap::from([
            ("contents", "read"),
            ("metadata", "read"),
            ("statuses", "write"),
        ]),
        default_events: ["push"],
    }
}

/// What GitHub answers to the conversion: the App's id, keys and page.
#[derive(Debug, Deserialize)]
pub(crate) struct Conversion {
    id: u64,
    client_id: String,
    pem: String,
    webhook_secret: String,
    #[serde(default)]
    html_url: String,
}

impl Conversion {
    /// The Secret [`crate::github`] reads.
    pub(crate) fn secret(&self, name: &str, namespace: &str, api_url: &str) -> Secret {
        let mut data = BTreeMap::from([
            ("app-id".to_owned(), self.id.to_string()),
            ("client-id".to_owned(), self.client_id.clone()),
            ("private-key".to_owned(), self.pem.clone()),
            ("webhook-secret".to_owned(), self.webhook_secret.clone()),
        ]);
        if api_url.trim_end_matches('/') != "https://api.github.com" {
            data.insert("api-url".to_owned(), api_url.to_owned());
        }
        Secret {
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                namespace: Some(namespace.to_owned()),
                annotations: Some(BTreeMap::from([(
                    "builder.zeabur.com/github-app".to_owned(),
                    self.html_url.clone(),
                )])),
                ..ObjectMeta::default()
            },
            type_: Some("Opaque".to_owned()),
            string_data: Some(data),
            ..Secret::default()
        }
    }
}

fn escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '&' => "&amp;".to_owned(),
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            '"' => "&quot;".to_owned(),
            '\'' => "&#39;".to_owned(),
            c => c.to_string(),
        })
        .collect()
}

/// The page that POSTs the manifest to GitHub.
pub(crate) fn form(action: &str, manifest: &Manifest<'_>) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><title>Create the GitHub App</title>\
         <form method=post action=\"{}\"><input type=hidden name=manifest value=\"{}\">\
         <p>Zeabur Builder will ask GitHub to create the App below.</p>\
         <pre>{}</pre><button type=submit>Create on GitHub</button></form>",
        escape(action),
        escape(&serde_json::to_string(manifest).unwrap_or_default()),
        escape(&serde_json::to_string_pretty(manifest).unwrap_or_default()),
    )
}

/// `code` and `state` from `/callback?code=…&state=…`.
fn callback(query: &str) -> (Option<&str>, Option<&str>) {
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("code", v)) => code = Some(v),
            Some(("state", v)) => state = Some(v),
            _ => {}
        }
    }
    let code = code.filter(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_alphanumeric()));
    (code, state)
}

fn page(status: StatusCode, text: &str) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(format!(
        "<!doctype html><meta charset=utf-8><p>{}</p>",
        escape(text)
    ))));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
}

async fn convert(api_url: &str, code: &str) -> Result<Conversion, GithubAppError> {
    let client = remote::http_client()?;
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!(
            "{}/app-manifests/{code}/conversions",
            api_url.trim_end_matches('/')
        ))
        .header(header::USER_AGENT, USER_AGENT)
        .header(header::ACCEPT, "application/vnd.github+json")
        .body(Full::new(Bytes::new()))?;
    let response = tokio::time::timeout(Duration::from_secs(30), client.request(request)).await??;
    let status = response.status();
    let body = Limited::new(response.into_body(), 1024 * 1024)
        .collect()
        .await
        .map_err(GithubAppError::Body)?
        .to_bytes();
    if status != StatusCode::CREATED {
        return Err(GithubAppError::Status(status));
    }
    serde_json::from_slice(&body).map_err(GithubAppError::Answer)
}

/// Runs a `github-app` subcommand; the text for stdout.
pub(crate) async fn run(command: Command) -> Result<String, GithubAppError> {
    match command {
        Command::Manifest(args) => Ok(serde_json::to_string(&manifest(
            &args.name,
            &args.webhook_url,
            args.redirect_url.as_deref(),
        ))?),
        Command::Create(args) => create(args).await,
    }
}

type Done = Arc<Mutex<Option<oneshot::Sender<Result<Conversion, GithubAppError>>>>>;

/// The form page, the state, the API URL and where the conversion goes.
type Shared = (String, String, String, Done);

/// Serves the form and takes GitHub's redirect back.
async fn answer(
    request: &Request<hyper::body::Incoming>,
    shared: &Shared,
) -> Response<Full<Bytes>> {
    let (html, state, api_url, done) = shared;
    match request.uri().path() {
        "/" => page(StatusCode::OK, "").map(|_| Full::new(Bytes::from(html.clone()))),
        "/callback" => {
            let (code, given) = callback(request.uri().query().unwrap_or_default());
            let state_ok = given
                .is_some_and(|g| verify_slices_are_equal(g.as_bytes(), state.as_bytes()).is_ok());
            match code {
                Some(code) if state_ok => {
                    let converted = convert(api_url, code).await;
                    let text = match &converted {
                        Ok(_) => {
                            "The App is created. Its Secret is on the terminal; close this tab."
                                .to_owned()
                        }
                        Err(e) => e.to_string(),
                    };
                    if let Some(tx) = done.lock().unwrap_or_else(PoisonError::into_inner).take()
                        && tx.send(converted).is_err()
                    {
                        tracing::debug!("the flow already ended");
                    }
                    page(StatusCode::OK, &text)
                }
                _ => page(
                    StatusCode::BAD_REQUEST,
                    "The code or state is missing or wrong.",
                ),
            }
        }
        _ => page(StatusCode::NOT_FOUND, "Not found."),
    }
}

async fn create(args: CreateArgs) -> Result<String, GithubAppError> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(GithubAppError::Listen)?;
    let port = listener
        .local_addr()
        .map_err(GithubAppError::Listen)?
        .port();
    let mut nonce = [0_u8; 24];
    aws_lc_rs::rand::fill(&mut nonce).map_err(GithubAppError::NoRandomness)?;
    let state: String = nonce
        .iter()
        .flat_map(|b| [b >> 4, b & 15])
        .filter_map(|n| char::from_digit(n.into(), 16))
        .collect();
    let github = args.github_url.trim_end_matches('/');
    let new_app = args.organization.as_ref().map_or_else(
        || format!("{github}/settings/apps/new"),
        |org| format!("{github}/organizations/{org}/settings/apps/new"),
    );
    let redirect = args
        .manifest
        .redirect_url
        .clone()
        .unwrap_or_else(|| format!("http://127.0.0.1:{port}/callback"));
    let page_html = form(
        &format!("{new_app}?state={state}"),
        &manifest(
            &args.manifest.name,
            &args.manifest.webhook_url,
            Some(&redirect),
        ),
    );
    output::stderr_line(&format!(
        "Open http://127.0.0.1:{port}/ in a browser signed in to GitHub."
    ));

    let (tx, rx) = oneshot::channel();
    let done: Done = Arc::new(Mutex::new(Some(tx)));
    let shared: Arc<Shared> = Arc::new((page_html, state, args.api_url.clone(), done));
    let serve = async {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let shared = Arc::clone(&shared);
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                    let shared = Arc::clone(&shared);
                    async move { Ok::<_, Infallible>(answer(&request, &shared).await) }
                });
                if let Err(e) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                {
                    tracing::debug!(error = %e, "callback connection");
                }
            });
        }
    };
    let converted = tokio::select! {
        result = tokio::time::timeout(FLOW_LIMIT, rx) => result
            .map_err(GithubAppError::FlowTimedOut)?
            .map_err(GithubAppError::FlowStopped)??,
        () = serve => return Err(GithubAppError::ServerStopped),
    };
    let secret = converted.secret(&args.secret, &args.namespace, &args.api_url);
    output::stderr_line(&format!(
        "Created {}. Install it on the accounts whose repositories you build, then allow \
         namespaces in the ConfigMap github-app-installations.",
        if converted.html_url.is_empty() {
            "the App"
        } else {
            &converted.html_url
        }
    ));
    Ok(serde_json::to_string_pretty(&secret)?)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_manifest_asks_for_little() {
        let m = serde_json::to_value(manifest(
            "Zeabur Builder",
            "https://hooks.example.com/hooks/github-app",
            Some("http://127.0.0.1:9/callback"),
        ))
        .unwrap();
        assert_eq!(
            m,
            json!({
                "name": "Zeabur Builder",
                "url": "https://github.com/zeabur-oss/builder",
                "description": "Builds container images of pushed commits with Zeabur Builder.",
                "hook_attributes": {"url": "https://hooks.example.com/hooks/github-app", "active": true},
                "redirect_url": "http://127.0.0.1:9/callback",
                "public": false,
                "default_permissions": {"contents": "read", "metadata": "read", "statuses": "write"},
                "default_events": ["push"],
            })
        );
    }

    #[test]
    fn the_secret_keeps_what_the_controller_reads() {
        let conversion: Conversion = serde_json::from_value(json!({
            "id": 42, "slug": "zeabur-builder", "client_id": "Iv23li", "client_secret": "cs",
            "webhook_secret": "ws", "pem": "-----BEGIN RSA PRIVATE KEY-----",
            "html_url": "https://github.com/apps/zeabur-builder", "owner": {}
        }))
        .unwrap();
        let s = serde_json::to_value(conversion.secret(
            "github-app",
            "builder-system",
            "https://api.github.com/",
        ))
        .unwrap();
        assert_eq!(
            s,
            json!({
                "apiVersion": "v1",
                "kind": "Secret",
                "metadata": {
                    "name": "github-app",
                    "namespace": "builder-system",
                    "annotations": {"builder.zeabur.com/github-app": "https://github.com/apps/zeabur-builder"},
                },
                "type": "Opaque",
                "stringData": {
                    "app-id": "42", "client-id": "Iv23li",
                    "private-key": "-----BEGIN RSA PRIVATE KEY-----", "webhook-secret": "ws",
                },
            })
        );
        let ghes =
            serde_json::to_value(conversion.secret("x", "y", "https://ghe.example.com/api/v3"))
                .unwrap();
        assert_eq!(
            ghes,
            json!({
                "apiVersion": "v1",
                "kind": "Secret",
                "metadata": {
                    "name": "x",
                    "namespace": "y",
                    "annotations": {"builder.zeabur.com/github-app": "https://github.com/apps/zeabur-builder"},
                },
                "type": "Opaque",
                "stringData": {
                    "app-id": "42", "client-id": "Iv23li",
                    "private-key": "-----BEGIN RSA PRIVATE KEY-----", "webhook-secret": "ws",
                    "api-url": "https://ghe.example.com/api/v3",
                },
            })
        );
    }

    #[test]
    fn an_answer_without_the_keys_is_refused() {
        let no_secret = json!({
            "id": 42, "client_id": "Iv23li", "webhook_secret": null, "pem": "k"
        });
        assert!(serde_json::from_value::<Conversion>(no_secret).is_err());
    }

    #[test]
    fn the_form_escapes_the_manifest() {
        let m = manifest("<x\" onload=\"", "https://h/hooks/github-app", None);
        let html = form("https://github.com/settings/apps/new?state=s&x=1", &m);
        assert!(!html.contains("<x"));
        assert!(!html.contains("\" onload"));
        assert!(html.contains("state=s&amp;x=1"));
    }

    #[test]
    fn callbacks() {
        assert_eq!(callback("code=abc123&state=s"), (Some("abc123"), Some("s")));
        assert_eq!(callback("state=s&code=a/../b").0, None);
        assert_eq!(callback("code=").0, None);
        assert_eq!(callback(""), (None, None));
    }
}
