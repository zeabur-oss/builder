//! The default registry's CA, when a public CA did not sign the registry's certificate.
//!
//! The chart names a key of a Secret, such as `ca.crt` of the catalog Zot's `zot-tls`. The
//! controller watches that Secret and publishes the CA alone in the Secret [`PUBLISHED`] of its
//! own namespace. Each buildkitd mounts that Secret, and the chart lets every signed-in user read
//! it, so a client can trust the registry without reading the registry's own Secret, which also
//! holds its private key. A rotation of the CA reaches the controller, buildkitd and clients
//! without a reinstall.

use std::collections::BTreeMap;
use std::time::Duration;

use backon::{ExponentialBuilder, Retryable as _};
use futures_util::StreamExt as _;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use kube::Client;
use kube::api::{Api, ObjectMeta};
use kube::runtime::{WatchStreamExt as _, watcher};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;
use tokio::sync::watch;
use zeabur_builder_api::labels;
use zeabur_builder_jobspec::buildkitd;

use crate::tls;

/// The Secret, in the controller's namespace, that holds the CA alone.
pub(crate) const PUBLISHED: &str = buildkitd::REGISTRY_CA_SECRET;
/// The key of the CA in [`PUBLISHED`].
pub(crate) const KEY: &str = "ca.crt";

/// A key of a Secret that holds the CA, PEM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Source {
    pub(crate) namespace: String,
    pub(crate) name: String,
    pub(crate) key: String,
}

/// The CA the controller read last, `None` until the first read.
pub(crate) type Current = watch::Receiver<Option<String>>;

/// Follows `source` until the watch ends. Each CA it reads is sent to `current` and published in
/// [`PUBLISHED`] of `namespace`. A Secret without a certificate in the key keeps the last CA.
pub(crate) async fn follow(
    client: Client,
    source: Source,
    namespace: String,
    current: watch::Sender<Option<String>>,
) {
    let api = Api::<Secret>::namespaced(client.clone(), &source.namespace);
    let config = watcher::Config::default().fields(&format!("metadata.name={}", source.name));
    let secrets = watcher(api, config).default_backoff().applied_objects();
    let mut secrets = std::pin::pin!(secrets);
    while let Some(secret) = secrets.next().await {
        let secret = match secret {
            Ok(secret) => secret,
            Err(error) => {
                tracing::warn!(%error, "watching the registry CA's Secret");
                continue;
            }
        };
        let Some(ca) = ca_of(&secret, &source.key) else {
            tracing::warn!(
                namespace = source.namespace,
                secret = source.name,
                key = source.key,
                "the key holds no PEM certificate; the registry CA stays as it was"
            );
            continue;
        };
        current.send_replace(Some(ca.clone()));
        let published = published(&namespace, &ca);
        let applied = (|| tls::apply_secret(&client, &published))
            .retry(backoff())
            .notify(|error, wait| {
                tracing::warn!(%error, ?wait, "publishing the registry CA");
            })
            .await;
        if let Err(error) = applied {
            tracing::warn!(%error, "publishing the registry CA");
        }
    }
}

/// Retries a failed publish until it succeeds, at most a minute apart.
fn backoff() -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_max_delay(Duration::from_mins(1))
        .without_max_times()
        .with_jitter()
}

/// The PEM in `key`, when every block in it is a certificate and there is one at least.
fn ca_of(secret: &Secret, key: &str) -> Option<String> {
    let pem = secret.data.as_ref()?.get(key)?;
    let pem = std::str::from_utf8(&pem.0).ok()?;
    let mut certs = CertificateDer::pem_slice_iter(pem.as_bytes()).peekable();
    certs.peek()?;
    certs.all(|cert| cert.is_ok()).then(|| pem.to_owned())
}

