//! The cluster's GitHub App: installation tokens to resolve and fetch, and commit statuses.
//!
//! A cluster admin creates the App with GitHub's manifest flow ([`crate::github_app`]) and keeps
//! what GitHub returns in a Secret in the controller's namespace:
//!
//! | Key | Holds |
//! |---|---|
//! | `app-id` | The App's id |
//! | `client-id` | Its client id, the JWT issuer GitHub recommends; the id is used without it |
//! | `private-key` | Its PEM private key, PKCS#1 as GitHub hands it out, or PKCS#8 |
//! | `webhook-secret` | The secret of the App's webhook, checked on `/hooks/github-app` |
//! | `api-url` | Optional: `https://ghe.example.com/api/v3` for GitHub Enterprise Server |
//!
//! Every installation can reach repositories its account owns, whoever writes the `Build`. So a
//! `Build` may name an installation only when the `ConfigMap` of grants, also in the controller's
//! namespace, lists the `Build`'s namespace under the installation's id:
//!
//! ```yaml
//! data:
//!   "12345678": "team-a, team-b"   # or "*" for every namespace
//! ```
//!
//! Only cluster admins write either object, so a team cannot grant itself another team's
//! repositories.
//!
//! Authentication follows GitHub's documentation: a JWT signed with RS256, `iat` 60 seconds in
//! the past for clock drift, `exp` at most 10 minutes ahead, `iss` the client id
//! ([JWT]); then `POST /app/installations/{id}/access_tokens` with that JWT, narrowed to the one
//! repository and the permissions needed ([installation token]). Tokens last an hour. The
//! controller keeps its own (`contents: read`, `statuses: write`) until 5 minutes before they
//! expire; each Job gets a fresh `contents: read` token in a Secret its `BuildRun` owns, deleted
//! when the run finishes. Statuses are `POST /repos/{owner}/{repo}/statuses/{sha}`, which needs
//! the "Commit statuses" permission ([statuses], [permissions]).
//!
//! [JWT]: https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-json-web-token-jwt-for-a-github-app
//! [installation token]: https://docs.github.com/en/rest/apps/apps#create-an-installation-access-token-for-an-app
//! [statuses]: https://docs.github.com/en/rest/commits/statuses#create-a-commit-status
//! [permissions]: https://docs.github.com/en/rest/authentication/permissions-required-for-github-apps

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use bytes::Bytes;
use http::{Method, Request, StatusCode, header};
use http_body_util::{BodyExt as _, Full, Limited};
use jiff::Timestamp;
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use kube::Api;
use serde_json::{Value, json};
use zeabur_builder_api::build::ConditionReason;
use zeabur_builder_core::repo::RepoKey;

use crate::credentials::Refusal;
use crate::remote::HttpClient;

const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ANSWER: usize = 1024 * 1024;
const USER_AGENT: &str = concat!("zeabur-builder/", env!("CARGO_PKG_VERSION"));
/// <https://docs.github.com/en/rest/about-the-rest-api/api-versions>
const API_VERSION: &str = "2022-11-28";
/// A cached token is used while it has at least this long left.
const TOKEN_MARGIN: Duration = Duration::from_mins(5);
/// The longest description GitHub shows on a status; longer ones are cut.
const MAX_DESCRIPTION: usize = 140;

/// What the App's Secret holds.
pub(crate) struct AppConfig {
    issuer: Value,
    key: RsaKeyPair,
    /// The App webhook's secret.
    pub(crate) webhook_secret: Option<Vec<u8>>,
    api_url: String,
    /// The host repositories live on: `github.com` for `api.github.com`, the server's own host
    /// for GitHub Enterprise Server.
    web_host: String,
}

impl fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppConfig")
            .field("issuer", &self.issuer)
            .field("api_url", &self.api_url)
            .finish_non_exhaustive()
    }
}

