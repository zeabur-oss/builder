//! The HTTP client Rig sends model requests through: hyper with rustls on aws-lc-rs and the
//! system's CA bundle, as the rest of Builder uses. Rig's bundled client would add reqwest and
//! `webpki-root-certs`, which the dependency policy bans.

#![expect(
    clippy::result_large_err,
    reason = "Rig's HTTP error type is what the trait returns"
)]

use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt as _;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use rig_core::http_client::{
    BoxedStream, Error, HttpClientExt, LazyBody, MultipartForm, Request, Response, Result,
    StreamingResponse,
};
use rustls::ClientConfig;

/// The largest response body read whole: a model reply or an error.
const MAX_BODY: usize = 16 << 20;

/// Why a client could not be built or a request failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ClientError {
    /// A certificate of the system's CA bundle that rustls rejects.
    #[error("bad CA certificate: {0}")]
    Certificate(#[source] rustls::Error),
    #[error("no CA certificates found: {0:?}")]
    NoCertificates(Vec<rustls_native_certs::Error>),
    #[error("TLS setup: {0}")]
    Tls(#[source] rustls::Error),
    #[error("{0}")]
    Request(#[from] http::Error),
    #[error("{0}")]
    Send(#[from] hyper_util::client::legacy::Error),
    #[error("{0}")]
    Body(#[from] Error),
}

/// A request body and its media type.
#[derive(Debug)]
pub(crate) struct Body {
    pub(crate) content_type: &'static str,
    pub(crate) bytes: Vec<u8>,
}

impl Body {
    /// `value` as JSON.
    pub(crate) fn json(value: &impl serde::Serialize) -> Self {
        Self {
            content_type: "application/json",
            // A token request holds only strings, so serialization cannot fail.
            bytes: serde_json::to_vec(value).unwrap_or_default(),
        }
    }

    /// `pairs` as an HTML form.
    pub(crate) fn form<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self {
            content_type: "application/x-www-form-urlencoded",
            bytes: form_urlencoded::Serializer::new(String::new())
                .extend_pairs(pairs)
                .finish()
                .into_bytes(),
        }
    }
}

/// An HTTPS client for model APIs.
#[derive(Debug, Clone)]
pub(crate) struct HyperClient {
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
}

impl HyperClient {
    /// A client that trusts the system's CA bundle (`SSL_CERT_FILE` or `/etc/ssl/certs`).
    pub(crate) fn new() -> std::result::Result<Self, ClientError> {
        let mut roots = rustls::RootCertStore::empty();
        let found = rustls_native_certs::load_native_certs();
        for cert in found.certs {
            roots.add(cert).map_err(ClientError::Certificate)?;
        }
        if roots.is_empty() {
            return Err(ClientError::NoCertificates(found.errors));
        }
        let config = ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(ClientError::Tls)?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let https = HttpsConnectorBuilder::new()
            .with_tls_config(config)
            .https_or_http()
            .enable_http1()
            .build();
        Ok(Self {
            client: Client::builder(TokioExecutor::new()).build(https),
        })
    }

    /// Posts `body` to `url` and returns the status and the reply, whatever the status.
    pub(crate) async fn post(
        &self,
        url: &str,
        body: Body,
    ) -> std::result::Result<(http::StatusCode, Bytes), ClientError> {
        let req = Request::post(url)
            .header(http::header::CONTENT_TYPE, body.content_type)
            .body(Full::new(Bytes::from(body.bytes)))?;
        let response = self.client.request(req).await?;
        let status = response.status();
        let reply = read(response.into_body()).await?;
        Ok((status, reply))
    }

    /// Sends `req` and returns the response, or the error a non-success status makes.
    async fn exchange(&self, req: Request<Full<Bytes>>) -> Result<Response<hyper::body::Incoming>> {
        let response = self.client.request(req).await.map_err(Error::instance)?;
        if response.status().is_success() {
            return Ok(response);
        }
        let (parts, body) = response.into_parts();
        let body = read(body).await?;
        Err(Error::non_success_with_details(
            parts.status,
            parts.headers,
            String::from_utf8_lossy(&body).into_owned(),
        ))
    }
}

/// Reads a whole body, at most [`MAX_BODY`] bytes.
async fn read(body: hyper::body::Incoming) -> Result<Bytes> {
    Ok(Limited::new(body, MAX_BODY)
        .collect()
        .await
        .map_err(Error::Instance)?
        .to_bytes())
}

impl HttpClientExt for HyperClient {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        let client = self.clone();
        let req = req.map(|body| Full::new(body.into()));
        async move {
            let (parts, body) = client.exchange(req).await?.into_parts();
            let body: LazyBody<U> = Box::pin(async move { read(body).await.map(U::from) });
            Ok(Response::from_parts(parts, body))
        }
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        let (mut parts, form) = req.into_parts();
        let (boundary, body) = form.encode();
        let content_type = format!("multipart/form-data; boundary={boundary}");
        let req = http::HeaderValue::try_from(content_type)
            .map(|value| {
                parts.headers.insert(http::header::CONTENT_TYPE, value);
                Request::from_parts(parts, body)
            })
            .map_err(Error::from);
        let client = self.clone();
        async move { client.send(req?).await }
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<StreamingResponse>> + Send
    where
        T: Into<Bytes> + Send,
    {
        let client = self.clone();
        let req = req.map(|body| Full::new(body.into()));
        async move {
            let (parts, body) = client.exchange(req).await?.into_parts();
            let stream: BoxedStream = Box::pin(
                body.into_data_stream()
                    .map(|chunk| chunk.map_err(Error::instance)),
            );
            Ok(Response::from_parts(parts, stream))
        }
    }
}
