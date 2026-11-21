//! Pulls artifacts from an OCI registry ([distribution spec], "Pulling manifests" and "Pulling
//! blobs"), and pushes the plan artifact ("Pushing blobs" monolithically, "Pushing manifests").
//!
//! - **Credentials.** The run's `kubernetes.io/dockerconfigjson` Secret, mounted at
//!   `$DOCKER_CONFIG/config.json`. A `registrytoken` goes to the registry as the bearer token. Else
//!   `auth`, or `username` and `password`, go as basic auth. If the registry asks for a bearer
//!   token (`WWW-Authenticate: Bearer`), the basic credentials buy a pull token, or a pull and push
//!   token for a push, from its realm ([token authentication]).
//! - **Trust.** The system's CAs and the PEM in `ZB_REGISTRY_CA`, the CA buildkitd trusts.
//! - **Checks.** The manifest must hash to the run's digest and the layer to its own digest and
//!   size. The layer goes to a file first, so nothing unverified is unpacked.
//! - **Redirects.** Blob downloads may redirect to storage, such as S3. Credentials never go to
//!   another host, and `https` never becomes `http`.
//!
//! - **Uploads.** The upload URL the registry gives must be on its own host and scheme, so
//!   credentials never go elsewhere.
//!
//! [distribution spec]: https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md#pull
//! [token authentication]: https://distribution.github.io/distribution/spec/auth/token/

