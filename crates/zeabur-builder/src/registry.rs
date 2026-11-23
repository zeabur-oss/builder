//! Tokens for the default registry, and the deletion of a `Build`'s repositories there.
//!
//! The registry trusts one Ed25519 key, such as the catalog Zot's `zot-token-signer`. The
//! controller reads the private key from that Secret and signs a JWT for each use, in the form of
//! the Docker [token authentication] `access` claim. The token names each repository exactly and
//! expires, so a run pushes only to its own `Build`'s repository and pulls only its own archives.
//!
//! [token authentication]: https://distribution.github.io/distribution/spec/auth/jwt/

use std::collections::BTreeMap;
use std::time::Duration;

use aws_lc_rs::signature::Ed25519KeyPair;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use http::{Method, Request, StatusCode, header};
use http_body_util::{BodyExt as _, Full, Limited};
use jiff::Timestamp;
use k8s_openapi::api::core::v1::Secret;
use kube::{Api, Client};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeabur_builder_api::labels;

use crate::remote::HttpClient;

const TIMEOUT: Duration = Duration::from_secs(30);
/// How far the registry's clock may be from the controller's, either way.
pub(crate) const CLOCK_SKEW: Duration = Duration::from_mins(1);
const MAX_ANSWER: usize = 4 << 20;
const DOCKER_CONFIG_JSON: &str = "kubernetes.io/dockerconfigjson";
const USER_AGENT: &str = concat!("zeabur-builder/", env!("CARGO_PKG_VERSION"));
const INDEX_TYPES: [&str; 2] = [
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];
const MANIFEST_TYPES: [&str; 4] = [
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
    INDEX_TYPES[0],
    INDEX_TYPES[1],
];