fn text(secret: &Secret, key: &str) -> Option<String> {
    secret
        .data
        .as_ref()?
        .get(key)
        .and_then(|v| String::from_utf8(v.0.clone()).ok())
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

impl AppConfig {
    /// Reads the App's Secret.
    pub(crate) fn from_secret(secret: &Secret) -> Result<Self, String> {
        let app_id = text(secret, "app-id").ok_or("no `app-id`")?;
        let issuer = match text(secret, "client-id") {
            Some(client_id) => Value::String(client_id),
            None => app_id
                .parse::<u64>()
                .map(Value::from)
                .map_err(|_error| "`app-id` is not a number")?,
        };
        let pem = text(secret, "private-key").ok_or("no `private-key`")?;
        let key = private_key(&pem)?;
        let api_url = text(secret, "api-url")
            .unwrap_or_else(|| "https://api.github.com".to_owned())
            .trim_end_matches('/')
            .to_owned();
        let web_host = web_host(&api_url)?;
        Ok(Self {
            issuer,
            key,
            webhook_secret: text(secret, "webhook-secret").map(String::into_bytes),
            api_url,
            web_host,
        })
    }

    /// A JWT for the App, valid from a minute before `now` to nine minutes after.
    pub(crate) fn jwt(&self, now: Timestamp) -> Result<String, String> {
        let now = now.as_second();
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = json!({"iat": now - 60, "exp": now + 9 * 60, "iss": self.issuer});
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
        let input = format!("{header}.{claims}");
        let mut signature = vec![0; self.key.public_modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                input.as_bytes(),
                &mut signature,
            )
            .map_err(|_error| "cannot sign the JWT".to_owned())?;
        Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }

    /// The owner and name of a repository on the App's server.
    pub(crate) fn repository<'a>(&self, key: &'a RepoKey) -> Result<(&'a str, &'a str), String> {
        let mut parts = key.as_str().split('/');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(host), Some(owner), Some(name), None) if host == self.web_host => {
                Ok((owner, name))
            }
            _ => Err(format!(
                "`{key}` is not a repository on {}, the GitHub App's server",
                self.web_host
            )),
        }
    }
}

/// Parses a PEM private key: PKCS#1 (`RSA PRIVATE KEY`) or PKCS#8 (`PRIVATE KEY`).
pub(crate) fn private_key(pem: &str) -> Result<RsaKeyPair, String> {
    let label = |l: &str| pem.contains(&format!("-----BEGIN {l}-----"));
    let pkcs1 = label("RSA PRIVATE KEY");
    if !pkcs1 && !label("PRIVATE KEY") {
        return Err("`private-key` is not a PEM RSA private key".to_owned());
    }
    let body: String = pem
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("-----"))
        .collect();
    let der = STANDARD
        .decode(body)
        .map_err(|_error| "`private-key` is not valid PEM".to_owned())?;
    let key = if pkcs1 {
        RsaKeyPair::from_der(&der)
    } else {
        RsaKeyPair::from_pkcs8(&der)
    };
    key.map_err(|e| format!("`private-key` is rejected: {e}"))
}

fn web_host(api_url: &str) -> Result<String, String> {
    let rest = api_url
        .strip_prefix("https://")
        .ok_or("`api-url` must be an https URL")?;
    let host = rest
        .split('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if host.is_empty() {
        return Err("`api-url` has no host".to_owned());
    }
    Ok(if host == "api.github.com" {
        "github.com".to_owned()
    } else {
        host
    })
}

/// Whether a grant (`"team-a, team-b"` or `"*"`) covers `namespace`.
pub(crate) fn granted(grant: &str, namespace: &str) -> bool {
    grant
        .split(|c: char| c == ',' || c.is_whitespace())
        .any(|n| n == "*" || n == namespace)
}

/// What a token may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Access {
    /// Read the repository and set commit statuses: the controller's.
    Controller,
    /// Read the repository: a Job's.
    Fetch,
}

impl Access {
    fn permissions(self) -> Value {
        match self {
            Self::Controller => json!({"contents": "read", "statuses": "write"}),
            Self::Fetch => json!({"contents": "read"}),
        }
    }
}

/// A commit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    /// Building.
    Pending,
    /// Built and pushed.
    Success,
    /// The build failed.
    Failure,
    /// Cancelled, or the infrastructure failed.
    Error,
}

impl State {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Error => "error",
        }
    }
}

/// The body of a commit status.
pub(crate) fn status_body(state: State, context: &str, description: &str) -> Value {
    let description: String = description.chars().take(MAX_DESCRIPTION).collect();
    json!({"state": state.as_str(), "context": context, "description": description})
}

/// Installation, Build and access of a cached token.
type TokenKey = (i64, String, Access);

