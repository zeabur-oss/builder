//! Copies an image between registries by digest ([distribution spec], "Pushing manifests" and
//! "Pushing blobs").
//!
//! - **Source.** Only a digest. The top manifest must hash to it, each child manifest of an index
//!   to its entry, and each blob to its descriptor's digest and size. A blob goes to a file in the
//!   work directory first, so nothing unverified reaches the destination.
//! - **Order.** Blobs the destination lacks, then child manifests by digest, then the top manifest
//!   by digest with its exact bytes and media type, then each tag with the same bytes. A failure
//!   leaves no tag behind.
//! - **Check.** The destination must answer for the digest, and for each tag, with the source's
//!   digest.
//!
//! [distribution spec]: https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md#push

use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;
use zeabur_builder_core::archive::{self, Layer};
use zeabur_builder_core::result::is_digest;

use crate::registry::Repository;

/// An index of indexes of indexes is more than an image build makes.
const MAX_DEPTH: usize = 3;
const MAX_TAG_LEN: usize = 128;

#[derive(Deserialize)]
struct Manifest {
    #[serde(rename = "mediaType", default)]
    media_type: Option<String>,
    #[serde(default)]
    config: Option<Descriptor>,
    #[serde(default)]
    layers: Vec<Descriptor>,
    #[serde(default)]
    manifests: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct Descriptor {
    digest: String,
    #[serde(default)]
    size: u64,
}

/// A manifest as pushed.
struct Pushed {
    media_type: String,
    body: Vec<u8>,
}

/// `[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`, the distribution spec's tag grammar.
fn is_tag(tag: &str) -> bool {
    let mut chars = tag.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && tag.len() <= MAX_TAG_LEN
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Copies the image `digest` of `from` to `to`, tags it with `tags`, and checks the result.
/// `work` holds one blob at a time.
pub(crate) async fn copy(
    from: &mut Repository,
    to: &mut Repository,
    digest: &str,
    tags: &[String],
    work: &Path,
) -> Result<(), String> {
    if !is_digest(digest) {
        return Err(format!("`{digest}` is not a sha256 digest"));
    }
    if let Some(tag) = tags.iter().find(|tag| !is_tag(tag)) {
        return Err(format!("`{tag}` is not a tag"));
    }
    std::fs::create_dir_all(work).map_err(|e| format!("cannot create {}: {e}", work.display()))?;
    let top = copy_manifest(from, to, digest, work, 0).await?;
    for tag in tags {
        to.put_manifest(tag, &top.media_type, &top.body).await?;
    }
    let mut references = vec![digest];
    for tag in tags {
        references.push(tag);
    }
    for reference in references {
        let pushed = to.manifest_digest(reference).await?;
        if pushed != digest {
            return Err(format!(
                "the destination answers {pushed} for {reference}, not {digest}"
            ));
        }
    }
    Ok(())
}

/// Copies one manifest and what it names, and pushes it by digest.
async fn copy_manifest(
    from: &mut Repository,
    to: &mut Repository,
    digest: &str,
    work: &Path,
    depth: usize,
) -> Result<Pushed, String> {
    if depth > MAX_DEPTH {
        return Err("the index nests too deep".to_owned());
    }
    let (header_type, body) = from.manifest(digest).await?;
    if archive::sha256(&body) != digest {
        return Err(format!("the manifest does not match {digest}"));
    }
    let manifest: Manifest = serde_json::from_slice(&body)
        .map_err(|e| format!("cannot read the manifest {digest}: {e}"))?;
    let media_type = manifest
        .media_type
        .or(header_type)
        .ok_or_else(|| format!("the manifest {digest} has no media type"))?;
    if manifest.config.is_none() && manifest.manifests.is_empty() {
        return Err(format!("{digest} is neither an image nor an index"));
    }

    let mut blobs = BTreeSet::new();
    for descriptor in manifest.config.iter().chain(&manifest.layers) {
        check_digest(&descriptor.digest)?;
        if blobs.insert(descriptor.digest.as_str()) {
            copy_blob(from, to, descriptor, work).await?;
        }
    }
    for child in &manifest.manifests {
        check_digest(&child.digest)?;
        Box::pin(copy_manifest(from, to, &child.digest, work, depth + 1)).await?;
    }
    to.put_manifest(digest, &media_type, &body).await?;
    Ok(Pushed {
        media_type,
        body: body.to_vec(),
    })
}

fn check_digest(digest: &str) -> Result<(), String> {
    if is_digest(digest) {
        Ok(())
    } else {
        Err(format!("`{digest}` is not a sha256 digest"))
    }
}

/// Copies one blob unless the destination has it.
async fn copy_blob(
    from: &mut Repository,
    to: &mut Repository,
    descriptor: &Descriptor,
    work: &Path,
) -> Result<(), String> {
    if to.has_blob(&descriptor.digest).await? {
        return Ok(());
    }
    let layer = Layer {
        digest: descriptor.digest.clone(),
        size: descriptor.size,
    };
    let file = work.join(descriptor.digest.replace(':', "-"));
    drop(std::fs::remove_file(&file));
    from.download(&layer, &file).await?;
    let uploaded = to.upload(&layer.digest, &file, layer.size).await;
    drop(std::fs::remove_file(&file));
    uploaded
}

#[cfg(test)]
#[expect(
    clippy::significant_drop_tightening,
    reason = "the fake registry and the checks hold its state for a whole request or test"
)]
mod tests {
    use std::collections::BTreeMap;
    use std::convert::Infallible;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use http::{Method, Request, Response, StatusCode, header};
    use http_body_util::{BodyExt as _, Full};
    use hyper::body::Incoming;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use zeabur_builder_core::archive::sha256;

