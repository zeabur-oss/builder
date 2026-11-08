//! Pulls an archive source from an OCI registry ([distribution spec], "Pulling manifests" and
//! "Pulling blobs").
//!
//! - **Credentials.** The run's `kubernetes.io/dockerconfigjson` Secret, mounted at
//!   `$DOCKER_CONFIG/config.json`. A `registrytoken` goes to the registry as the bearer token. Else
//!   `auth`, or `username` and `password`, go as basic auth. If the registry asks for a bearer
//!   token (`WWW-Authenticate: Bearer`), the basic credentials buy a pull token from its realm
//!   ([token authentication]).
//! - **Trust.** The system's CAs and the PEM in `ZB_REGISTRY_CA`, the CA buildkitd trusts.
//! - **Checks.** The manifest must hash to the run's digest and the layer to its own digest and
//!   size. The layer goes to a file first, so nothing unverified is unpacked.
//! - **Redirects.** Blob downloads may redirect to storage, such as S3. Credentials never go to
//!   another host, and `https` never becomes `http`.
//!
//! [distribution spec]: https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md#pull
//! [token authentication]: https://distribution.github.io/distribution/spec/auth/token/

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use http::{HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt as _, Empty, Full};
use hyper::body::{Body, Frame, Incoming};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use zeabur_builder_core::archive::{self, Layer, MANIFEST_MEDIA_TYPE, MAX_MANIFEST_BYTES};
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

type RequestBody = UnsyncBoxBody<Bytes, std::io::Error>;
type HttpClient = Client<HttpsConnector<HttpConnector>, RequestBody>;

/// What a request sends.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Payload<'a> {
    Empty,
    Bytes(&'a [u8]),
    /// The first `size` bytes of a file, streamed.
    File(&'a Path, u64),
}

/// One request to a repository.
#[derive(Debug, Clone)]
struct Call<'a> {
    method: Method,
    accept: Option<&'a str>,
    content_type: Option<&'a str>,
    payload: Payload<'a>,
    /// A 404 is an answer, not an error.
    missing_ok: bool,
}

impl Call<'_> {
    const fn new(method: Method) -> Self {
        Self {
            method,
            accept: None,
            content_type: None,
            payload: Payload::Empty,
            missing_ok: false,
        }
    }
}

/// A file as a request body.
struct FileBody {
    file: tokio::fs::File,
    left: u64,
}

impl Body for FileBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.left == 0 {
            return Poll::Ready(None);
        }
        let len = usize::try_from(self.left.min(64 << 10)).unwrap_or(64 << 10);
        let mut chunk = vec![0; len];
        let mut buf = tokio::io::ReadBuf::new(&mut chunk);
        match tokio::io::AsyncRead::poll_read(Pin::new(&mut self.file), cx, &mut buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
            Poll::Ready(Ok(())) => {
                let read = buf.filled().len();
                if read == 0 {
                    return Poll::Ready(Some(Err(std::io::ErrorKind::UnexpectedEof.into())));
                }
                chunk.truncate(read);
                self.left -= u64::try_from(read).unwrap_or(u64::MAX);
                Poll::Ready(Some(Ok(Frame::data(Bytes::from(chunk)))))
            }
        }
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        hyper::body::SizeHint::with_exact(self.left)
    }
}

async fn request_body(payload: Payload<'_>) -> Result<RequestBody, String> {
    Ok(match payload {
        Payload::Empty => Empty::new().map_err(|e| match e {}).boxed_unsync(),
        Payload::Bytes(bytes) => Full::new(Bytes::copy_from_slice(bytes))
            .map_err(|e| match e {})
            .boxed_unsync(),
        Payload::File(path, size) => {
            let file = tokio::fs::File::open(path)
                .await
                .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
            FileBody { file, left: size }.boxed_unsync()
        }
    })
}