/// The App, read from its Secret when needed, so an admin can set it up or rotate the key while
/// the controller runs.
///
/// The App and its tokens are cached per replica; a replica that has no token mints its own.
pub(crate) struct GitHub {
    http: HttpClient,
    secrets: Api<Secret>,
    config_maps: Api<ConfigMap>,
    secret_name: String,
    grants_name: String,
    app: Mutex<Option<(String, Arc<AppConfig>)>>,
    tokens: Mutex<HashMap<TokenKey, (String, Timestamp)>>,
}

impl fmt::Debug for GitHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHub")
            .field("secret_name", &self.secret_name)
            .field("grants_name", &self.grants_name)
            .finish_non_exhaustive()
    }
}

impl GitHub {
    /// The App whose Secret is `secret_name`, and whose grants are the `ConfigMap`
    /// `grants_name`, both in `namespace`.
    pub(crate) fn new(
        client: &kube::Client,
        http: HttpClient,
        namespace: &str,
        secret_name: String,
        grants_name: String,
    ) -> Self {
        Self {
            http,
            secrets: Api::namespaced(client.clone(), namespace),
            config_maps: Api::namespaced(client.clone(), namespace),
            secret_name,
            grants_name,
            app: Mutex::new(None),
            tokens: Mutex::new(HashMap::new()),
        }
    }

    /// The App, or why there is none.
    pub(crate) async fn app(&self) -> Result<Arc<AppConfig>, Refusal> {
        let name = &self.secret_name;
        let missing = |m: String| (ConditionReason::GitHubAppMissing, m);
        let secret = self
            .secrets
            .get_opt(name)
            .await
            .map_err(|e| {
                (
                    ConditionReason::ResolveFailed,
                    format!("the GitHub App's Secret: {e}"),
                )
            })?
            .ok_or_else(|| missing(format!("no GitHub App: the Secret `{name}` is missing")))?;
        let version = secret.metadata.resource_version.clone().unwrap_or_default();
        let mut cached = self.app.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((v, app)) = cached.as_ref()
            && *v == version
        {
            return Ok(Arc::clone(app));
        }
        let app = Arc::new(
            AppConfig::from_secret(&secret)
                .map_err(|e| missing(format!("the GitHub App's Secret `{name}`: {e}")))?,
        );
        *cached = Some((version, Arc::clone(&app)));
        drop(cached);
        Ok(app)
    }

    /// Whether `namespace` may use `installation`.
    pub(crate) async fn allowed(&self, installation: i64, namespace: &str) -> Result<(), Refusal> {
        let grants = self
            .config_maps
            .get_opt(&self.grants_name)
            .await
            .map_err(|e| {
                (
                    ConditionReason::ResolveFailed,
                    format!("the GitHub App's grants: {e}"),
                )
            })?;
        let grant = grants
            .and_then(|c| c.data)
            .and_then(|mut d| d.remove(&installation.to_string()));
        if grant.is_some_and(|g| granted(&g, namespace)) {
            Ok(())
        } else {
            Err((
                ConditionReason::InstallationNotAllowed,
                format!(
                    "a cluster admin has not allowed namespace `{namespace}` to use GitHub App installation {installation}"
                ),
            ))
        }
    }