/// Why the key, a Secret, a Docker config or the registry refused.
#[derive(Debug, Error)]
pub(crate) enum RegistryError {
    /// The Secret with the signing key could not be read.
    #[error("reading Secret `{namespace}/{name}`: {source}")]
    ReadSecret {
        namespace: String,
        name: String,
        #[source]
        source: kube::Error,
    },
    /// There is no Secret with the signing key.
    #[error("no Secret `{name}` with the registry token key in `{namespace}`")]
    NoKeySecret { namespace: String, name: String },
    /// The Secret has no `tls.key`.
    #[error("Secret `{namespace}/{name}` has no tls.key")]
    NoKey { namespace: String, name: String },
    /// The Secret's `tls.key` is no Ed25519 key.
    #[error("Secret `{namespace}/{name}`: {source}")]
    Key {
        namespace: String,
        name: String,
        #[source]
        source: KeyError,
    },
    /// A Docker config does not parse.
    #[error("cannot read the Docker config: {0}")]
    ReadConfig(#[source] serde_json::Error),
    /// A Docker config does not encode.
    #[error("cannot write the Docker config: {0}")]
    WriteConfig(#[source] serde_json::Error),
    /// The push Secret lacks the opt-in label.
    #[error(
        "Secret `{name}` needs the label `{}: \"true\"`",
        labels::PUSH_CREDENTIALS
    )]
    PushLabel { name: String },
    /// The push Secret is not a `kubernetes.io/dockerconfigjson` Secret.
    #[error("Secret `{name}` is of type `{type_}`; use {DOCKER_CONFIG_JSON}")]
    PushType { name: String, type_: String },
    /// The push Secret has no `.dockerconfigjson`.
    #[error("Secret `{name}` has no .dockerconfigjson")]
    PushNoConfig { name: String },
    /// The push Secret's `.dockerconfigjson` is no Docker config.
    #[error("Secret `{name}`: .dockerconfigjson is not a Docker config")]
    PushNotConfig {
        name: String,
        #[source]
        source: serde_json::Error,
    },
    /// The request could not be built.
    #[error(transparent)]
    Request(#[from] http::Error),
    /// The registry did not answer in time.
    #[error("{method} {url}: timed out")]
    TimedOut { method: Method, url: String },
    /// The registry could not be reached.
    #[error("{method} {url}: {source}")]
    Transport {
        method: Method,
        url: String,
        #[source]
        source: hyper_util::client::legacy::Error,
    },
    /// The answer's body could not be read.
    #[error("{method} {url}: {source}")]
    Body {
        method: Method,
        url: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The registry refused to list the tags of a repository.
    #[error("listing the tags of {repository}: {status}")]
    ListStatus {
        repository: String,
        status: StatusCode,
    },
    /// The list of tags is not a list of tags.
    #[error("listing the tags of {repository}: {source}")]
    ListAnswer {
        repository: String,
        #[source]
        source: serde_json::Error,
    },
    /// The registry refused to give a manifest.
    #[error("reading {repository}:{tag}: {status}")]
    ReadStatus {
        repository: String,
        tag: String,
        status: StatusCode,
    },
    /// The registry sent a manifest without its digest.
    #[error("{repository}:{tag} has no Docker-Content-Digest")]
    NoDigest { repository: String, tag: String },
    /// The manifest is not a manifest.
    #[error("reading {repository}:{tag}: {source}")]
    ManifestAnswer {
        repository: String,
        tag: String,
        #[source]
        source: serde_json::Error,
    },
    /// The registry refused to delete a manifest.
    #[error("deleting {repository}@{digest}: {status}")]
    DeleteStatus {
        repository: String,
        digest: String,
        status: StatusCode,
    },
}

/// Why a PEM text is no signing key.
#[derive(Debug, Error)]
pub(crate) enum KeyError {
    /// The key has no PEM `PRIVATE KEY` block.
    #[error("the key is not a PEM PKCS#8 private key: {0}")]
    NotPkcs8(#[source] rustls::pki_types::pem::Error),
    /// The DER is no Ed25519 key.
    #[error("the key is not an Ed25519 key: {0}")]
    NotEd25519(#[source] aws_lc_rs::error::KeyRejected),
}

/// What a token allows on one repository, besides pulling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Access {
    pub(crate) push: bool,
    pub(crate) delete: bool,
}

impl Access {
    pub(crate) const PULL: Self = Self {
        push: false,
        delete: false,
    };
    pub(crate) const PUSH: Self = Self {
        push: true,
        delete: false,
    };
    pub(crate) const DELETE: Self = Self {
        push: false,
        delete: true,
    };

    fn actions(self) -> Vec<&'static str> {
        let mut actions = vec!["pull"];
        if self.push {
            actions.push("push");
        }
        if self.delete {
            actions.push("delete");
        }
        actions
    }
}

/// The key that signs tokens.
pub(crate) struct Signer {
    key: Ed25519KeyPair,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer").finish_non_exhaustive()
    }
}

impl Signer {
    /// Reads a PEM PKCS#8 Ed25519 key, as cert-manager writes to `tls.key`.
    pub(crate) fn from_pem(pem: &[u8]) -> Result<Self, KeyError> {
        use rustls::pki_types::PrivatePkcs8KeyDer;
        use rustls::pki_types::pem::PemObject as _;

        let der = PrivatePkcs8KeyDer::from_pem_slice(pem).map_err(KeyError::NotPkcs8)?;
        let key = Ed25519KeyPair::from_pkcs8_maybe_unchecked(der.secret_pkcs8_der())
            .map_err(KeyError::NotEd25519)?;
        Ok(Self { key })
    }

    /// A token for `grants`, valid from `now` for `ttl`.
    pub(crate) fn token(&self, grants: &[(&str, Access)], now: Timestamp, ttl: Duration) -> String {
        let now = now.as_second();
        let ttl = i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
        let skew = i64::try_from(CLOCK_SKEW.as_secs()).unwrap_or(i64::MAX);
        let access = grants
            .iter()
            .map(|&(name, access)| Grant {
                kind: "repository",
                name,
                actions: access.actions(),
            })
            .collect();
        // The header is signed as these exact bytes.
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"EdDSA","typ":"JWT"}"#);
        let claims = Claims {
            iss: "zeabur-builder",
            iat: now,
            nbf: now.saturating_sub(skew),
            exp: now.saturating_add(ttl),
            access,
        };
        let claims = serde_json::to_vec(&claims).unwrap_or_default();
        let claims = URL_SAFE_NO_PAD.encode(claims);
        let input = format!("{header}.{claims}");
        let signature = self.key.sign(input.as_bytes());
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
    }

    #[cfg(test)]
    fn public_key(&self) -> Vec<u8> {
        use aws_lc_rs::signature::KeyPair as _;
        self.key.public_key().as_ref().to_vec()
    }
}