    use super::*;
    use crate::registry::{Credentials, client};

    const NAME: &str = "team-a/api";
    const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
    const OCI_IMAGE: &str = "application/vnd.oci.image.manifest.v1+json";
    const TOKEN: &str = "Bearer push-token";

    #[derive(Default)]
    struct State {
        blobs: BTreeMap<String, Vec<u8>>,
        manifests: BTreeMap<String, (String, Vec<u8>)>,
        tags: BTreeMap<String, String>,
        requests: Vec<String>,
        unauthorized: usize,
    }

    struct Fake {
        /// The `Authorization` value every request must carry.
        token: Option<&'static str>,
        state: Mutex<State>,
    }

    impl Fake {
        fn new(token: Option<&'static str>) -> Arc<Self> {
            Arc::new(Self {
                token,
                state: Mutex::new(State::default()),
            })
        }

        fn seed(&self, blobs: &[&Vec<u8>], manifests: &[(&str, &Vec<u8>)]) {
            let mut state = self.state.lock().unwrap();
            for blob in blobs {
                state.blobs.insert(sha256(blob), (*blob).clone());
            }
            for (media_type, body) in manifests {
                state
                    .manifests
                    .insert(sha256(body), ((*media_type).to_owned(), (*body).clone()));
            }
        }

        fn log(&self) -> Vec<String> {
            self.state.lock().unwrap().requests.clone()
        }
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

    async fn handle(fake: &Fake, request: Request<Incoming>) -> Response<Full<Bytes>> {
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        let query = request.uri().query().unwrap_or_default().to_owned();
        let auth = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let content_type = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = request
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        let mut state = fake.state.lock().unwrap();
        state.requests.push(format!("{method} {path}"));
        if fake.token.is_some() && auth.as_deref() != fake.token {
            state.unauthorized += 1;
            return reply(StatusCode::UNAUTHORIZED, &[], Vec::new());
        }
        let Some(rest) = path.strip_prefix(&format!("/v2/{NAME}/")) else {
            return reply(StatusCode::NOT_FOUND, &[], Vec::new());
        };
        if rest == "blobs/uploads/" && method == Method::POST {
            let location = format!("/v2/{NAME}/blobs/uploads/session-1?_state=abc");
            return reply(StatusCode::ACCEPTED, &[("location", location)], Vec::new());
        }
        if rest.starts_with("blobs/uploads/") && method == Method::PUT {
            let digest = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("digest="))
                .unwrap_or_default()
                .replace("%3A", ":");
            if sha256(&body) != digest || !query.contains("_state=abc") {
                return reply(StatusCode::BAD_REQUEST, &[], Vec::new());
            }
            state.blobs.insert(digest, body);
            return reply(StatusCode::CREATED, &[], Vec::new());
        }
        if let Some(digest) = rest.strip_prefix("blobs/") {
            return match state.blobs.get(digest) {
                Some(blob) if method == Method::HEAD => reply(
                    StatusCode::OK,
                    &[("content-length", blob.len().to_string())],
                    Vec::new(),
                ),
                Some(blob) => reply(StatusCode::OK, &[], blob.clone()),
                None => reply(StatusCode::NOT_FOUND, &[], Vec::new()),
            };
        }
        if let Some(reference) = rest.strip_prefix("manifests/") {
            if method == Method::PUT {
                let digest = sha256(&body);
                if reference.starts_with("sha256:") && reference != digest {
                    return reply(StatusCode::BAD_REQUEST, &[], Vec::new());
                }
                if !reference.starts_with("sha256:") {
                    state.tags.insert(reference.to_owned(), digest.clone());
                }
                state
                    .manifests
                    .insert(digest, (content_type.unwrap_or_default(), body));
                return reply(StatusCode::CREATED, &[], Vec::new());
            }
            let digest = state
                .tags
                .get(reference)
                .cloned()
                .unwrap_or_else(|| reference.to_owned());
            return match state.manifests.get(&digest) {
                Some((media_type, body)) => reply(
                    StatusCode::OK,
                    &[
                        ("content-type", media_type.clone()),
                        ("docker-content-digest", digest),
                    ],
                    if method == Method::HEAD {
                        Vec::new()
                    } else {
                        body.clone()
                    },
                ),
                None => reply(StatusCode::NOT_FOUND, &[], Vec::new()),
            };
        }
        reply(StatusCode::NOT_FOUND, &[], Vec::new())
    }