    /// A token of `installation` for the repository `key`.
    pub(crate) async fn token(
        &self,
        installation: i64,
        key: &RepoKey,
        access: Access,
    ) -> Result<String, Refusal> {
        let now = Timestamp::now();
        let cache_key = (installation, key.as_str().to_owned(), access);
        if access == Access::Controller {
            let tokens = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some((token, expires)) = tokens.get(&cache_key)
                && expires.duration_since(now).unsigned_abs() > TOKEN_MARGIN
                && *expires > now
            {
                return Ok(token.clone());
            }
        }
        let app = self.app().await?;
        let (_, name) = app
            .repository(key)
            .map_err(|m| (ConditionReason::UnsupportedURL, m))?;
        let jwt = app
            .jwt(now)
            .map_err(|m| (ConditionReason::GitHubAppMissing, m))?;
        let url = format!(
            "{}/app/installations/{installation}/access_tokens",
            app.api_url
        );
        let body = json!({"repositories": [name], "permissions": access.permissions()});
        let (status, answer) = self.call(Method::POST, &url, &jwt, &body).await?;
        if status != StatusCode::CREATED {
            let reason = if status.is_server_error() {
                ConditionReason::ResolveFailed
            } else {
                ConditionReason::AuthFailed
            };
            return Err((
                reason,
                format!(
                    "GitHub refused a token of installation {installation} for {key}: {status} {}",
                    github_message(&answer)
                ),
            ));
        }
        let answer: Value = serde_json::from_slice(&answer).map_err(|_error| {
            (
                ConditionReason::ResolveFailed,
                "GitHub's token answer is not JSON".to_owned(),
            )
        })?;
        let token = answer
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                (
                    ConditionReason::ResolveFailed,
                    "GitHub's answer has no token".to_owned(),
                )
            })?
            .to_owned();
        let expires = answer
            .get("expires_at")
            .and_then(Value::as_str)
            .and_then(|t| t.parse::<Timestamp>().ok())
            .unwrap_or(now);
        if access == Access::Controller {
            let mut tokens = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
            tokens.retain(|_, (_, e)| *e > now);
            tokens.insert(cache_key, (token.clone(), expires));
        }
        Ok(token)
    }

    /// Sets a commit status. Failures are logged; a build never fails over its status.
    pub(crate) async fn report(&self, installation: i64, key: &RepoKey, sha: &str, body: &Value) {
        if let Err(e) = self.try_report(installation, key, sha, body).await {
            tracing::warn!(repository = %key, revision = sha, error = %e.1, "commit status not set");
        }
    }

    async fn try_report(
        &self,
        installation: i64,
        key: &RepoKey,
        sha: &str,
        body: &Value,
    ) -> Result<(), Refusal> {
        let app = self.app().await?;
        let (owner, name) = app
            .repository(key)
            .map_err(|m| (ConditionReason::UnsupportedURL, m))?;
        let token = self.token(installation, key, Access::Controller).await?;
        let url = format!("{}/repos/{owner}/{name}/statuses/{sha}", app.api_url);
        let (status, answer) = self.call(Method::POST, &url, &token, body).await?;
        if status == StatusCode::CREATED {
            Ok(())
        } else {
            Err((
                ConditionReason::AuthFailed,
                format!("{status} {}", github_message(&answer)),
            ))
        }
    }

    async fn call(
        &self,
        method: Method,
        url: &str,
        bearer: &str,
        body: &Value,
    ) -> Result<(StatusCode, Bytes), Refusal> {
        let failed = |e: &dyn fmt::Display| (ConditionReason::ResolveFailed, format!("{url}: {e}"));
        let mut auth =
            header::HeaderValue::try_from(format!("Bearer {bearer}")).map_err(|e| failed(&e))?;
        auth.set_sensitive(true);
        let request = Request::builder()
            .method(method)
            .uri(url)
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::ACCEPT, "application/vnd.github+json")
            .header(header::CONTENT_TYPE, "application/json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(header::AUTHORIZATION, auth)
            .body(Full::new(Bytes::from(body.to_string())))
            .map_err(|e| failed(&e))?;
        let response = tokio::time::timeout(TIMEOUT, self.http.request(request))
            .await
            .map_err(|e| failed(&e))?
            .map_err(|e| failed(&e))?;
        let status = response.status();
        let answer = tokio::time::timeout(
            TIMEOUT,
            Limited::new(response.into_body(), MAX_ANSWER).collect(),
        )
        .await
        .map_err(|e| failed(&e))?
        .map_err(|e| failed(&*e))?
        .to_bytes();
        Ok((status, answer))
    }
}