/// Reads the signing key from `tls.key` of the Secret `name`.
pub(crate) async fn read_signer(
    client: &Client,
    namespace: &str,
    name: &str,
) -> Result<Signer, RegistryError> {
    let here = || (namespace.to_owned(), name.to_owned());
    let secret = Api::<Secret>::namespaced(client.clone(), namespace)
        .get_opt(name)
        .await
        .map_err(|source| RegistryError::ReadSecret {
            namespace: namespace.to_owned(),
            name: name.to_owned(),
            source,
        })?
        .ok_or_else(|| {
            let (namespace, name) = here();
            RegistryError::NoKeySecret { namespace, name }
        })?;
    let key = secret
        .data
        .and_then(|mut d| d.remove("tls.key"))
        .ok_or_else(|| {
            let (namespace, name) = here();
            RegistryError::NoKey { namespace, name }
        })?;
    Signer::from_pem(&key.0).map_err(|source| {
        let (namespace, name) = here();
        RegistryError::Key {
            namespace,
            name,
            source,
        }
    })
}

/// The registry token's claims.
#[derive(Serialize)]
struct Claims<'a> {
    iss: &'static str,
    iat: i64,
    nbf: i64,
    exp: i64,
    access: Vec<Grant<'a>>,
}

/// One repository a token grants.
#[derive(Serialize)]
struct Grant<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
    actions: Vec<&'static str>,
}

/// A field of a user's Docker config that this crate does not read, kept as it is.
#[expect(
    clippy::disallowed_types,
    reason = "a Docker config holds fields of any shape, such as credHelpers, that pass through unread"
)]
type PassThrough = serde_json::Value;

/// A Docker config. Fields this crate does not set pass through.
#[derive(Serialize, Deserialize, Default)]
struct DockerConfig {
    #[serde(default)]
    auths: BTreeMap<String, Auth>,
    #[serde(flatten)]
    rest: BTreeMap<String, PassThrough>,
}

/// The credentials of one registry host.
#[derive(Serialize, Deserialize, Default)]
struct Auth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    registrytoken: Option<String>,
    #[serde(flatten)]
    rest: BTreeMap<String, PassThrough>,
}

/// A Docker config that sends `token` to `host` as it is.
pub(crate) fn docker_config(host: &str, token: &str) -> Vec<u8> {
    let config = DockerConfig {
        auths: BTreeMap::from([(host.to_owned(), bearer(token))]),
        rest: BTreeMap::new(),
    };
    serde_json::to_vec(&config).unwrap_or_default()
}

fn bearer(token: &str) -> Auth {
    Auth {
        registrytoken: Some(token.to_owned()),
        rest: BTreeMap::new(),
    }
}

/// Adds `token` for `host` to a Docker config, replacing any entry for `host`.
pub(crate) fn with_token(config: &[u8], host: &str, token: &str) -> Result<Vec<u8>, RegistryError> {
    let mut config: DockerConfig =
        serde_json::from_slice(config).map_err(RegistryError::ReadConfig)?;
    config.auths.retain(|key, _| {
        let key = key
            .strip_prefix("https://")
            .or_else(|| key.strip_prefix("http://"))
            .unwrap_or(key);
        key.split('/').next() != Some(host)
    });
    config.auths.insert(host.to_owned(), bearer(token));
    serde_json::to_vec(&config).map_err(RegistryError::WriteConfig)
}

/// The Docker config of `secret`, named `name`: a `kubernetes.io/dockerconfigjson` Secret with the
/// label `builder.zeabur.com/push-credentials: "true"`. The error says what is wrong.
pub(crate) fn push_config(name: &str, secret: &Secret) -> Result<Vec<u8>, RegistryError> {
    let labelled = secret
        .metadata
        .labels
        .as_ref()
        .and_then(|l| l.get(labels::PUSH_CREDENTIALS))
        .is_some_and(|v| v == "true");
    if !labelled {
        return Err(RegistryError::PushLabel {
            name: name.to_owned(),
        });
    }
    if secret.type_.as_deref() != Some(DOCKER_CONFIG_JSON) {
        return Err(RegistryError::PushType {
            name: name.to_owned(),
            type_: secret.type_.as_deref().unwrap_or("Opaque").to_owned(),
        });
    }
    let config = secret
        .data
        .as_ref()
        .and_then(|d| d.get(".dockerconfigjson"))
        .ok_or_else(|| RegistryError::PushNoConfig {
            name: name.to_owned(),
        })?;
    serde_json::from_slice::<DockerConfig>(&config.0)
        .map(|_| config.0.clone())
        .map_err(|source| RegistryError::PushNotConfig {
            name: name.to_owned(),
            source,
        })
}

