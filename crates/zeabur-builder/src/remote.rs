//! Asks a Git server which commit a ref points at ([`zeabur_builder_core::git`]).
//!
//! `https://` and `http://` URLs are asked over smart HTTP, anonymously or with basic auth;
//! `ssh://` and scp-like URLs over SSH with a deploy key ([`crate::ssh`]).

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use http::{Method, Request, StatusCode, header};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use rustls::ClientConfig;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;
use thiserror::Error;
use zeabur_builder_api::build::ConditionReason;
use zeabur_builder_core::git::{self, Advertisement, GitError, MAX_RESPONSE_BYTES};
use zeabur_builder_core::ssh::SshRemote;

use crate::credentials::GitCredentials;
use crate::ssh;

const TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("git/zeabur-builder-", env!("CARGO_PKG_VERSION"));

/// Why a ref could not be resolved.
#[derive(Debug, Error)]
pub(crate) enum ResolveError {
    /// Neither an HTTP nor an SSH URL.
    #[error("cannot poll `{0}`: use an http, https or ssh URL")]
    Scheme(String),
    /// An SSH URL without an SSH Secret.
    #[error("{0}")]
    NoCredentials(String),
    /// The credentials do not fit the URL, or do not parse.
    #[error("{0}")]
    Credentials(String),
    /// The SSH server's host key is not in `known_hosts`.
    #[error("{0}")]
    HostKey(String),
    /// The server refused the credentials or the repository.
    #[error("{0}")]
    Denied(String),
    /// The server answered with an HTTP error.
    #[error("{0} answered {1}")]
    Status(String, StatusCode),
    /// The request failed or timed out.
    #[error("{0}: {1}")]
    Transport(String, String),
    /// The answer was not understood, or names no such ref.
    #[error(transparent)]
    Git(#[from] GitError),
}

impl ResolveError {
    /// The `SourceReady` reason.
    pub(crate) const fn reason(&self) -> ConditionReason {
        match self {
            Self::Git(GitError::RefNotFound(_) | GitError::NoDefaultBranch) => {
                ConditionReason::RefNotFound
            }
            Self::Status(_, status)
                if status.as_u16() == 401 || status.as_u16() == 403 || status.as_u16() == 404 =>
            {
                ConditionReason::AuthFailed
            }
            Self::Denied(_) => ConditionReason::AuthFailed,
            Self::Scheme(_) => ConditionReason::UnsupportedURL,
            Self::NoCredentials(_) => ConditionReason::CredentialsMissing,
            Self::Credentials(_) => ConditionReason::CredentialsInvalid,
            Self::HostKey(_) => ConditionReason::HostKeyMismatch,
            Self::Status(..) | Self::Transport(..) | Self::Git(_) => ConditionReason::ResolveFailed,
        }
    }
}

/// An HTTP/1.1 client for `http://` and `https://` URLs.
pub(crate) type HttpClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// A client that trusts the system's CA bundle (`SSL_CERT_FILE` or `/etc/ssl/certs`).
pub(crate) fn http_client() -> Result<HttpClient, String> {
    http_client_with(None)
}

/// A client that trusts the system's CA bundle and `ca`, a PEM bundle.
pub(crate) fn http_client_with(ca: Option<&str>) -> Result<HttpClient, String> {
    let mut roots = rustls::RootCertStore::empty();
    let found = rustls_native_certs::load_native_certs();
    for cert in found.certs {
        roots.add(cert).map_err(|e| e.to_string())?;
    }
    if let Some(ca) = ca {
        for cert in CertificateDer::pem_slice_iter(ca.as_bytes()) {
            let cert = cert.map_err(|e| format!("cannot read the registry CA: {e}"))?;
            roots
                .add(cert)
                .map_err(|e| format!("cannot use the registry CA: {e}"))?;
        }
    }
    if roots.is_empty() {
        return Err(format!("no CA certificates found: {:?}", found.errors));
    }
    let config = ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let https = HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_or_http()
        .enable_http1()
        .build();
    Ok(Client::builder(TokioExecutor::new()).build(https))
}

/// An HTTP client for Git servers.
#[derive(Debug, Clone)]
pub(crate) struct Resolver {
    client: HttpClient,
}

impl Resolver {
    /// Asks with `client`.
    pub(crate) const fn new(client: HttpClient) -> Self {
        Self { client }
    }

