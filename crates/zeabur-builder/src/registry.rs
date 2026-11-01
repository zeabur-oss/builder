//! Tokens for the default registry, and the deletion of a `Build`'s repositories there.
//!
//! The registry trusts one Ed25519 key, such as the catalog Zot's `zot-token-signer`. The
//! controller reads the private key from that Secret and signs a JWT for each use, in the form of
//! the Docker [token authentication] `access` claim. The token names each repository exactly and
//! expires, so a run pushes only to its own `Build`'s repository and pulls only its own archives.
//!
//! [token authentication]: https://distribution.github.io/distribution/spec/auth/jwt/

use std::time::Duration;

use aws_lc_rs::signature::Ed25519KeyPair;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use bytes::Bytes;
use http::{Method, Request, StatusCode, header};
use http_body_util::{BodyExt as _, Full, Limited};
use jiff::Timestamp;
use k8s_openapi::api::core::v1::Secret;
use kube::{Api, Client};
use serde::Deserialize;
use serde_json::json;
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
    pub(crate) fn from_pem(pem: &[u8]) -> Result<Self, String> {
        let pem = std::str::from_utf8(pem).map_err(|_error| "the key is not PEM".to_owned())?;
        if !pem.contains("-----BEGIN PRIVATE KEY-----") {
            return Err("the key is not a PEM PKCS#8 private key".to_owned());
        }
        let body: String = pem
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("-----"))
            .collect();
        let der = STANDARD
            .decode(body)
            .map_err(|_error| "the key is not valid PEM".to_owned())?;
        let key = Ed25519KeyPair::from_pkcs8_maybe_unchecked(&der)
            .map_err(|e| format!("the key is not an Ed25519 key: {e}"))?;
        Ok(Self { key })
    }

    /// A token for `grants`, valid from `now` for `ttl`.
    pub(crate) fn token(&self, grants: &[(&str, Access)], now: Timestamp, ttl: Duration) -> String {
        let now = now.as_second();
        let ttl = i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
        let skew = i64::try_from(CLOCK_SKEW.as_secs()).unwrap_or(i64::MAX);
        let access: Vec<_> = grants
            .iter()
            .map(|(name, access)| {
                json!({"type": "repository", "name": name, "actions": access.actions()})
            })
            .collect();
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"EdDSA","typ":"JWT"}"#);
        let claims = json!({
            "iss": "zeabur-builder",
            "iat": now,
            "nbf": now.saturating_sub(skew),
            "exp": now.saturating_add(ttl),
            "access": access,
        });
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
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
) -> Result<Signer, String> {
    let secret = Api::<Secret>::namespaced(client.clone(), namespace)
        .get_opt(name)
        .await
        .map_err(|e| format!("reading Secret `{namespace}/{name}`: {e}"))?
        .ok_or_else(|| {
            format!("no Secret `{name}` with the registry token key in `{namespace}`")
        })?;
    let key = secret
        .data
        .and_then(|mut d| d.remove("tls.key"))
        .ok_or_else(|| format!("Secret `{namespace}/{name}` has no tls.key"))?;
    Signer::from_pem(&key.0).map_err(|e| format!("Secret `{namespace}/{name}`: {e}"))
}

/// A Docker config that sends `token` to `host` as it is.
pub(crate) fn docker_config(host: &str, token: &str) -> Vec<u8> {
    json!({"auths": {host: {"registrytoken": token}}})
        .to_string()
        .into_bytes()
}

/// Adds `token` for `host` to a Docker config, replacing any entry for `host`.
pub(crate) fn with_token(config: &[u8], host: &str, token: &str) -> Result<Vec<u8>, String> {
    let mut config: serde_json::Value = serde_json::from_slice(config)
        .map_err(|e| format!("cannot read the Docker config: {e}"))?;
    let auths = config
        .as_object_mut()
        .ok_or("the Docker config is not an object")?
        .entry("auths")
        .or_insert_with(|| json!({}));
    let auths = auths
        .as_object_mut()
        .ok_or("`auths` in the Docker config is not an object")?;
    auths.retain(|key, _| {
        let key = key
            .strip_prefix("https://")
            .or_else(|| key.strip_prefix("http://"))
            .unwrap_or(key);
        key.split('/').next() != Some(host)
    });
    auths.insert(host.to_owned(), json!({"registrytoken": token}));
    Ok(config.to_string().into_bytes())
}