/// The registry API of one host.
#[derive(Debug, Clone)]
pub(crate) struct Registry {
    client: HttpClient,
    base: String,
}

#[derive(Deserialize)]
struct Tags {
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    #[serde(default)]
    media_type: Option<String>,
    #[serde(default)]
    manifests: Vec<Descriptor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    digest: String,
    #[serde(default)]
    media_type: Option<String>,
}

impl Registry {
    pub(crate) fn new(client: HttpClient, host: &str, plain_http: bool) -> Self {
        let scheme = if plain_http { "http" } else { "https" };
        Self {
            client,
            base: format!("{scheme}://{host}"),
        }
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        token: &str,
    ) -> Result<(StatusCode, http::HeaderMap, Bytes), RegistryError> {
        let url = format!("{}/v2/{path}", self.base);
        let request = Request::builder()
            .method(method.clone())
            .uri(&url)
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::ACCEPT, MANIFEST_TYPES.join(", "))
            .body(Full::new(Bytes::new()))?;
        let timed_out = |_elapsed| RegistryError::TimedOut {
            method: method.clone(),
            url: url.clone(),
        };
        let response = tokio::time::timeout(TIMEOUT, self.client.request(request))
            .await
            .map_err(timed_out)?
            .map_err(|source| RegistryError::Transport {
                method: method.clone(),
                url: url.clone(),
                source,
            })?;
        let (parts, body) = response.into_parts();
        let body = tokio::time::timeout(TIMEOUT, Limited::new(body, MAX_ANSWER).collect())
            .await
            .map_err(timed_out)?
            .map_err(|source| RegistryError::Body {
                method: method.clone(),
                url: url.clone(),
                source,
            })?
            .to_bytes();
        Ok((parts.status, parts.headers, body))
    }

    /// Deletes every tagged manifest of `repository`, and the manifests its indexes list. The
    /// registry may then remove the repository, and Zot does so at once. A missing repository is
    /// no error.
    pub(crate) async fn delete_repository(
        &self,
        repository: &str,
        token: &str,
    ) -> Result<(), RegistryError> {
        let (status, _, body) = self
            .send(Method::GET, &format!("{repository}/tags/list"), token)
            .await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        if status != StatusCode::OK {
            return Err(RegistryError::ListStatus {
                repository: repository.to_owned(),
                status,
            });
        }
        let tags: Tags =
            serde_json::from_slice(&body).map_err(|source| RegistryError::ListAnswer {
                repository: repository.to_owned(),
                source,
            })?;
        let mut deleted = Vec::new();
        for tag in tags.tags.unwrap_or_default() {
            let (status, headers, body) = self
                .send(Method::GET, &format!("{repository}/manifests/{tag}"), token)
                .await?;
            if status == StatusCode::NOT_FOUND {
                continue;
            }
            if status != StatusCode::OK {
                return Err(RegistryError::ReadStatus {
                    repository: repository.to_owned(),
                    tag,
                    status,
                });
            }
            let digest = headers
                .get("docker-content-digest")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
                .ok_or_else(|| RegistryError::NoDigest {
                    repository: repository.to_owned(),
                    tag: tag.clone(),
                })?;
            if deleted.contains(&digest) {
                continue;
            }
            let manifest: Manifest =
                serde_json::from_slice(&body).map_err(|source| RegistryError::ManifestAnswer {
                    repository: repository.to_owned(),
                    tag,
                    source,
                })?;
            let index = manifest
                .media_type
                .as_deref()
                .or_else(|| {
                    headers
                        .get(header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                })
                .is_some_and(|t| INDEX_TYPES.contains(&t));
            self.delete_manifest(repository, &digest, token).await?;
            if index {
                for child in manifest.manifests {
                    if child
                        .media_type
                        .as_deref()
                        .is_some_and(|t| MANIFEST_TYPES.contains(&t))
                    {
                        self.delete_manifest(repository, &child.digest, token)
                            .await?;
                    }
                }
            }
            deleted.push(digest);
        }
        Ok(())
    }

    async fn delete_manifest(
        &self,
        repository: &str,
        digest: &str,
        token: &str,
    ) -> Result<(), RegistryError> {
        let (status, ..) = self
            .send(
                Method::DELETE,
                &format!("{repository}/manifests/{digest}"),
                token,
            )
            .await?;
        if status.is_success() || status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(RegistryError::DeleteStatus {
                repository: repository.to_owned(),
                digest: digest.to_owned(),
                status,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::disallowed_types,
        reason = "test fixtures are JSON of any shape"
    )]

    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::signature::{ED25519, UnparsedPublicKey};
    use serde_json::json;

    use super::*;

    fn pem(der: &[u8]) -> Vec<u8> {
        format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            base64::engine::general_purpose::STANDARD.encode(der)
        )
        .into_bytes()
    }

    fn claims(token: &str) -> serde_json::Value {
        let claims = token.split('.').nth(1).unwrap();
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).unwrap()).unwrap()
    }

    #[test]
    fn signs_tokens_the_public_key_verifies() {
        let der = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let signer = Signer::from_pem(&pem(der.as_ref())).unwrap();
        let now: Timestamp = "2026-10-25T08:00:00Z".parse().unwrap();
        let token = signer.token(
            &[
                ("team-a/api", Access::PUSH),
                ("team-a/api/source", Access::PULL),
            ],
            now,
            Duration::from_mins(10),
        );
        let (input, signature) = token.rsplit_once('.').unwrap();
        UnparsedPublicKey::new(&ED25519, signer.public_key())
            .verify(
                input.as_bytes(),
                &URL_SAFE_NO_PAD.decode(signature).unwrap(),
            )
            .unwrap();
        let t = now.as_second();
        let skew = i64::try_from(CLOCK_SKEW.as_secs()).unwrap();
        assert_eq!(
            claims(&token),
            json!({
                "iss": "zeabur-builder",
                "iat": t,
                "nbf": t - skew,
                "exp": t + 600,
                "access": [
                    {"type": "repository", "name": "team-a/api", "actions": ["pull", "push"]},
                    {"type": "repository", "name": "team-a/api/source", "actions": ["pull"]},
                ],
            })
        );
    }

    #[test]
    fn reads_a_key_without_its_public_half() {
        // PKCS#8 v1, as cert-manager writes it: the seed only.
        let mut der = vec![
            0x30, 0x2E, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2B, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        der.extend([7; 32]);
        assert!(Signer::from_pem(&pem(&der)).is_ok());
        assert!(Signer::from_pem(b"not a key").is_err());
    }

    #[test]
    fn a_token_replaces_the_hosts_entry() {
        let config = json!({"auths": {
            "https://zot.zot.svc:5000": {"auth": "dTpw"},
            "ghcr.io": {"auth": "Z2g6cA=="},
        }})
        .to_string();
        let config: serde_json::Value = serde_json::from_slice(
            &with_token(config.as_bytes(), "zot.zot.svc:5000", "t").unwrap(),
        )
        .unwrap();
        assert_eq!(
            config,
            json!({"auths": {
                "ghcr.io": {"auth": "Z2g6cA=="},
                "zot.zot.svc:5000": {"registrytoken": "t"},
            }})
        );
        let config: serde_json::Value =
            serde_json::from_slice(&docker_config("zot.zot.svc:5000", "t")).unwrap();
        assert_eq!(
            config,
            json!({"auths": {"zot.zot.svc:5000": {"registrytoken": "t"}}})
        );
    }

    #[test]
    fn push_credentials_need_the_label_and_the_type() {
        let secret = |label: Option<&str>, type_: &str, config: &str| Secret {
            metadata: kube::api::ObjectMeta {
                labels: label
                    .map(|v| BTreeMap::from([(labels::PUSH_CREDENTIALS.to_owned(), v.to_owned())])),
                ..Default::default()
            },
            type_: Some(type_.to_owned()),
            data: Some(BTreeMap::from([(
                ".dockerconfigjson".to_owned(),
                k8s_openapi::ByteString(config.as_bytes().to_vec()),
            )])),
            ..Secret::default()
        };
        let config = json!({"auths": {"ghcr.io": {"auth": "Z2g6cA=="}}}).to_string();
        let config = config.as_str();
        assert_eq!(
            push_config("push", &secret(Some("true"), DOCKER_CONFIG_JSON, config)).unwrap(),
            config.as_bytes()
        );
        for (case, refused) in [
            ("no label", secret(None, DOCKER_CONFIG_JSON, config)),
            (
                "label false",
                secret(Some("false"), DOCKER_CONFIG_JSON, config),
            ),
            ("Opaque type", secret(Some("true"), "Opaque", config)),
            (
                "not an object",
                secret(Some("true"), DOCKER_CONFIG_JSON, "[]"),
            ),
        ] {
            assert!(push_config("push", &refused).is_err(), "{case}");
        }
    }
}