/// A client that trusts the system's CAs and `ca`, a PEM bundle.
pub(crate) fn client(ca: Option<&str>) -> Result<HttpClient, String> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        // A system certificate rustls cannot parse is skipped, as the controller does not need it.
        drop(roots.add(cert));
    }
    if let Some(ca) = ca {
        for cert in CertificateDer::pem_slice_iter(ca.as_bytes()) {
            let cert = cert.map_err(|e| format!("cannot read the registry CA: {e}"))?;
            roots
                .add(cert)
                .map_err(|e| format!("cannot use the registry CA: {e}"))?;
        }
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
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
pub(crate) fn credentials(config: &[u8], host: &str) -> Result<Option<Credentials>, String> {
    let config: DockerConfig = serde_json::from_slice(config)
        .map_err(|e| format!("cannot read the Docker config: {e}"))?;
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
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push('%');
            for nibble in [b >> 4, b & 0xF] {
                let digit = char::from_digit(u32::from(nibble), 16).unwrap_or('0');
                out.push(digit.to_ascii_uppercase());
            }
        }
    }
    out
}

/// One repository of one registry.
pub(crate) struct Repository {
    client: HttpClient,
    scheme: &'static str,
    host: String,
    name: String,
    basic: Option<String>,
    bearer: Option<String>,
    /// The token scope to ask for on a challenge.
    actions: &'static str,
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
    ) -> Result<Self, String> {
        let (host, name) = repository
            .split_once('/')
            .filter(|(host, name)| !host.is_empty() && !name.is_empty())
            .ok_or_else(|| format!("`{repository}` is not <host>/<name>"))?;
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
            actions: "pull",
        })
    }

    /// Asks for push access too when the registry challenges.
    pub(crate) const fn for_push(mut self) -> Self {
        self.actions = "pull,push";
        self
    }

    async fn send(
        &self,
        url: &str,
        call: &Call<'_>,
        authorization: Option<&str>,
    ) -> Result<Response<Incoming>, String> {
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
                    HeaderValue::from_str(value).map_err(|_e| "invalid credentials".to_owned())?;
                value.set_sensitive(true);
                request = request.header(header::AUTHORIZATION, value);
            }
            let request = request
                .body(request_body(call.payload).await?)
                .map_err(|e| format!("{url}: {e}"))?;
            match self.client.request(request).await {
                Err(e) if e.is_connect() && retries < CONNECT_RETRIES => {
                    retries += 1;
                    tokio::time::sleep(CONNECT_RETRY).await;
                }
                answer => return answer.map_err(|e| format!("{url}: {e:?}")),
            }
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}://{}/v2/{}/{path}", self.scheme, self.host, self.name)
    }

    /// GETs `/v2/<name>/<path>`, with a pull token if the registry asks for one, following
    /// redirects.
    async fn get(
        &mut self,
        path: &str,
        accept: Option<&str>,
    ) -> Result<Response<Incoming>, String> {
        let call = Call {
            accept,
            ..Call::new(Method::GET)
        };
        self.call(self.url(path), &call).await
    }

    /// Sends `call` to `first`. A configured bearer token goes up front, without waiting for a
    /// challenge. Only a GET or HEAD follows redirects, and credentials stay on this host.
    async fn call(&mut self, first: String, call: &Call<'_>) -> Result<Response<Incoming>, String> {
        let follows = matches!(call.method, Method::GET | Method::HEAD);
        let mut url = first.clone();
        let mut same_host = true;
        for _ in 0..=MAX_REDIRECTS {
            let authorization = if same_host {
                self.bearer.as_deref().or(self.basic.as_deref())
            } else {
                None
            };
            let response = self.send(&url, call, authorization).await?;
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED && same_host && self.bearer.is_none() {
                let challenge = response
                    .headers()
                    .get(header::WWW_AUTHENTICATE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(bearer_challenge);
                if let Some(challenge) = challenge {
                    self.bearer = Some(self.token(&challenge).await?);
                    continue;
                }
            }
            if follows && status.is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| format!("{url} answered {status} without a Location"))?;
                let next = if location.starts_with('/') {
                    format!("{}://{}{location}", self.scheme, self.host)
                } else {
                    location.to_owned()
                };
                let next_uri: http::Uri = next
                    .parse()
                    .map_err(|e| format!("redirect to {next}: {e}"))?;
                if self.scheme == "https" && next_uri.scheme_str() != Some("https") {
                    return Err(format!("{url} redirects to {next}, which is not https"));
                }
                same_host &= next_uri.authority().map(http::uri::Authority::as_str)
                    == Some(self.host.as_str());
                url = next;
                continue;
            }
            let missing = call.missing_ok && status == StatusCode::NOT_FOUND;
            if !status.is_success() && !missing {
                return Err(format!("{first} answered {status}"));
            }
            return Ok(response);
        }
        Err(format!("{first}: too many redirects"))
    }

    /// A pull token from the challenge's realm.
    async fn token(&self, challenge: &Challenge) -> Result<String, String> {
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
            encode(&format!("repository:{}:{}", self.name, self.actions))
        );
        let response = self
            .send(&url, &Call::new(Method::GET), self.basic.as_deref())
            .await?;
        if !response.status().is_success() {
            return Err(format!(
                "{} answered {}",
                challenge.realm,
                response.status()
            ));
        }
        let body = read_limited(response, MAX_TOKEN_BYTES).await?;
        let token: Token =
            serde_json::from_slice(&body).map_err(|e| format!("cannot read the token: {e}"))?;
        token
            .token
            .or(token.access_token)
            .map(|t| format!("Bearer {t}"))
            .ok_or_else(|| format!("{} gave no token", challenge.realm))
    }

    /// The manifest at `reference` (a digest or a tag): its media type from the answer, and its
    /// bytes.
    pub(crate) async fn manifest(
        &mut self,
        reference: &str,
    ) -> Result<(Option<String>, Bytes), String> {
        let response = self
            .get(&format!("manifests/{reference}"), Some(MANIFEST_ACCEPT))
            .await?;
        let media_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .map(|v| v.trim().to_owned());
        let body = read_limited(response, MAX_MANIFEST_BYTES).await?;
        Ok((media_type, body))
    }

    /// The `Docker-Content-Digest` of the manifest at `reference`.
    pub(crate) async fn manifest_digest(&mut self, reference: &str) -> Result<String, String> {
        let url = self.url(&format!("manifests/{reference}"));
        let call = Call {
            accept: Some(MANIFEST_ACCEPT),
            ..Call::new(Method::HEAD)
        };
        let response = self.call(url, &call).await?;
        response
            .headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| format!("{reference} has no Docker-Content-Digest"))
    }

    /// Pushes `body` as the manifest at `reference`, byte for byte.
    pub(crate) async fn put_manifest(
        &mut self,
        reference: &str,
        media_type: &str,
        body: &[u8],
    ) -> Result<(), String> {
        let url = self.url(&format!("manifests/{reference}"));
        let call = Call {
            content_type: Some(media_type),
            payload: Payload::Bytes(body),
            ..Call::new(Method::PUT)
        };
        drop(self.call(url, &call).await?);
        Ok(())
    }

    /// Whether the repository has the blob.
    pub(crate) async fn has_blob(&mut self, digest: &str) -> Result<bool, String> {
        let url = self.url(&format!("blobs/{digest}"));
        let call = Call {
            missing_ok: true,
            ..Call::new(Method::HEAD)
        };
        Ok(self.call(url, &call).await?.status().is_success())
    }

    /// Uploads the first `size` bytes of `file` as the blob `digest`, in one request.
    pub(crate) async fn upload(
        &mut self,
        digest: &str,
        file: &Path,
        size: u64,
    ) -> Result<(), String> {
        let start = self.url("blobs/uploads/");
        let response = self.call(start, &Call::new(Method::POST)).await?;
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or("the upload has no Location")?;
        let location = if location.starts_with('/') {
            format!("{}://{}{location}", self.scheme, self.host)
        } else {
            location.to_owned()
        };
        let authority = location
            .parse::<http::Uri>()
            .map_err(|e| format!("upload to {location}: {e}"))?
            .authority()
            .map(http::uri::Authority::to_string);
        if authority.as_deref() != Some(self.host.as_str()) {
            return Err(format!("the upload moves to {location}, another host"));
        }
        let separator = if location.contains('?') { '&' } else { '?' };
        let call = Call {
            content_type: Some("application/octet-stream"),
            payload: Payload::File(file, size),
            ..Call::new(Method::PUT)
        };
        drop(
            self.call(
                format!("{location}{separator}digest={}", encode(digest)),
                &call,
            )
            .await?,
        );
        Ok(())
    }

    /// The archive's layer, from the manifest with `digest`.
    pub(crate) async fn layer(&mut self, digest: &str) -> Result<Layer, String> {
        let response = self
            .get(&format!("manifests/{digest}"), Some(MANIFEST_MEDIA_TYPE))
            .await?;
        let body = read_limited(response, MAX_MANIFEST_BYTES).await?;
        archive::layer(&body, digest).map_err(|e| e.to_string())
    }

    /// A manifest or index with `digest`, checked against it.
    async fn manifest_json(&mut self, digest: &str) -> Result<ImageManifest, String> {
        let response = self
            .get(&format!("manifests/{digest}"), Some(MANIFEST_ACCEPT))
            .await?;
        let body = read_limited(response, MAX_MANIFEST_BYTES).await?;
        if archive::sha256(&body) != digest {
            return Err(format!("the manifest does not match {digest}"));
        }
        serde_json::from_slice(&body).map_err(|e| format!("cannot read the manifest: {e}"))
    }

    /// The `ExposedPorts` of the image with `digest`, or an error once `within` passes: the client
    /// only bounds connecting, and a registry that stalls mid-response must not hold the build.
    pub(crate) async fn exposed_ports(
        &mut self,
        digest: &str,
        within: Duration,
    ) -> Result<Vec<String>, String> {
        tokio::time::timeout(within, self.read_exposed_ports(digest))
            .await
            .map_err(|_elapsed| {
                format!("the registry did not answer within {}s", within.as_secs())
            })?
    }

    /// An index gives its first image that is not an attestation.
    async fn read_exposed_ports(&mut self, digest: &str) -> Result<Vec<String>, String> {
        let mut manifest = self.manifest_json(digest).await?;
        if manifest.config.is_none() {
            let entry = manifest
                .manifests
                .iter()
                .find(|m| m.platform.as_ref().is_some_and(|p| p.os != "unknown"))
                .ok_or("the index has no image")?;
            if !is_digest(&entry.digest) {
                return Err(format!("`{}` is not a digest", entry.digest));
            }
            let entry = entry.digest.clone();
            manifest = self.manifest_json(&entry).await?;
        }
        let config = manifest.config.ok_or("the manifest has no config")?;
        if !is_digest(&config.digest) {
            return Err(format!("`{}` is not a digest", config.digest));
        }
        let response = self.get(&format!("blobs/{}", config.digest), None).await?;
        let body = read_limited(response, MAX_CONFIG_BYTES).await?;
        if archive::sha256(&body) != config.digest {
            return Err(format!("the image config does not match {}", config.digest));
        }
        exposed_ports(&body)
    }

    /// Downloads the layer to `to`, checks its size and digest, and removes the file if it fails.
    pub(crate) async fn download(&mut self, layer: &Layer, to: &Path) -> Result<(), String> {
        let written = self.download_unchecked(layer, to).await;
        if written.is_err() {
            drop(std::fs::remove_file(to));
        }
        written
    }

    async fn download_unchecked(&mut self, layer: &Layer, to: &Path) -> Result<(), String> {
        let mut response = self.get(&format!("blobs/{}", layer.digest), None).await?;
        let mut file = std::fs::File::create_new(to)
            .map_err(|e| format!("cannot create {}: {e}", to.display()))?;
        let mut hash = Sha256::new();
        let mut size = 0_u64;
        while let Some(frame) = response.body_mut().frame().await {
            let frame = frame.map_err(|e| format!("cannot download the archive: {e}"))?;
            let Ok(data) = frame.into_data() else {
                continue;
            };
            size += u64::try_from(data.len()).unwrap_or(u64::MAX);
            if size > layer.size {
                return Err(format!(
                    "the archive is larger than the {} bytes its manifest gives",
                    layer.size
                ));
            }
            hash.update(&data);
            file.write_all(&data)
                .map_err(|e| format!("cannot write {}: {e}", to.display()))?;
        }
        if size != layer.size {
            return Err(format!(
                "the archive has {size} bytes, not the {} its manifest gives",
                layer.size
            ));
        }
        if archive::hex(&hash.finalize()) != layer.digest {
            return Err(format!("the archive does not match {}", layer.digest));
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
struct Descriptor {
    digest: String,
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
struct ContainerConfig {
    #[serde(rename = "ExposedPorts", default)]
    exposed_ports: Option<BTreeMap<String, serde_json::Value>>,
}

/// The `ExposedPorts` keys of an image config, sorted, without keys that are not `<port>/<proto>`
/// or `<low>-<high>/<proto>`.
fn exposed_ports(config: &[u8]) -> Result<Vec<String>, String> {
    let config: ImageConfig =
        serde_json::from_slice(config).map_err(|e| format!("cannot read the image config: {e}"))?;
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
async fn read_limited(response: Response<Incoming>, limit: u64) -> Result<Bytes, String> {
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    http_body_util::Limited::new(response.into_body(), limit)
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .map_err(|e| format!("cannot read the answer: {e}"))
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::net::SocketAddr;

    use http_body_util::Full;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use zeabur_builder_core::archive::{
        ARTIFACT_TYPE, EMPTY_CONFIG_MEDIA_TYPE, LAYER_MEDIA_TYPE, sha256,
    };

    use super::*;

    const NAME: &str = "team-a/api/source";
    const BASIC: &str = "Basic dGVhbS1hOnNlY3JldA==";

    #[test]
    fn credentials_for_the_registry_host() {
        let config = br#"{"auths":{
            "https://index.docker.io/v1/":{"auth":"aHViOmh1Yg=="},
            "zot.zot.svc:5000":{"username":"team-a","password":"secret"},
            "registry.example":{"registrytoken":"eyJ.x.y","username":"u","password":"p"}}}"#;
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
        assert_eq!(credentials(br#"{"auths":{}}"#, "zot").unwrap(), None);
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
                reply(StatusCode::OK, &[], br#"{"token":"t0k"}"#.to_vec())
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
        format!(
            r#"{{"schemaVersion":2,"mediaType":"{MANIFEST_MEDIA_TYPE}","artifactType":"{ARTIFACT_TYPE}",
            "config":{{"mediaType":"{EMPTY_CONFIG_MEDIA_TYPE}","digest":"{}","size":2}},
            "layers":[{{"mediaType":"{LAYER_MEDIA_TYPE}","digest":"{}","size":{}}}]}}"#,
            sha256(b"{}"),
            sha256(blob),
            blob.len()
        )
        .into_bytes()
    }

    async fn pull(addr: SocketAddr, digest: &str, to: &Path) -> Result<(), String> {
        let mut repository = Repository::new(
            client(None).unwrap(),
            &format!("127.0.0.1:{}/{NAME}", addr.port()),
            true,
            Some(Credentials::Basic(BASIC.to_owned())),
        )
        .unwrap();
        let layer = repository.layer(digest).await?;
        repository.download(&layer, to).await
    }

    fn scratch(name: &str) -> std::path::PathBuf {
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
        let config = br#"{"config":{"ExposedPorts":{"53/udp":{},"8000-8010/tcp":{},"3000/tcp":{},"../x":{},"80":{}}}}"#;
        assert_eq!(
            exposed_ports(config).unwrap(),
            ["3000/tcp", "53/udp", "8000-8010/tcp"]
        );
        assert_eq!(
            exposed_ports(br#"{"config":{}}"#).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            exposed_ports(br#"{"config":{"ExposedPorts":null}}"#).unwrap(),
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

    async fn ports(addr: SocketAddr, digest: &str) -> Result<Vec<String>, String> {
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
        let config = br#"{"config":{"ExposedPorts":{"3000/tcp":{},"53/udp":{}}}}"#.to_vec();
        let config_digest = sha256(&config);
        let image = format!(
            r#"{{"schemaVersion":2,"config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{}}},"layers":[]}}"#,
            config.len()
        )
        .into_bytes();
        let image_digest = sha256(&image);
        let attestation = br#"{"schemaVersion":2,"config":{"digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000"}}"#.to_vec();
        let attestation_digest = sha256(&attestation);
        let index = format!(
            r#"{{"schemaVersion":2,"manifests":[
            {{"digest":"{attestation_digest}","platform":{{"os":"unknown","architecture":"unknown"}}}},
            {{"digest":"{image_digest}","platform":{{"os":"linux","architecture":"amd64"}}}}]}}"#
        )
        .into_bytes();
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
        let config = br#"{"config":{"ExposedPorts":{"3000/tcp":{}}}}"#.to_vec();
        let config_digest = sha256(&config);
        let image =
            format!(r#"{{"schemaVersion":2,"config":{{"digest":"{config_digest}"}},"layers":[]}}"#)
                .into_bytes();
        let image_digest = sha256(&image);
        let addr = serve_images(vec![
            (
                config_digest,
                br#"{"config":{"ExposedPorts":{"22/tcp":{}}}}"#.to_vec(),
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
        assert!(error.contains("did not answer"), "{error}");
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
}