use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use http::{HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use zeabur_builder_core::archive::{
    self, ArchiveError, EMPTY_CONFIG_MEDIA_TYPE, LAYER_MEDIA_TYPE, Layer, MANIFEST_MEDIA_TYPE,
    MAX_MANIFEST_BYTES,
};
use zeabur_builder_core::result::{MAX_EXPOSED_PORTS, is_digest};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// A network policy can admit a new Pod some seconds after it starts, and refuse it until then.
const CONNECT_RETRY: Duration = Duration::from_secs(1);
const CONNECT_RETRIES: u32 = 30;
const MAX_REDIRECTS: usize = 5;
const MAX_TOKEN_BYTES: u64 = 1 << 20;
const MAX_CONFIG_BYTES: u64 = 4 << 20;
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json";
const USER_AGENT: &str = concat!("zeabur-builder-job/", env!("CARGO_PKG_VERSION"));

type HttpClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// Why a registry call failed. The controller shows the message as the run's message.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RegistryError {
    #[error("cannot read the registry CA: {0}")]
    ReadCa(#[source] rustls::pki_types::pem::Error),
    #[error("cannot use the registry CA: {0}")]
    UseCa(#[source] rustls::Error),
    #[error("cannot set up TLS: {0}")]
    Tls(#[source] rustls::Error),
    #[error("cannot read the Docker config: {0}")]
    DockerConfig(#[source] serde_json::Error),
    #[error("`{0}` is not <host>/<name>")]
    Repository(String),
    #[error("invalid credentials")]
    Credentials,
    #[error("{url}: {source}")]
    Request { url: String, source: http::Error },
    #[error("{url}: {source:?}")]
    Send {
        url: String,
        source: hyper_util::client::legacy::Error,
    },
    #[error("{url} answered {status}")]
    Status { url: String, status: StatusCode },
    #[error("{url} answered {status} without a Location")]
    RedirectWithoutLocation { url: String, status: StatusCode },
    #[error("redirect to {next}: {source}")]
    RedirectTarget {
        next: String,
        source: http::uri::InvalidUri,
    },
    #[error("{url} redirects to {next}, which is not https")]
    Downgrade { url: String, next: String },
    #[error("{0}: too many redirects")]
    TooManyRedirects(String),
    #[error("cannot read the token: {0}")]
    ReadToken(#[source] serde_json::Error),
    #[error("{0} gave no token")]
    NoToken(String),
    #[error("{0} answered without a Location")]
    UploadWithoutLocation(String),
    #[error("the upload goes to {location}, not to {base}")]
    UploadHost { location: String, base: String },
    #[error("the blob upload answered {0}")]
    BlobUpload(StatusCode),
    #[error("cannot write the manifest: {0}")]
    WriteManifest(#[source] serde_json::Error),
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error("the manifest does not match {0}")]
    ManifestDigest(String),
    #[error("cannot read the manifest: {0}")]
    ReadManifest(#[source] serde_json::Error),
    #[error("the registry did not answer within {0}s")]
    Timeout(u64),
    #[error("the index has no image")]
    NoImage,
    #[error("the manifest has no config")]
    NoConfig,
    #[error("`{0}` is not a digest")]
    NotDigest(String),
    #[error("the image config does not match {0}")]
    ConfigDigest(String),
    #[error("cannot read the image config: {0}")]
    ReadImageConfig(#[source] serde_json::Error),
    #[error("cannot create {}: {source}", path.display())]
    CreateFile { path: PathBuf, source: io::Error },
    #[error("cannot write {}: {source}", path.display())]
    WriteFile { path: PathBuf, source: io::Error },
    #[error("cannot download the archive: {0}")]
    Download(#[source] hyper::Error),
    #[error("the archive is larger than the {0} bytes its manifest gives")]
    LayerTooLarge(u64),
    #[error("the archive has {size} bytes, not the {expected} its manifest gives")]
    LayerSize { size: u64, expected: u64 },
    #[error("the archive does not match {0}")]
    LayerDigest(String),
    #[error("cannot read the answer: {0}")]
    ReadAnswer(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// A client that trusts the system's CAs and `ca`, a PEM bundle.
pub(crate) fn client(ca: Option<&str>) -> Result<HttpClient, RegistryError> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        // A system certificate rustls cannot parse is skipped, as the controller does not need it.
        drop(roots.add(cert));
    }
    if let Some(ca) = ca {
        for cert in CertificateDer::pem_slice_iter(ca.as_bytes()) {
            let cert = cert.map_err(RegistryError::ReadCa)?;
            roots.add(cert).map_err(RegistryError::UseCa)?;
        }
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(RegistryError::Tls)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(Some(CONNECT_TIMEOUT));
    let https = HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    Ok(Client::builder(TokioExecutor::new()).build(https))
}

#[derive(Deserialize)]
struct DockerConfig {
    #[serde(default)]
    auths: BTreeMap<String, AuthEntry>,
}

#[derive(Deserialize)]
struct AuthEntry {
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    registrytoken: Option<String>,
}

/// The credentials for one registry host, as `Authorization` values.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Credentials {
    /// Sent as is; a challenge buys a token with them.
    Basic(String),
    /// A token the registry takes as it is.
    Bearer(String),
}

/// A token endpoint's answer. Docker's registry writes `token`, OAuth servers `access_token`.
#[derive(Deserialize)]
struct Token {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

/// The credentials for `host` in a Docker config: its `registrytoken`, else its `auth`, or
/// `username` and `password`. A key may carry a scheme and a path, as `docker login` writes for
/// Docker Hub.
pub(crate) fn credentials(config: &[u8], host: &str) -> Result<Option<Credentials>, RegistryError> {
    let config: DockerConfig =
        serde_json::from_slice(config).map_err(RegistryError::DockerConfig)?;
    let entry = config.auths.into_iter().find_map(|(key, entry)| {
        let key = key
            .strip_prefix("https://")
            .or_else(|| key.strip_prefix("http://"))
            .unwrap_or(&key);
        (key.split('/').next() == Some(host)).then_some(entry)
    });
    Ok(entry.and_then(|entry| {
        if let Some(token) = entry.registrytoken {
            return Some(Credentials::Bearer(format!("Bearer {token}")));
        }
        let pair = entry.auth.or_else(|| {
            let pair = format!("{}:{}", entry.username?, entry.password?);
            Some(STANDARD.encode(pair))
        })?;
        Some(Credentials::Basic(format!("Basic {pair}")))
    }))
}

/// A bearer challenge.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Challenge {
    realm: String,
    service: Option<String>,
}

/// Reads `Bearer realm="…",service="…"` from a `WWW-Authenticate` header ([RFC 6750] §3).
///
/// [RFC 6750]: https://www.rfc-editor.org/rfc/rfc6750#section-3
pub(crate) fn bearer_challenge(value: &str) -> Option<Challenge> {
    let (scheme, rest) = value.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let mut params = BTreeMap::new();
    let mut rest = rest.trim_start();
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let (value, after) = if let Some(quoted) = after.strip_prefix('"') {
            let end = quoted.find('"')?;
            (quoted.get(..end)?, quoted.get(end + 1..)?)
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (after.get(..end)?, after.get(end..)?)
        };
        params.insert(key.trim().to_ascii_lowercase(), value.to_owned());
        rest = after.trim_start_matches([',', ' ']);
    }
    Some(Challenge {
        realm: params.remove("realm")?,
        service: params.remove("service"),
    })
}

/// Percent-encodes a query value: everything except the unreserved characters of RFC 3986.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// One request: its method, headers and body.
struct Call<'a> {
    method: Method,
    url: &'a str,
    accept: Option<&'a str>,
    content_type: Option<&'a str>,
    body: Bytes,
}

impl<'a> Call<'a> {
    const fn get(url: &'a str, accept: Option<&'a str>) -> Self {
        Self {
            method: Method::GET,
            url,
            accept,
            content_type: None,
            body: Bytes::new(),
        }
    }
}

/// An OCI descriptor.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    #[serde(default)]
    media_type: String,
    digest: String,
    #[serde(default)]
    size: u64,
}

/// The image manifest of an artifact with one layer ([OCI image spec], "Guidelines for artifact
/// usage").
///
/// [OCI image spec]: https://github.com/opencontainers/image-spec/blob/v1.1.1/manifest.md#guidelines-for-artifact-usage
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactManifest<'a> {
    schema_version: u32,
    media_type: &'a str,
    artifact_type: &'a str,
    config: Descriptor,
    layers: [Descriptor; 1],
}

/// One repository of one registry.
pub(crate) struct Repository {
    client: HttpClient,
    scheme: &'static str,
    host: String,
    name: String,
    basic: Option<String>,
    bearer: Option<String>,
}

impl std::fmt::Debug for Repository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repository")
            .field("host", &self.host)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Repository {
    /// `repository` is `<host>/<name>`.
    pub(crate) fn new(
        client: HttpClient,
        repository: &str,
        plain_http: bool,
        credentials: Option<Credentials>,
    ) -> Result<Self, RegistryError> {
        let (host, name) = repository
            .split_once('/')
            .filter(|(host, name)| !host.is_empty() && !name.is_empty())
            .ok_or_else(|| RegistryError::Repository(repository.to_owned()))?;
        let (basic, bearer) = match credentials {
            Some(Credentials::Basic(basic)) => (Some(basic), None),
            Some(Credentials::Bearer(bearer)) => (None, Some(bearer)),
            None => (None, None),
        };
        Ok(Self {
            client,
            scheme: if plain_http { "http" } else { "https" },
            host: host.to_owned(),
            name: name.to_owned(),
            basic,
            bearer,
        })
    }

    async fn send(
        &self,
        call: &Call<'_>,
        authorization: Option<&str>,
    ) -> Result<Response<Incoming>, RegistryError> {
        let url = call.url;
        let mut retries = 0;
        loop {
            let mut request = Request::builder()
                .method(call.method.clone())
                .uri(url)
                .header(header::USER_AGENT, USER_AGENT);
            if let Some(accept) = call.accept {
                request = request.header(header::ACCEPT, accept);
            }
            if let Some(content_type) = call.content_type {
                request = request.header(header::CONTENT_TYPE, content_type);
            }
            if let Some(value) = authorization {
                let mut value =
                    HeaderValue::from_str(value).map_err(|_e| RegistryError::Credentials)?;
                value.set_sensitive(true);
                request = request.header(header::AUTHORIZATION, value);
            }
            let request = request
                .body(Full::new(call.body.clone()))
                .map_err(|source| RegistryError::Request {
                    url: url.to_owned(),
                    source,
                })?;
            match self.client.request(request).await {
                Err(e) if e.is_connect() && retries < CONNECT_RETRIES => {
                    retries += 1;
                    tokio::time::sleep(CONNECT_RETRY).await;
                }
                answer => {
                    return answer.map_err(|source| RegistryError::Send {
                        url: url.to_owned(),
                        source,
                    });
                }
            }
        }
    }

    /// GETs `/v2/<name>/<path>`, with a pull token if the registry asks for one, following
    /// redirects.
    async fn get(
        &mut self,
        path: &str,
        accept: Option<&str>,
    ) -> Result<Response<Incoming>, RegistryError> {
        let first = format!("{}://{}/v2/{}/{path}", self.scheme, self.host, self.name);
        let mut url = first.clone();
        let mut same_host = true;
        for _ in 0..=MAX_REDIRECTS {
            let authorization = if same_host {
                self.bearer.as_deref().or(self.basic.as_deref())
            } else {
                None
            };
            let response = self.send(&Call::get(&url, accept), authorization).await?;
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED && same_host && self.bearer.is_none() {
                let challenge = response
                    .headers()
                    .get(header::WWW_AUTHENTICATE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(bearer_challenge);
                if let Some(challenge) = challenge {
                    self.bearer = Some(self.token(&challenge, "pull").await?);
                    continue;
                }
            }
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| RegistryError::RedirectWithoutLocation {
                        url: url.clone(),
                        status,
                    })?;
                let next = if location.starts_with('/') {
                    format!("{}://{}{location}", self.scheme, self.host)
                } else {
                    location.to_owned()
                };
                let next_uri: http::Uri =
                    next.parse()
                        .map_err(|source| RegistryError::RedirectTarget {
                            next: next.clone(),
                            source,
                        })?;
                if self.scheme == "https" && next_uri.scheme_str() != Some("https") {
                    return Err(RegistryError::Downgrade { url, next });
                }
                same_host &= next_uri.authority().map(http::uri::Authority::as_str)
                    == Some(self.host.as_str());
                url = next;
                continue;
            }
            if !status.is_success() {
                return Err(RegistryError::Status { url: first, status });
            }
            return Ok(response);
        }
        Err(RegistryError::TooManyRedirects(first))
    }

    /// A token for `actions` (`pull`, or `pull,push`) from the challenge's realm.
    async fn token(&self, challenge: &Challenge, actions: &str) -> Result<String, RegistryError> {
        let service = challenge
            .service
            .as_deref()
            .map_or_else(String::new, |s| format!("&service={}", encode(s)));
        let url = format!(
            "{}{}scope={}{service}",
            challenge.realm,
            if challenge.realm.contains('?') {
                '&'
            } else {
                '?'
            },
            encode(&format!("repository:{}:{actions}", self.name))
        );
        let response = self
            .send(&Call::get(&url, None), self.basic.as_deref())
            .await?;
        if !response.status().is_success() {
            return Err(RegistryError::Status {
                url: challenge.realm.clone(),
                status: response.status(),
            });
        }
        let body = read_limited(response, MAX_TOKEN_BYTES).await?;
        let token: Token = serde_json::from_slice(&body).map_err(RegistryError::ReadToken)?;
        token
            .token
            .or(token.access_token)
            .map(|t| format!("Bearer {t}"))
            .ok_or_else(|| RegistryError::NoToken(challenge.realm.clone()))
    }

    /// Sends `call` with the credentials, buying a pull and push token when the registry asks for
    /// one. Only for URLs on the registry's own host.
    async fn send_for_push(
        &mut self,
        call: &Call<'_>,
    ) -> Result<Response<Incoming>, RegistryError> {
        let response = self
            .send(call, self.bearer.as_deref().or(self.basic.as_deref()))
            .await?;
        if response.status() != StatusCode::UNAUTHORIZED || self.bearer.is_some() {
            return Ok(response);
        }
        let Some(challenge) = response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .and_then(bearer_challenge)
        else {
            return Ok(response);
        };
        self.bearer = Some(self.token(&challenge, "pull,push").await?);
        self.send(call, self.bearer.as_deref()).await
    }

    /// Uploads `blob` in one request and returns its descriptor.
    async fn push_blob(
        &mut self,
        media_type: &str,
        blob: Bytes,
    ) -> Result<Descriptor, RegistryError> {
        let base = format!("{}://{}", self.scheme, self.host);
        let start = format!("{base}/v2/{}/blobs/uploads/", self.name);
        let response = self
            .send_for_push(&Call {
                method: Method::POST,
                url: &start,
                accept: None,
                content_type: None,
                body: Bytes::new(),
            })
            .await?;
        if response.status() != StatusCode::ACCEPTED {
            return Err(RegistryError::Status {
                url: start,
                status: response.status(),
            });
        }
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| RegistryError::UploadWithoutLocation(start.clone()))?;
        let location = if location.starts_with('/') {
            format!("{base}{location}")
        } else {
            location.to_owned()
        };
        if !location.starts_with(&format!("{base}/")) {
            return Err(RegistryError::UploadHost { location, base });
        }
        let digest = archive::sha256(&blob);
        let size = u64::try_from(blob.len()).unwrap_or(u64::MAX);
        let url = format!(
            "{location}{}digest={}",
            if location.contains('?') { '&' } else { '?' },
            encode(&digest)
        );
        let response = self
            .send_for_push(&Call {
                method: Method::PUT,
                url: &url,
                accept: None,
                content_type: Some("application/octet-stream"),
                body: blob,
            })
            .await?;
        if response.status() != StatusCode::CREATED {
            return Err(RegistryError::BlobUpload(response.status()));
        }
        Ok(Descriptor {
            media_type: media_type.to_owned(),
            digest,
            size,
        })
    }

    /// Pushes an artifact of `artifact_type` with `layer`, a gzip tar, as its one layer, tagged
    /// `tag`, and returns the manifest's digest. The form is the one [`archive::layer`] reads.
    pub(crate) async fn push_artifact(
        &mut self,
        tag: &str,
        artifact_type: &str,
        layer: Bytes,
    ) -> Result<String, RegistryError> {
        let config = self
            .push_blob(EMPTY_CONFIG_MEDIA_TYPE, Bytes::from_static(b"{}"))
            .await?;
        let layer = self.push_blob(LAYER_MEDIA_TYPE, layer).await?;
        let manifest = serde_json::to_vec(&ArtifactManifest {
            schema_version: 2,
            media_type: MANIFEST_MEDIA_TYPE,
            artifact_type,
            config,
            layers: [layer],
        })
        .map_err(RegistryError::WriteManifest)?;
        let digest = archive::sha256(&manifest);
        let url = format!(
            "{}://{}/v2/{}/manifests/{tag}",
            self.scheme, self.host, self.name
        );
        let response = self
            .send_for_push(&Call {
                method: Method::PUT,
                url: &url,
                accept: None,
                content_type: Some(MANIFEST_MEDIA_TYPE),
                body: Bytes::from(manifest),
            })
            .await?;
        if response.status() != StatusCode::CREATED {
            return Err(RegistryError::Status {
                url,
                status: response.status(),
            });
        }
        Ok(digest)
    }

    /// The one layer of the manifest with `digest`, an artifact of `artifact_type`.
    pub(crate) async fn layer(
        &mut self,
        digest: &str,
        artifact_type: &str,
    ) -> Result<Layer, RegistryError> {
        let response = self
            .get(&format!("manifests/{digest}"), Some(MANIFEST_MEDIA_TYPE))
            .await?;
        let body = read_limited(response, MAX_MANIFEST_BYTES).await?;
        Ok(archive::layer(&body, digest, artifact_type)?)
    }

    /// A manifest or index with `digest`, checked against it.
    async fn manifest_json(&mut self, digest: &str) -> Result<ImageManifest, RegistryError> {
        let response = self
            .get(&format!("manifests/{digest}"), Some(MANIFEST_ACCEPT))
            .await?;
        let body = read_limited(response, MAX_MANIFEST_BYTES).await?;
        if archive::sha256(&body) != digest {
            return Err(RegistryError::ManifestDigest(digest.to_owned()));
        }
        serde_json::from_slice(&body).map_err(RegistryError::ReadManifest)
    }

    /// The `ExposedPorts` of the image with `digest`, or an error once `within` passes: the client
    /// only bounds connecting, and a registry that stalls mid-response must not hold the build.
    pub(crate) async fn exposed_ports(
        &mut self,
        digest: &str,
        within: Duration,
    ) -> Result<Vec<String>, RegistryError> {
        tokio::time::timeout(within, self.read_exposed_ports(digest))
            .await
            .map_err(|_elapsed| RegistryError::Timeout(within.as_secs()))?
    }

    /// An index gives its first image that is not an attestation.
    async fn read_exposed_ports(&mut self, digest: &str) -> Result<Vec<String>, RegistryError> {
        let mut manifest = self.manifest_json(digest).await?;
        if manifest.config.is_none() {
            let entry = manifest
                .manifests
                .iter()
                .find(|m| m.platform.as_ref().is_some_and(|p| p.os != "unknown"))
                .ok_or(RegistryError::NoImage)?;
            if !is_digest(&entry.digest) {
                return Err(RegistryError::NotDigest(entry.digest.clone()));
            }
            let entry = entry.digest.clone();
            manifest = self.manifest_json(&entry).await?;
        }
        let config = manifest.config.ok_or(RegistryError::NoConfig)?;
        if !is_digest(&config.digest) {
            return Err(RegistryError::NotDigest(config.digest));
        }
        let response = self.get(&format!("blobs/{}", config.digest), None).await?;
        let body = read_limited(response, MAX_CONFIG_BYTES).await?;
        if archive::sha256(&body) != config.digest {
            return Err(RegistryError::ConfigDigest(config.digest));
        }
        exposed_ports(&body)
    }

    /// Downloads the layer to `to`, checks its size and digest, and removes the file if it fails.
    pub(crate) async fn download(&mut self, layer: &Layer, to: &Path) -> Result<(), RegistryError> {
        let written = self.download_unchecked(layer, to).await;
        if written.is_err() {
            drop(std::fs::remove_file(to));
        }
        written
    }

    async fn download_unchecked(&mut self, layer: &Layer, to: &Path) -> Result<(), RegistryError> {
        let mut response = self.get(&format!("blobs/{}", layer.digest), None).await?;
        let mut file =
            std::fs::File::create_new(to).map_err(|source| RegistryError::CreateFile {
                path: to.to_path_buf(),
                source,
            })?;
        let mut hash = Sha256::new();
        let mut size = 0_u64;
        while let Some(frame) = response.body_mut().frame().await {
            let frame = frame.map_err(RegistryError::Download)?;
            let Ok(data) = frame.into_data() else {
                continue;
            };
            size += u64::try_from(data.len()).unwrap_or(u64::MAX);
            if size > layer.size {
                return Err(RegistryError::LayerTooLarge(layer.size));
            }
            hash.update(&data);
            file.write_all(&data)
                .map_err(|source| RegistryError::WriteFile {
                    path: to.to_path_buf(),
                    source,
                })?;
        }
        if size != layer.size {
            return Err(RegistryError::LayerSize {
                size,
                expected: layer.size,
            });
        }
        if format!("sha256:{}", archive::hex(&hash.finalize())) != layer.digest {
            return Err(RegistryError::LayerDigest(layer.digest.clone()));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct ImageManifest {
    #[serde(default)]
    config: Option<Descriptor>,
    #[serde(default)]
    manifests: Vec<IndexEntry>,
}

#[derive(Deserialize)]
struct IndexEntry {
    digest: String,
    #[serde(default)]
    platform: Option<Platform>,
}

#[derive(Deserialize)]
struct Platform {
    #[serde(default)]
    os: String,
}

#[derive(Deserialize)]
struct ImageConfig {
    #[serde(default)]
    config: Option<ContainerConfig>,
}

#[derive(Deserialize)]
#[expect(clippy::zero_sized_map_values, reason = "only the keys are read")]
struct ContainerConfig {
    #[serde(rename = "ExposedPorts", default)]
    exposed_ports: Option<BTreeMap<String, serde::de::IgnoredAny>>,
}

/// The `ExposedPorts` keys of an image config, sorted, without keys that are not `<port>/<proto>`
/// or `<low>-<high>/<proto>`.
#[expect(clippy::zero_sized_map_values, reason = "only the keys are read")]
fn exposed_ports(config: &[u8]) -> Result<Vec<String>, RegistryError> {
    let config: ImageConfig =
        serde_json::from_slice(config).map_err(RegistryError::ReadImageConfig)?;
    let mut ports: Vec<String> = config
        .config
        .and_then(|c| c.exposed_ports)
        .into_iter()
        .flat_map(BTreeMap::into_keys)
        .filter(|key| is_port_key(key))
        .collect();
    ports.truncate(MAX_EXPOSED_PORTS);
    Ok(ports)
}

fn is_port_key(key: &str) -> bool {
    let Some((ports, protocol)) = key.split_once('/') else {
        return false;
    };
    let numbers = ports.split('-').count() <= 2
        && ports
            .split('-')
            .all(|n| !n.is_empty() && n.len() <= 5 && n.bytes().all(|b| b.is_ascii_digit()));
    numbers
        && !protocol.is_empty()
        && protocol.len() <= 8
        && protocol.bytes().all(|b| b.is_ascii_lowercase())
}

/// The body, refused when it is longer than `limit`.
async fn read_limited(response: Response<Incoming>, limit: u64) -> Result<Bytes, RegistryError> {
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    http_body_util::Limited::new(response.into_body(), limit)
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .map_err(RegistryError::ReadAnswer)
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::net::SocketAddr;

    use http_body_util::Full;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use serde_json::json;
    use zeabur_builder_core::archive::{
        ARTIFACT_TYPE, EMPTY_CONFIG_MEDIA_TYPE, LAYER_MEDIA_TYPE, sha256,
    };

    use super::*;

    const NAME: &str = "team-a/api/source";
    const BASIC: &str = "Basic dGVhbS1hOnNlY3JldA==";

    fn bytes(value: &impl Serialize) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    #[test]
    fn credentials_for_the_registry_host() {
        let config = &bytes(&json!({"auths": {
            "https://index.docker.io/v1/": {"auth": "aHViOmh1Yg=="},
            "zot.zot.svc:5000": {"username": "team-a", "password": "secret"},
            "registry.example": {"registrytoken": "eyJ.x.y", "username": "u", "password": "p"},
        }}));
        assert_eq!(
            credentials(config, "zot.zot.svc:5000").unwrap(),
            Some(Credentials::Basic(BASIC.to_owned()))
        );
        assert_eq!(
            credentials(config, "index.docker.io").unwrap(),
            Some(Credentials::Basic("Basic aHViOmh1Yg==".to_owned()))
        );
        assert_eq!(
            credentials(config, "registry.example").unwrap(),
            Some(Credentials::Bearer("Bearer eyJ.x.y".to_owned()))
        );
        assert_eq!(credentials(config, "zot.zot.svc").unwrap(), None);
        assert_eq!(
            credentials(&bytes(&json!({"auths": {}})), "zot").unwrap(),
            None
        );
    }

    #[test]
    fn bearer_challenges() {
        assert_eq!(
            bearer_challenge(
                r#"Bearer realm="https://auth.example/token?a=1,b",service="registry.example",scope="repository:x:pull""#
            ),
            Some(Challenge {
                realm: "https://auth.example/token?a=1,b".to_owned(),
                service: Some("registry.example".to_owned()),
            })
        );
        assert_eq!(
            bearer_challenge("bearer realm=https://auth.example/token"),
            Some(Challenge {
                realm: "https://auth.example/token".to_owned(),
                service: None,
            })
        );
        assert_eq!(bearer_challenge(r#"Basic realm="zot""#), None);
        assert_eq!(bearer_challenge(r#"Bearer service="x""#), None);
    }

    struct Fake {
        manifest: Vec<u8>,
        blob: Vec<u8>,
        addr: SocketAddr,
    }

    fn reply(
        status: StatusCode,
        headers: &[(&str, String)],
        body: Vec<u8>,
    ) -> Response<Full<Bytes>> {
        let mut response = Response::builder().status(status);
        for (name, value) in headers {
            response = response.header(*name, value);
        }
        response.body(Full::new(Bytes::from(body))).unwrap()
    }

    /// A registry that wants a bearer token for `team-a/api/source`, sold for [`BASIC`], and that
    /// redirects blobs to storage on another host name, which refuses credentials.
    fn handle(fake: &Fake, request: &Request<Incoming>) -> Response<Full<Bytes>> {
        let auth = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        let path = request.uri().path();
        let query = request.uri().query().unwrap_or_default();
        let port = fake.addr.port();
        if path == "/token" {
            let scope = format!("scope={}", encode(&format!("repository:{NAME}:pull")));
            return if auth == Some(BASIC)
                && query.contains(&scope)
                && query.contains("service=fake")
            {
                reply(StatusCode::OK, &[], bytes(&json!({"token": "t0k"})))
            } else {
                reply(StatusCode::FORBIDDEN, &[], Vec::new())
            };
        }
        if path == "/storage/blob" {
            return if auth.is_none() {
                reply(StatusCode::OK, &[], fake.blob.clone())
            } else {
                reply(StatusCode::BAD_REQUEST, &[], Vec::new())
            };
        }
        if auth != Some("Bearer t0k") {
            let challenge =
                format!(r#"Bearer realm="http://127.0.0.1:{port}/token",service="fake""#);
            return reply(
                StatusCode::UNAUTHORIZED,
                &[("www-authenticate", challenge)],
                Vec::new(),
            );
        }
        let Some(rest) = path.strip_prefix(&format!("/v2/{NAME}/")) else {
            return reply(StatusCode::NOT_FOUND, &[], Vec::new());
        };
        if rest.starts_with("manifests/") {
            reply(StatusCode::OK, &[], fake.manifest.clone())
        } else if rest.starts_with("blobs/") {
            let location = format!("http://localhost:{port}/storage/blob");
            reply(
                StatusCode::TEMPORARY_REDIRECT,
                &[("location", location)],
                Vec::new(),
            )
        } else {
            reply(StatusCode::NOT_FOUND, &[], Vec::new())
        }
    }

    async fn serve(manifest: Vec<u8>, blob: Vec<u8>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        serve_on(listener, manifest, blob)
    }

    fn serve_on(listener: tokio::net::TcpListener, manifest: Vec<u8>, blob: Vec<u8>) -> SocketAddr {
        let addr = listener.local_addr().unwrap();
        let fake = Arc::new(Fake {
            manifest,
            blob,
            addr,
        });
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let fake = Arc::clone(&fake);
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let response = handle(&fake, &request);
                        async move { Ok::<_, Infallible>(response) }
                    });
                    drop(
                        hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await,
                    );
                });
            }
        });
        addr
    }

    fn manifest(blob: &[u8]) -> Vec<u8> {
        bytes(&json!({
            "schemaVersion": 2,
            "mediaType": MANIFEST_MEDIA_TYPE,
            "artifactType": ARTIFACT_TYPE,
            "config": {"mediaType": EMPTY_CONFIG_MEDIA_TYPE, "digest": sha256(b"{}"), "size": 2},
            "layers": [{"mediaType": LAYER_MEDIA_TYPE, "digest": sha256(blob), "size": blob.len()}],
        }))
    }

    async fn pull(addr: SocketAddr, digest: &str, to: &Path) -> Result<(), RegistryError> {
        let mut repository = Repository::new(
            client(None).unwrap(),
            &format!("127.0.0.1:{}/{NAME}", addr.port()),
            true,
            Some(Credentials::Basic(BASIC.to_owned())),
        )
        .unwrap();
        let layer = repository.layer(digest, ARTIFACT_TYPE).await?;
        repository.download(&layer, to).await
    }

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "zeabur-builder-registry-{}-{name}",
            std::process::id()
        ));
        drop(std::fs::remove_file(&path));
        path
    }

    #[tokio::test]
    async fn pulls_with_a_token_and_follows_redirects_without_credentials() {
        let blob = b"the archive".to_vec();
        let manifest = manifest(&blob);
        let digest = sha256(&manifest);
        let addr = serve(manifest, blob.clone()).await;
        let to = scratch("ok");
        pull(addr, &digest, &to).await.unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), blob);
        std::fs::remove_file(&to).unwrap();
    }

    #[tokio::test]
    async fn refuses_content_that_does_not_match() {
        let blob = b"the archive".to_vec();
        let manifest = manifest(&blob);
        let digest = sha256(&manifest);

        let swapped = serve(manifest.clone(), b"the ARCHIVE".to_vec()).await;
        let to = scratch("swapped");
        assert!(pull(swapped, &digest, &to).await.is_err());
        assert!(!to.exists());

        let longer = serve(manifest.clone(), b"the archive and more".to_vec()).await;
        assert!(pull(longer, &digest, &to).await.is_err());
        assert!(!to.exists());

        let honest = serve(manifest, blob).await;
        let other = sha256(b"another manifest");
        assert!(pull(honest, &other, &to).await.is_err());
        assert!(!to.exists());
    }

    #[test]
    fn exposed_ports_of_a_config() {
        let config = json!({"config": {"ExposedPorts": {
            "53/udp": {}, "8000-8010/tcp": {}, "3000/tcp": {}, "../x": {}, "80": {},
        }}});
        assert_eq!(
            exposed_ports(&bytes(&config)).unwrap(),
            ["3000/tcp", "53/udp", "8000-8010/tcp"]
        );
        assert_eq!(
            exposed_ports(&bytes(&json!({"config": {}}))).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            exposed_ports(&bytes(&json!({"config": {"ExposedPorts": null}}))).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(exposed_ports(b"{}").unwrap(), Vec::<String>::new());
        assert!(exposed_ports(b"nope").is_err());
    }

    /// A registry that serves `blobs` and `manifests` by their digests under [`NAME`], with the
    /// bearer dance skipped.
    async fn serve_images(items: Vec<(String, Vec<u8>)>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let items = Arc::new(items);
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let items = Arc::clone(&items);
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let digest = request.uri().path().rsplit('/').next().unwrap_or_default();
                        let body = items
                            .iter()
                            .find(|(d, _)| d == digest)
                            .map(|(_, b)| b.clone());
                        async move {
                            Ok::<_, Infallible>(body.map_or_else(
                                || reply(StatusCode::NOT_FOUND, &[], Vec::new()),
                                |body| reply(StatusCode::OK, &[], body),
                            ))
                        }
                    });
                    drop(
                        hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await,
                    );
                });
            }
        });
        addr
    }

    async fn ports(addr: SocketAddr, digest: &str) -> Result<Vec<String>, RegistryError> {
        let mut repository = Repository::new(
            client(None).unwrap(),
            &format!("127.0.0.1:{}/{NAME}", addr.port()),
            true,
            None,
        )
        .unwrap();
        repository
            .exposed_ports(digest, Duration::from_secs(10))
            .await
    }

    #[tokio::test]
    async fn reads_the_ports_of_an_image_and_of_an_index_past_attestations() {
        let config = bytes(&json!({"config": {"ExposedPorts": {"3000/tcp": {}, "53/udp": {}}}}));
        let config_digest = sha256(&config);
        let image = bytes(&json!({
            "schemaVersion": 2,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest,
                "size": config.len(),
            },
            "layers": [],
        }));
        let image_digest = sha256(&image);
        let attestation = bytes(&json!({
            "schemaVersion": 2,
            "config": {"digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000"},
        }));
        let attestation_digest = sha256(&attestation);
        let index = bytes(&json!({
            "schemaVersion": 2,
            "manifests": [
                {"digest": attestation_digest, "platform": {"os": "unknown", "architecture": "unknown"}},
                {"digest": image_digest, "platform": {"os": "linux", "architecture": "amd64"}},
            ],
        }));
        let index_digest = sha256(&index);
        let addr = serve_images(vec![
            (config_digest, config),
            (image_digest.clone(), image),
            (attestation_digest, attestation),
            (index_digest.clone(), index),
        ])
        .await;
        let expected = ["3000/tcp", "53/udp"];
        assert_eq!(ports(addr, &image_digest).await.unwrap(), expected);
        assert_eq!(ports(addr, &index_digest).await.unwrap(), expected);
        assert!(ports(addr, &sha256(b"missing")).await.is_err());
    }

    #[tokio::test]
    async fn refuses_a_config_that_does_not_match() {
        let config = bytes(&json!({"config": {"ExposedPorts": {"3000/tcp": {}}}}));
        let config_digest = sha256(&config);
        let image = bytes(&json!({
            "schemaVersion": 2,
            "config": {"digest": config_digest},
            "layers": [],
        }));
        let image_digest = sha256(&image);
        let addr = serve_images(vec![
            (
                config_digest,
                bytes(&json!({"config": {"ExposedPorts": {"22/tcp": {}}}})),
            ),
            (image_digest.clone(), image),
        ])
        .await;
        assert!(ports(addr, &image_digest).await.is_err());
    }

    #[tokio::test]
    async fn gives_up_on_a_registry_that_stalls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _stream = stream;
                    std::future::pending::<()>().await;
                });
            }
        });
        let mut repository = Repository::new(
            client(None).unwrap(),
            &format!("127.0.0.1:{}/{NAME}", addr.port()),
            true,
            None,
        )
        .unwrap();
        let digest = sha256(b"stalled");
        let error = repository
            .exposed_ports(&digest, Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("did not answer"), "{error}");
    }

    #[tokio::test]
    async fn waits_for_a_registry_that_refuses_connections_at_first() {
        let blob = b"the archive".to_vec();
        let manifest = manifest(&blob);
        let digest = sha256(&manifest);
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let late = blob.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            serve_on(listener, manifest, late);
        });
        let to = scratch("late");
        pull(addr, &digest, &to).await.unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), blob);
        std::fs::remove_file(&to).unwrap();
    }

    /// What a pushing fake registry was given: blobs by digest, manifests by tag.
    #[derive(Default)]
    struct Pushed {
        blobs: std::sync::Mutex<BTreeMap<String, Vec<u8>>>,
        manifests: std::sync::Mutex<BTreeMap<String, Vec<u8>>>,
    }

    /// A registry that sells a pull and push token for [`BASIC`] and keeps what is pushed to
    /// [`NAME`]. Its upload URL is relative, or on `localhost` when `elsewhere`.
    async fn handle_push(
        pushed: &Pushed,
        port: u16,
        elsewhere: bool,
        request: Request<Incoming>,
    ) -> Response<Full<Bytes>> {
        let auth = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        let query = request.uri().query().unwrap_or_default().to_owned();
        let body = request.into_body().collect().await.unwrap().to_bytes();
        if path == "/token" {
            let scope = format!("scope={}", encode(&format!("repository:{NAME}:pull,push")));
            return if auth.as_deref() == Some(BASIC) && query.contains(&scope) {
                reply(StatusCode::OK, &[], bytes(&json!({"token": "t0k"})))
            } else {
                reply(StatusCode::FORBIDDEN, &[], Vec::new())
            };
        }
        if auth.as_deref() != Some("Bearer t0k") {
            let challenge = format!(r#"Bearer realm="http://127.0.0.1:{port}/token""#);
            return reply(
                StatusCode::UNAUTHORIZED,
                &[("www-authenticate", challenge)],
                Vec::new(),
            );
        }
        let uploads = format!("/v2/{NAME}/blobs/uploads/");
        let manifests = format!("/v2/{NAME}/manifests/");
        if method == Method::POST && path == uploads {
            let location = if elsewhere {
                format!("http://localhost:{port}/upload/1")
            } else {
                "/upload/1".to_owned()
            };
            return reply(StatusCode::ACCEPTED, &[("location", location)], Vec::new());
        }
        if method == Method::PUT && path == "/upload/1" {
            let digest = query.strip_prefix("digest=").unwrap().replace("%3A", ":");
            assert_eq!(sha256(&body), digest);
            pushed.blobs.lock().unwrap().insert(digest, body.to_vec());
            return reply(StatusCode::CREATED, &[], Vec::new());
        }
        if let Some(tag) = path.strip_prefix(&manifests)
            && method == Method::PUT
        {
            pushed
                .manifests
                .lock()
                .unwrap()
                .insert(tag.to_owned(), body.to_vec());
            return reply(StatusCode::CREATED, &[], Vec::new());
        }
        reply(StatusCode::NOT_FOUND, &[], Vec::new())
    }

    async fn serve_push(elsewhere: bool) -> (SocketAddr, Arc<Pushed>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let pushed = Arc::new(Pushed::default());
        let kept = Arc::clone(&pushed);
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let pushed = Arc::clone(&kept);
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let pushed = Arc::clone(&pushed);
                        async move {
                            let response =
                                handle_push(&pushed, addr.port(), elsewhere, request).await;
                            Ok::<_, Infallible>(response)
                        }
                    });
                    drop(
                        hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await,
                    );
                });
            }
        });
        (addr, pushed)
    }

    fn push_repository(addr: SocketAddr) -> Repository {
        Repository::new(
            client(None).unwrap(),
            &format!("127.0.0.1:{}/{NAME}", addr.port()),
            true,
            Some(Credentials::Basic(BASIC.to_owned())),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn pushes_an_artifact_that_reads_back_as_one() {
        let plan = zeabur_builder_core::plan::ARTIFACT_TYPE;
        let (addr, pushed) = serve_push(false).await;
        let layer = b"the plan".to_vec();
        let digest = push_repository(addr)
            .push_artifact("zeabur-plan-1", plan, Bytes::from(layer.clone()))
            .await
            .unwrap();

        let manifest = pushed.manifests.lock().unwrap()["zeabur-plan-1"].clone();
        let read = archive::layer(&manifest, &digest, plan).unwrap();
        let blobs = pushed.blobs.lock().unwrap();
        assert_eq!(blobs[&read.digest], layer);
        assert_eq!(blobs[&sha256(b"{}")], b"{}");
    }

    #[tokio::test]
    async fn refuses_an_upload_url_on_another_host() {
        let (addr, pushed) = serve_push(true).await;
        let error = push_repository(addr)
            .push_artifact("t", ARTIFACT_TYPE, Bytes::from_static(b"x"))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("not to http://127.0.0.1"),
            "{error}"
        );
        assert!(pushed.blobs.lock().unwrap().is_empty());
    }
}