    async fn request(
        &self,
        method: Method,
        url: &str,
        content_type: Option<&str>,
        body: Vec<u8>,
        basic: Option<(&str, &str)>,
    ) -> Result<Bytes, ResolveError> {
        let transport =
            |e: &dyn std::fmt::Display| ResolveError::Transport(url.to_owned(), e.to_string());
        let request =
            git_request(method, url, content_type, body, basic).map_err(|e| transport(&e))?;
        let response = tokio::time::timeout(TIMEOUT, self.client.request(request))
            .await
            .map_err(|e| transport(&e))?
            .map_err(|e| transport(&e))?;
        if response.status() != StatusCode::OK {
            return Err(ResolveError::Status(url.to_owned(), response.status()));
        }
        let body = tokio::time::timeout(
            TIMEOUT,
            Limited::new(response.into_body(), MAX_RESPONSE_BYTES).collect(),
        )
        .await
        .map_err(|e| transport(&e))?
        .map_err(|e| transport(&*e))?;
        Ok(body.to_bytes())
    }

    /// The commit `git_ref` points at in the repository at `url`, and the ref it came from.
    pub(crate) async fn resolve(
        &self,
        url: &str,
        git_ref: &str,
        credentials: Option<&GitCredentials>,
    ) -> Result<git::Resolved, ResolveError> {
        let http = url.starts_with("https://") || url.starts_with("http://");
        if !http {
            let remote =
                SshRemote::parse(url).ok_or_else(|| ResolveError::Scheme(url.to_owned()))?;
            let (private_key, known_hosts) = match credentials {
                Some(GitCredentials::Ssh {
                    private_key,
                    known_hosts,
                }) => (private_key, known_hosts),
                Some(GitCredentials::Basic { .. }) => {
                    return Err(ResolveError::Credentials(
                        "a kubernetes.io/basic-auth Secret needs an https URL".to_owned(),
                    ));
                }
                None => {
                    return Err(ResolveError::NoCredentials(
                        "an SSH URL needs a kubernetes.io/ssh-auth Secret in credentialsRef"
                            .to_owned(),
                    ));
                }
            };
            let advertisement = ssh::advertisement(&remote, private_key, known_hosts).await?;
            let Advertisement::Refs(refs) = git::parse_advertisement(&advertisement)? else {
                return Err(GitError::PktLine.into());
            };
            return Ok(git::resolve(git_ref, &refs)?);
        }
        let basic = match credentials {
            None => None,
            Some(GitCredentials::Basic { .. }) if !url.starts_with("https://") => {
                return Err(ResolveError::Credentials(
                    "a kubernetes.io/basic-auth Secret needs an https URL".to_owned(),
                ));
            }
            Some(GitCredentials::Basic { username, password }) => {
                Some((username.as_str(), password.as_str()))
            }
            Some(GitCredentials::Ssh { .. }) => {
                return Err(ResolveError::Credentials(
                    "a kubernetes.io/ssh-auth Secret needs an SSH URL, such as git@host:owner/repo.git"
                        .to_owned(),
                ));
            }
        };
        let base = url.trim_end_matches('/');
        let advertisement = self
            .request(
                Method::GET,
                &format!("{base}/info/refs?service=git-upload-pack"),
                None,
                Vec::new(),
                basic,
            )
            .await?;
        let refs = match git::parse_advertisement(&advertisement)? {
            Advertisement::Refs(refs) => refs,
            Advertisement::V2 => {
                let answer = self
                    .request(
                        Method::POST,
                        &format!("{base}/git-upload-pack"),
                        Some("application/x-git-upload-pack-request"),
                        git::ls_refs_request(git_ref),
                        basic,
                    )
                    .await?;
                git::parse_ls_refs(&answer)?
            }
        };
        Ok(git::resolve(git_ref, &refs)?)
    }
}

/// A smart HTTP request, with `Authorization: Basic` when `basic` gives a user and password.
fn git_request(
    method: Method,
    url: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
    basic: Option<(&str, &str)>,
) -> Result<Request<Full<Bytes>>, http::Error> {
    let mut request = Request::builder()
        .method(method)
        .uri(url)
        .header(header::USER_AGENT, USER_AGENT)
        .header("Git-Protocol", "version=2");
    if let Some(content_type) = content_type {
        request = request.header(header::CONTENT_TYPE, content_type);
    }
    if let Some((username, password)) = basic {
        let mut value = header::HeaderValue::try_from(format!(
            "Basic {}",
            STANDARD.encode(format!("{username}:{password}"))
        ))?;
        value.set_sensitive(true);
        request = request.header(header::AUTHORIZATION, value);
    }
    request.body(Full::new(Bytes::from(body)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resolve_error_names_its_source_condition_reason() {
        let text = || String::from("x");
        for (error, reason) in [
            (
                ResolveError::Git(GitError::RefNotFound(text())),
                ConditionReason::RefNotFound,
            ),
            (
                ResolveError::Git(GitError::NoDefaultBranch),
                ConditionReason::RefNotFound,
            ),
            (
                ResolveError::Status(text(), StatusCode::NOT_FOUND),
                ConditionReason::AuthFailed,
            ),
            (ResolveError::Denied(text()), ConditionReason::AuthFailed),
            (
                ResolveError::Scheme(text()),
                ConditionReason::UnsupportedURL,
            ),
            (
                ResolveError::NoCredentials(text()),
                ConditionReason::CredentialsMissing,
            ),
            (
                ResolveError::Credentials(text()),
                ConditionReason::CredentialsInvalid,
            ),
            (
                ResolveError::HostKey(text()),
                ConditionReason::HostKeyMismatch,
            ),
            (
                ResolveError::Status(text(), StatusCode::BAD_GATEWAY),
                ConditionReason::ResolveFailed,
            ),
            (
                ResolveError::Transport(text(), text()),
                ConditionReason::ResolveFailed,
            ),
        ] {
            assert_eq!(error.reason(), reason);
        }
    }

    #[test]
    fn basic_auth_header() {
        let request = git_request(
            Method::GET,
            "https://github.com/acme/api.git/info/refs?service=git-upload-pack",
            None,
            Vec::new(),
            Some(("x-access-token", "ghs_secret")),
        );
        let Ok(request) = request else {
            panic!("request not built");
        };
        let auth = request.headers().get(header::AUTHORIZATION);
        assert_eq!(
            auth.and_then(|v| v.to_str().ok()),
            Some("Basic eC1hY2Nlc3MtdG9rZW46Z2hzX3NlY3JldA==")
        );
        assert!(auth.is_some_and(header::HeaderValue::is_sensitive));
        assert!(!format!("{request:?}").contains("eC1hY2Nlc3MtdG9rZW46Z2hzX3NlY3JldA"));

        let anonymous = git_request(Method::GET, "https://github.com/x", None, Vec::new(), None);
        assert!(anonymous.is_ok_and(|r| !r.headers().contains_key(header::AUTHORIZATION)));
    }

    #[tokio::test]
    async fn credentials_must_fit_the_url() {
        let Ok(client) = http_client() else {
            return;
        };
        let resolver = Resolver::new(client);
        let ssh = GitCredentials::Ssh {
            private_key: String::new(),
            known_hosts: String::new(),
        };
        let basic = GitCredentials::Basic {
            username: "u".to_owned(),
            password: "p".to_owned(),
        };
        let reason = async |url: &str, credentials: Option<&GitCredentials>| {
            resolver
                .resolve(url, "main", credentials)
                .await
                .map_err(|e| e.reason())
        };
        assert_eq!(
            reason("https://github.com/acme/api.git", Some(&ssh)).await,
            Err(ConditionReason::CredentialsInvalid)
        );
        assert_eq!(
            reason("git@github.com:acme/api.git", None).await,
            Err(ConditionReason::CredentialsMissing)
        );
        assert_eq!(
            reason("git@github.com:acme/api.git", Some(&basic)).await,
            Err(ConditionReason::CredentialsInvalid)
        );
        assert_eq!(
            reason("git@github.com:acme/api.git", Some(&ssh)).await,
            Err(ConditionReason::CredentialsInvalid)
        );
        assert_eq!(
            reason("http://git.example.com/acme/api.git", Some(&basic)).await,
            Err(ConditionReason::CredentialsInvalid)
        );
        assert_eq!(
            reason("file:///srv/api.git", None).await,
            Err(ConditionReason::UnsupportedURL)
        );
    }
}