    async fn serve(fake: &Arc<Fake>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fake = Arc::clone(fake);
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let fake = Arc::clone(&fake);
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let fake = Arc::clone(&fake);
                        async move { Ok::<_, Infallible>(handle(&fake, request).await) }
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

    fn repository(addr: SocketAddr, token: Option<&str>) -> Repository {
        Repository::new(
            client(None).unwrap(),
            &format!("127.0.0.1:{}/{NAME}", addr.port()),
            true,
            token.map(|t| Credentials::Bearer(t.to_owned())),
        )
        .unwrap()
    }

    /// One image: its config, layer and manifest.
    struct Image {
        config: Vec<u8>,
        layer: Vec<u8>,
        manifest: Vec<u8>,
    }

    fn image(arch: &str, layer: &[u8]) -> Image {
        let config = format!(r#"{{"architecture":"{arch}","os":"linux"}}"#).into_bytes();
        let layer = layer.to_vec();
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"{OCI_IMAGE}",
            "config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},
            "layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{}","size":{}}}]}}"#,
            sha256(&config),
            config.len(),
            sha256(&layer),
            layer.len()
        )
        .into_bytes();
        Image {
            config,
            layer,
            manifest,
        }
    }

    /// An index of two platforms that share a layer.
    fn index(images: &[&Image]) -> Vec<u8> {
        let mut entries = Vec::new();
        for (image, arch) in images.iter().zip(["amd64", "arm64"]) {
            entries.push(format!(
                r#"{{"mediaType":"{OCI_IMAGE}","digest":"{}","size":{},"platform":{{"architecture":"{arch}","os":"linux"}}}}"#,
                sha256(&image.manifest),
                image.manifest.len()
            ));
        }
        format!(
            r#"{{"schemaVersion":2,"mediaType":"{OCI_INDEX}","manifests":[{}]}}"#,
            entries.join(",")
        )
        .into_bytes()
    }

    struct Setup {
        source: Arc<Fake>,
        dest: Arc<Fake>,
        source_addr: SocketAddr,
        dest_addr: SocketAddr,
        index: Vec<u8>,
        images: [Image; 2],
    }

    async fn setup(dest_token: Option<&'static str>) -> Setup {
        let images = [
            image("amd64", b"shared layer"),
            image("arm64", b"arm layer"),
        ];
        let index = index(&[&images[0], &images[1]]);
        let source = Fake::new(None);
        let mut blobs = Vec::new();
        let mut manifests = vec![(OCI_INDEX, &index)];
        for image in &images {
            blobs.push(&image.config);
            blobs.push(&image.layer);
            manifests.push((OCI_IMAGE, &image.manifest));
        }
        source.seed(&blobs, &manifests);
        let dest = Fake::new(dest_token);
        Setup {
            source_addr: serve(&source).await,
            dest_addr: serve(&dest).await,
            source,
            dest,
            index,
            images,
        }
    }

    fn work(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("zeabur-builder-copy-{}-{name}", std::process::id()))
    }

    async fn run(setup: &Setup, name: &str, tags: &[&str]) -> Result<(), String> {
        let tags: Vec<String> = tags.iter().map(|t| (*t).to_owned()).collect();
        let mut from = repository(setup.source_addr, None);
        let mut to = repository(setup.dest_addr, setup.dest.token.map(|_| TOKEN)).for_push();
        let dir = work(name);
        let done = copy(&mut from, &mut to, &sha256(&setup.index), &tags, &dir).await;
        let leftovers = std::fs::read_dir(&dir).map_or(0, Iterator::count);
        assert_eq!(leftovers, 0, "the work directory is emptied");
        drop(std::fs::remove_dir(&dir));
        done
    }

    #[tokio::test]
    async fn copies_an_index_with_both_platforms() {
        let setup = setup(None).await;
        run(&setup, "index", &["v1", "latest"]).await.unwrap();

        let state = setup.dest.state.lock().unwrap();
        let digest = sha256(&setup.index);
        let (media_type, body) = &state.manifests[&digest];
        assert_eq!(media_type, OCI_INDEX);
        assert_eq!(body, &setup.index);
        assert_eq!(state.tags.get("v1"), Some(&digest));
        assert_eq!(state.tags.get("latest"), Some(&digest));
        for image in &setup.images {
            assert!(state.manifests.contains_key(&sha256(&image.manifest)));
            assert_eq!(state.blobs.get(&sha256(&image.config)), Some(&image.config));
            assert_eq!(state.blobs.get(&sha256(&image.layer)), Some(&image.layer));
        }
    }

    #[tokio::test]
    async fn skips_blobs_the_destination_has() {
        let setup = setup(None).await;
        let first = &setup.images[0];
        setup.dest.seed(&[&first.config, &first.layer], &[]);
        run(&setup, "skip", &[]).await.unwrap();

        let downloads = setup.source.log();
        for blob in [&first.config, &first.layer] {
            let request = format!("GET /v2/{NAME}/blobs/{}", sha256(blob));
            assert!(!downloads.contains(&request), "{request}");
        }
        let second = &setup.images[1];
        let request = format!("GET /v2/{NAME}/blobs/{}", sha256(&second.layer));
        assert!(downloads.contains(&request));
        let uploads = setup
            .dest
            .log()
            .iter()
            .filter(|r| r.starts_with("PUT") && r.contains("/blobs/uploads/"))
            .count();
        assert_eq!(uploads, 2, "only the second image's config and layer");
    }

    #[tokio::test]
    async fn refuses_a_blob_that_does_not_match_and_tags_nothing() {
        let setup = setup(None).await;
        let layer = sha256(&setup.images[1].layer);
        setup
            .source
            .state
            .lock()
            .unwrap()
            .blobs
            .insert(layer, b"arm LAYER".to_vec());
        let error = run(&setup, "mismatch", &["v1"]).await.unwrap_err();
        assert!(error.contains("does not match"), "{error}");

        let state = setup.dest.state.lock().unwrap();
        assert!(state.tags.is_empty());
        assert!(!state.manifests.contains_key(&sha256(&setup.index)));
    }

    #[tokio::test]
    async fn refuses_a_manifest_that_does_not_match() {
        let setup = setup(None).await;
        let digest = sha256(&setup.images[0].manifest);
        setup.source.state.lock().unwrap().manifests.insert(
            digest,
            (OCI_IMAGE.to_owned(), br#"{"schemaVersion":2,"config":{"digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000"}}"#.to_vec()),
        );
        assert!(run(&setup, "manifest", &["v1"]).await.is_err());
        assert!(setup.dest.state.lock().unwrap().tags.is_empty());
    }

    #[tokio::test]
    async fn sends_the_token_up_front() {
        let setup = setup(Some(TOKEN)).await;
        run(&setup, "token", &["v1"]).await.unwrap();
        let state = setup.dest.state.lock().unwrap();
        assert_eq!(state.unauthorized, 0, "no 401 round trip");
        assert!(state.tags.contains_key("v1"));
    }

    #[tokio::test]
    async fn refuses_what_is_not_a_digest_or_a_tag() {
        let setup = setup(None).await;
        let mut from = repository(setup.source_addr, None);
        let mut to = repository(setup.dest_addr, None);
        let dir = work("grammar");
        let digest = sha256(&setup.index);
        assert!(copy(&mut from, &mut to, "latest", &[], &dir).await.is_err());
        let bad = ["../x".to_owned()];
        assert!(copy(&mut from, &mut to, &digest, &bad, &dir).await.is_err());
        assert_eq!(setup.dest.log(), Vec::<String>::new());
    }
}