/// The Docker config of `secret`, named `name`: a `kubernetes.io/dockerconfigjson` Secret with the
/// label `builder.zeabur.com/push-credentials: "true"`. The message says what is wrong.
pub(crate) fn push_config(name: &str, secret: &Secret) -> Result<Vec<u8>, String> {
    let labelled = secret
        .metadata
        .labels
        .as_ref()
        .and_then(|l| l.get(labels::PUSH_CREDENTIALS))
        .is_some_and(|v| v == "true");
    if !labelled {
        return Err(format!(
            "Secret `{name}` needs the label `{}: \"true\"`",
            labels::PUSH_CREDENTIALS
        ));
    }
    if secret.type_.as_deref() != Some(DOCKER_CONFIG_JSON) {
        return Err(format!(
            "Secret `{name}` is of type `{}`; use {DOCKER_CONFIG_JSON}",
            secret.type_.as_deref().unwrap_or("Opaque")
        ));
    }
    let config = secret
        .data
        .as_ref()
        .and_then(|d| d.get(".dockerconfigjson"))
        .ok_or_else(|| format!("Secret `{name}` has no .dockerconfigjson"))?;
    match serde_json::from_slice::<serde_json::Value>(&config.0) {
        Ok(serde_json::Value::Object(_)) => Ok(config.0.clone()),
        _ => Err(format!(
            "Secret `{name}`: .dockerconfigjson is not a Docker config"
        )),
    }
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
    ) -> Result<(StatusCode, http::HeaderMap, Bytes), String> {
        let url = format!("{}/v2/{path}", self.base);
        let request = Request::builder()
            .method(method.clone())
            .uri(&url)
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::ACCEPT, MANIFEST_TYPES.join(", "))
            .body(Full::new(Bytes::new()))
            .map_err(|e| e.to_string())?;
        let response = tokio::time::timeout(TIMEOUT, self.client.request(request))
            .await
            .map_err(|_elapsed| format!("{method} {url}: timed out"))?
            .map_err(|e| format!("{method} {url}: {e}"))?;
        let (parts, body) = response.into_parts();
        let body = tokio::time::timeout(TIMEOUT, Limited::new(body, MAX_ANSWER).collect())
            .await
            .map_err(|_elapsed| format!("{method} {url}: timed out"))?
            .map_err(|e| format!("{method} {url}: {e}"))?
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
    ) -> Result<(), String> {
        let (status, _, body) = self
            .send(Method::GET, &format!("{repository}/tags/list"), token)
            .await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        if status != StatusCode::OK {
            return Err(format!("listing the tags of {repository}: {status}"));
        }
        let tags: Tags = serde_json::from_slice(&body)
            .map_err(|e| format!("listing the tags of {repository}: {e}"))?;
        let mut deleted = Vec::new();
        for tag in tags.tags.unwrap_or_default() {
            let (status, headers, body) = self
                .send(Method::GET, &format!("{repository}/manifests/{tag}"), token)
                .await?;
            if status == StatusCode::NOT_FOUND {
                continue;
            }
            if status != StatusCode::OK {
                return Err(format!("reading {repository}:{tag}: {status}"));
            }
            let digest = headers
                .get("docker-content-digest")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
                .ok_or_else(|| format!("{repository}:{tag} has no Docker-Content-Digest"))?;
            if deleted.contains(&digest) {
                continue;
            }
            let manifest: Manifest = serde_json::from_slice(&body)
                .map_err(|e| format!("reading {repository}:{tag}: {e}"))?;
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
    ) -> Result<(), String> {
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
            Err(format!("deleting {repository}@{digest}: {status}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::signature::{ED25519, UnparsedPublicKey};

    use super::*;

    fn pem(der: &[u8]) -> Vec<u8> {
        format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            STANDARD.encode(der)
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
        let claims = claims(&token);
        assert_eq!(
            claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
            600
        );
        assert_eq!(
            claims["access"],
            json!([
                {"type": "repository", "name": "team-a/api", "actions": ["pull", "push"]},
                {"type": "repository", "name": "team-a/api/source", "actions": ["pull"]},
            ])
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
        let config = br#"{"auths":{
            "https://zot.zot.svc:5000":{"auth":"dTpw"},
            "ghcr.io":{"auth":"Z2g6cA=="}}}"#;
        let config: serde_json::Value =
            serde_json::from_slice(&with_token(config, "zot.zot.svc:5000", "t").unwrap()).unwrap();
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
                labels: label.map(|v| {
                    std::collections::BTreeMap::from([(
                        labels::PUSH_CREDENTIALS.to_owned(),
                        v.to_owned(),
                    )])
                }),
                ..Default::default()
            },
            type_: Some(type_.to_owned()),
            data: Some(std::collections::BTreeMap::from([(
                ".dockerconfigjson".to_owned(),
                k8s_openapi::ByteString(config.as_bytes().to_vec()),
            )])),
            ..Secret::default()
        };
        let config = r#"{"auths":{"ghcr.io":{"auth":"Z2g6cA=="}}}"#;
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