/// GitHub's `message`, for an error.
fn github_message(answer: &[u8]) -> String {
    serde_json::from_slice::<Value>(answer)
        .ok()
        .and_then(|v| {
            v.get("message")
                .and_then(Value::as_str)
                .map(|m| m.chars().take(200).collect())
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use aws_lc_rs::encoding::AsDer as _;
    use aws_lc_rs::rsa::KeySize;
    use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
    use k8s_openapi::ByteString;

    use super::*;

    fn pkcs8_pem() -> (String, Vec<u8>) {
        let key = RsaKeyPair::generate(KeySize::Rsa2048).unwrap();
        let der = key.as_der().unwrap();
        let body = STANDARD.encode(der.as_ref());
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        (
            format!(
                "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
                lines.join("\n")
            ),
            key.public_key().as_ref().to_vec(),
        )
    }

    fn secret(data: &[(&str, &str)]) -> Secret {
        Secret {
            data: Some(
                data.iter()
                    .map(|(k, v)| ((*k).to_owned(), ByteString(v.as_bytes().to_vec())))
                    .collect::<BTreeMap<_, _>>(),
            ),
            ..Secret::default()
        }
    }

    #[test]
    fn jwt_is_rs256_and_short_lived() {
        let (pem, public) = pkcs8_pem();
        let app = AppConfig::from_secret(&secret(&[
            ("app-id", "12345"),
            ("client-id", "Iv23liexample"),
            ("private-key", &pem),
        ]))
        .unwrap();
        let now: Timestamp = "2026-10-23T00:00:00Z".parse().unwrap();
        let jwt = app.jwt(now).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header, json!({"alg": "RS256", "typ": "JWT"}));
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        let t = now.as_second();
        assert_eq!(
            claims,
            json!({"iat": t - 60, "exp": t + 540, "iss": "Iv23liexample"})
        );
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public)
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .unwrap();
    }

    #[test]
    fn app_id_is_the_issuer_without_a_client_id() {
        let (pem, _) = pkcs8_pem();
        let app =
            AppConfig::from_secret(&secret(&[("app-id", "12345"), ("private-key", &pem)])).unwrap();
        assert_eq!(app.issuer, json!(12345));
        assert!(app.webhook_secret.is_none());
    }

    #[test]
    fn bad_secrets() {
        let (pem, _) = pkcs8_pem();
        for data in [
            &[("private-key", pem.as_str())][..],
            &[("app-id", "1")],
            &[("app-id", "x"), ("private-key", &pem)],
            &[("app-id", "1"), ("private-key", "not a key")],
            &[
                ("app-id", "1"),
                (
                    "private-key",
                    "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----",
                ),
            ],
            &[
                ("app-id", "1"),
                ("private-key", &pem),
                ("api-url", "http://ghe.example.com/api/v3"),
            ],
        ] {
            assert!(AppConfig::from_secret(&secret(data)).is_err(), "{data:?}");
        }
        let debug = format!(
            "{:?}",
            AppConfig::from_secret(&secret(&[
                ("app-id", "1"),
                ("private-key", &pem),
                ("webhook-secret", "hush")
            ]))
            .unwrap()
        );
        assert!(!debug.contains("hush") && !debug.contains("PRIVATE"));
    }

    #[test]
    fn repositories_on_the_apps_server() {
        let (pem, _) = pkcs8_pem();
        let dotcom =
            AppConfig::from_secret(&secret(&[("app-id", "1"), ("private-key", &pem)])).unwrap();
        let key = |u: &str| RepoKey::parse(u).unwrap();
        assert_eq!(
            dotcom
                .repository(&key("https://github.com/Acme/API.git"))
                .unwrap(),
            ("acme", "api")
        );
        assert!(
            dotcom
                .repository(&key("https://gitlab.com/acme/api"))
                .is_err()
        );
        assert!(
            dotcom
                .repository(&key("https://github.com/acme/api/extra"))
                .is_err()
        );
        let ghes = AppConfig::from_secret(&secret(&[
            ("app-id", "1"),
            ("private-key", &pem),
            ("api-url", "https://GHE.example.com/api/v3/"),
        ]))
        .unwrap();
        assert_eq!(ghes.api_url, "https://GHE.example.com/api/v3");
        assert!(
            ghes.repository(&key("https://ghe.example.com/team/app"))
                .is_ok()
        );
        assert!(
            ghes.repository(&key("https://github.com/team/app"))
                .is_err()
        );
    }

    #[test]
    fn grants() {
        assert!(granted("team-a, team-b", "team-b"));
        assert!(granted("team-a\nteam-b", "team-a"));
        assert!(granted("*", "anyone"));
        assert!(!granted("team-a", "team-ab"));
        assert!(!granted("", "team-a"));
    }

    #[test]
    fn status_descriptions_are_cut() {
        let body = status_body(State::Failure, "zeabur-builder/a/b", &"x".repeat(500));
        assert_eq!(body["state"], "failure");
        assert_eq!(body["description"].as_str().unwrap().len(), MAX_DESCRIPTION);
    }

    #[test]
    fn token_permissions() {
        assert_eq!(Access::Fetch.permissions(), json!({"contents": "read"}));
        assert_eq!(
            Access::Controller.permissions(),
            json!({"contents": "read", "statuses": "write"})
        );
    }
}