/// The Secret [`PUBLISHED`] with `ca`.
fn published(namespace: &str, ca: &str) -> Secret {
    Secret {
        metadata: ObjectMeta {
            name: Some(PUBLISHED.to_owned()),
            namespace: Some(namespace.to_owned()),
            labels: Some(BTreeMap::from([(
                labels::MANAGED_BY.to_owned(),
                labels::MANAGER.to_owned(),
            )])),
            ..ObjectMeta::default()
        },
        data: Some(BTreeMap::from([(
            KEY.to_owned(),
            ByteString(ca.as_bytes().to_vec()),
        )])),
        ..Secret::default()
    }
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::Namespace;
    use kube::api::{DeleteParams, Patch, PatchParams, PostParams};

    use super::*;

    fn ca(name: &str) -> String {
        rcgen::generate_simple_self_signed(vec![name.to_owned()])
            .unwrap()
            .cert
            .pem()
    }

    fn secret(key: &str, value: &str) -> Secret {
        Secret {
            data: Some(BTreeMap::from([(
                key.to_owned(),
                ByteString(value.as_bytes().to_vec()),
            )])),
            ..Secret::default()
        }
    }

    #[test]
    fn the_ca_is_read_from_its_key() {
        let pem = ca("registry");
        assert_eq!(ca_of(&secret("ca.crt", &pem), "ca.crt"), Some(pem.clone()));
        let bundle = format!("{pem}{}", ca("other"));
        assert_eq!(ca_of(&secret("ca.crt", &bundle), "ca.crt"), Some(bundle));
    }

    #[test]
    fn a_key_without_a_certificate_gives_no_ca() {
        let pem = ca("registry");
        assert_eq!(ca_of(&secret("tls.crt", &pem), "ca.crt"), None);
        assert_eq!(ca_of(&secret("ca.crt", ""), "ca.crt"), None);
        assert_eq!(ca_of(&secret("ca.crt", "not a PEM"), "ca.crt"), None);
        let key = rcgen::KeyPair::generate().unwrap().serialize_pem();
        assert_eq!(ca_of(&secret("ca.crt", &key), "ca.crt"), None);
        assert_eq!(ca_of(&Secret::default(), "ca.crt"), None);
    }

    #[test]
    fn the_published_secret_holds_the_ca_alone() {
        let pem = ca("registry");
        let published = published("builder-system", &pem);
        assert_eq!(published.metadata.name.as_deref(), Some(PUBLISHED));
        assert_eq!(
            published.metadata.namespace.as_deref(),
            Some("builder-system")
        );
        assert_eq!(
            published.data,
            Some(BTreeMap::from([(
                KEY.to_owned(),
                ByteString(pem.into_bytes())
            )]))
        );
    }

    /// Waits until `current` holds `ca` and the published Secret too.
    async fn reaches(current: &mut Current, published: &Api<Secret>, ca: &str) {
        let read = |secret: Option<Secret>| {
            secret
                .and_then(|s| s.data)
                .and_then(|d| d.get(KEY).cloned())
                .map(|v| v.0)
        };
        tokio::time::timeout(Duration::from_secs(30), async {
            current
                .wait_for(|now| now.as_deref() == Some(ca))
                .await
                .unwrap();
            while read(published.get_opt(PUBLISHED).await.unwrap()).as_deref()
                != Some(ca.as_bytes())
            {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "needs a disposable cluster in KUBECONFIG (scripts/ci/kubernetes.sh)"]
    async fn a_rotated_ca_reaches_the_controller_and_the_published_secret() {
        let client = Client::try_default().await.unwrap();
        let namespaces: Api<Namespace> = Api::all(client.clone());
        let namespace = format!(
            "registry-ca-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let created = Namespace {
            metadata: ObjectMeta {
                name: Some(namespace.clone()),
                ..ObjectMeta::default()
            },
            ..Namespace::default()
        };
        namespaces
            .create(&PostParams::default(), &created)
            .await
            .unwrap();
        let secrets: Api<Secret> = Api::namespaced(client.clone(), &namespace);
        let source = |pem: &str| Secret {
            metadata: ObjectMeta {
                name: Some("zot-tls".to_owned()),
                ..ObjectMeta::default()
            },
            ..secret("ca.crt", pem)
        };
        let first = ca("first");
        secrets
            .create(&PostParams::default(), &source(&first))
            .await
            .unwrap();

        let (sender, mut current) = watch::channel(None);
        let following = tokio::spawn(follow(
            client.clone(),
            Source {
                namespace: namespace.clone(),
                name: "zot-tls".to_owned(),
                key: "ca.crt".to_owned(),
            },
            namespace.clone(),
            sender,
        ));
        let written = secrets.clone();
        let result = tokio::spawn(async move {
            reaches(&mut current, &written, &first).await;
            let second = ca("second");
            written
                .patch(
                    "zot-tls",
                    &PatchParams::default(),
                    &Patch::Merge(&source(&second)),
                )
                .await
                .unwrap();
            reaches(&mut current, &written, &second).await;
        })
        .await;
        following.abort();
        namespaces
            .delete(&namespace, &DeleteParams::default())
            .await
            .unwrap();
        result.unwrap();
    }
}
