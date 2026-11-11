//! mTLS between build Jobs and buildkitd: one CA per build namespace, made the first
//! time the namespace builds, with a server certificate for its buildkitd and a client
//! certificate for its Jobs.
//!
//! buildkitd asks for a client certificate that chains to `--tlscacert` and checks nothing else
//! (`RequireAndVerifyClientCert` in [main.go]), so the CA is the boundary: a namespace's client
//! certificate, which its members can read, opens no other namespace's daemon.
//!
//! Both pairs live in one Secret in the builder's namespace, created in one request: two runs that
//! start together in a new namespace cannot end up with a server and a client from different CAs.
//! buildkitd mounts the server files; the Jobs get a copy of the client files in their namespace,
//! under the names `buildctl --tlsdir` reads (`ca.pem`, `cert.pem`, `key.pem`). The CA key is not
//! kept. Rotation is not implemented; the certificates are valid until rcgen's default end date.
//!
//! [main.go]: https://github.com/moby/buildkit/blob/v0.34.1/cmd/buildkitd/main.go

use std::collections::BTreeMap;

use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, ObjectMeta, PostParams};
use kube::{Client, ResourceExt as _};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use zeabur_builder_api::labels;
use zeabur_builder_jobspec::buildkitd;

/// The PEM files of one side.
pub(crate) type Pems = BTreeMap<String, String>;

/// The keys of the client pair in the namespace's Secret, next to the server's `cert.pem` and
/// `key.pem`.
const CLIENT_CERT: &str = "client-cert.pem";
const CLIENT_KEY: &str = "client-key.pem";

/// Generates a CA and the server and client certificates it signs, for the `StatefulSet` Pods
/// behind the headless Service `service` in `namespace`.
pub(crate) fn generate(service: &str, namespace: &str) -> Result<(Pems, Pems), rcgen::Error> {
    let ca_key = KeyPair::generate()?;
    let mut ca = CertificateParams::new(Vec::<String>::new())?;
    ca.distinguished_name
        .push(DnType::CommonName, format!("zeabur-builder {service} CA"));
    ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_pem = ca.self_signed(&ca_key)?.pem();
    let issuer = Issuer::new(ca, ca_key);

    let leaf = |names: Vec<String>, cn: &str, usage: ExtendedKeyUsagePurpose| {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(names)?;
        params.distinguished_name.push(DnType::CommonName, cn);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![usage];
        let cert = params.signed_by(&key, &issuer)?;
        Ok::<_, rcgen::Error>(Pems::from([
            ("ca.pem".to_owned(), ca_pem.clone()),
            ("cert.pem".to_owned(), cert.pem()),
            ("key.pem".to_owned(), key.serialize_pem()),
        ]))
    };
    let server = leaf(
        vec![
            format!("*.{service}.{namespace}.svc"),
            format!("*.{service}.{namespace}.svc.cluster.local"),
        ],
        service,
        ExtendedKeyUsagePurpose::ServerAuth,
    )?;
    let client = leaf(
        Vec::new(),
        "zeabur-builder-job",
        ExtendedKeyUsagePurpose::ClientAuth,
    )?;
    Ok((server, client))
}

fn data(pems: &Pems) -> BTreeMap<String, ByteString> {
    pems.iter()
        .map(|(k, v)| (k.clone(), ByteString(v.clone().into_bytes())))
        .collect()
}

fn client_pems(secret: &Secret) -> Option<Pems> {
    let data = secret.data.as_ref()?;
    let text = |key: &str| {
        data.get(key)
            .and_then(|b| String::from_utf8(b.0.clone()).ok())
    };
    Some(Pems::from([
        ("ca.pem".to_owned(), text("ca.pem")?),
        ("cert.pem".to_owned(), text(CLIENT_CERT)?),
        ("key.pem".to_owned(), text(CLIENT_KEY)?),
    ]))
}

/// Makes the namespace's certificates unless its Secret `secret` exists in `daemon_namespace`,
/// and returns the client files. `None` when the Secret belongs to a deleted namespace of the same
/// name: a new namespace gets a new CA.
pub(crate) async fn ensure(
    client: &Client,
    daemon_namespace: &str,
    secret: &str,
    service: &str,
    owner: &OwnerReference,
) -> Result<Option<Pems>, kube::Error> {
    let namespace = &owner.name;
    let api: Api<Secret> = Api::namespaced(client.clone(), daemon_namespace);
    let invalid = || kube::Error::Service(format!("Secret `{secret}` lacks a client pair").into());
    if let Some(mut existing) = api.get_opt(secret).await? {
        if buildkitd::of_deleted_namespace(&existing.metadata, &owner.uid) {
            return Ok(None);
        }
        if !existing
            .owner_references()
            .iter()
            .any(|o| o.uid == owner.uid)
        {
            // Made before Secrets had an owner.
            existing.metadata.owner_references = Some(vec![owner.clone()]);
            existing = api
                .replace(secret, &PostParams::default(), &existing)
                .await?;
        }
        return client_pems(&existing).map(Some).ok_or_else(invalid);
    }
    let (server, client_pair) = generate(service, daemon_namespace)
        .map_err(|e| kube::Error::Service(e.to_string().into()))?;
    let mut pems = server;
    for (from, to) in [("cert.pem", CLIENT_CERT), ("key.pem", CLIENT_KEY)] {
        if let Some(pem) = client_pair.get(from) {
            pems.insert(to.to_owned(), pem.clone());
        }
    }
    let wanted = Secret {
        metadata: ObjectMeta {
            name: Some(secret.to_owned()),
            namespace: Some(daemon_namespace.to_owned()),
            labels: Some(BTreeMap::from([
                (labels::MANAGED_BY.to_owned(), labels::MANAGER.to_owned()),
                (labels::NAMESPACE.to_owned(), namespace.clone()),
            ])),
            owner_references: Some(vec![owner.clone()]),
            ..ObjectMeta::default()
        },
        data: Some(data(&pems)),
        ..Secret::default()
    };
    match api.create(&PostParams::default(), &wanted).await {
        Ok(created) => {
            tracing::info!(%namespace, %secret, "created buildkitd certificates");
            client_pems(&created).map(Some).ok_or_else(invalid)
        }
        // Another run in the namespace made them first; theirs are the ones buildkitd mounts.
        Err(kube::Error::Api(status)) if status.is_already_exists() => {
            client_pems(&api.get(secret).await?)
                .map(Some)
                .ok_or_else(invalid)
        }
        Err(e) => Err(e),
    }
}

/// Writes `data` to the Secret `name` in `namespace`, when missing or different.
pub(crate) async fn put_secret(
    client: &Client,
    namespace: &str,
    name: &str,
    type_: Option<String>,
    data: BTreeMap<String, ByteString>,
) -> Result<(), kube::Error> {
    let api: Api<Secret> = Api::namespaced(client.clone(), namespace);
    let mut wanted = Secret {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            labels: Some(BTreeMap::from([(
                labels::MANAGED_BY.to_owned(),
                labels::MANAGER.to_owned(),
            )])),
            ..ObjectMeta::default()
        },
        data: Some(data),
        type_,
        ..Secret::default()
    };
    match api.get_opt(name).await? {
        Some(existing) if existing.data == wanted.data => Ok(()),
        Some(existing) => {
            wanted.metadata.resource_version = existing.metadata.resource_version;
            api.replace(name, &PostParams::default(), &wanted)
                .await
                .map(drop)
        }
        // Runs started together race to create the copy; the loser finds it there.
        None => match api.create(&PostParams::default(), &wanted).await {
            Err(kube::Error::Api(status)) if status.is_already_exists() => Ok(()),
            result => result.map(drop),
        },
    }
}

/// Copies the client files into the build namespace as `name`.
pub(crate) async fn put_client(
    client: &Client,
    namespace: &str,
    name: &str,
    pems: &Pems,
) -> Result<(), kube::Error> {
    put_secret(client, namespace, name, None, data(pems)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_certificate_covers_the_pod() {
        let (server, client) = generate("buildkitd-team-a", "builder-system").unwrap();
        assert_eq!(server["ca.pem"], client["ca.pem"]);
        for pems in [&server, &client] {
            assert!(pems["cert.pem"].starts_with("-----BEGIN CERTIFICATE-----"));
            assert!(pems["key.pem"].starts_with("-----BEGIN PRIVATE KEY-----"));
        }
        let der = der_of(&server["cert.pem"]);
        let text = String::from_utf8_lossy(&der);
        assert!(text.contains("*.buildkitd-team-a.builder-system.svc.cluster.local"));
    }

    #[test]
    fn each_namespace_has_its_own_ca() {
        let (a, _) = generate("buildkitd-team-a", "builder-system").unwrap();
        let (b, _) = generate("buildkitd-team-b", "builder-system").unwrap();
        assert_ne!(a["ca.pem"], b["ca.pem"]);
    }

    #[test]
    fn client_files_come_from_the_client_keys() {
        let (server, client) = generate("buildkitd-team-a", "builder-system").unwrap();
        let mut pems = server;
        pems.insert(CLIENT_CERT.to_owned(), client["cert.pem"].clone());
        pems.insert(CLIENT_KEY.to_owned(), client["key.pem"].clone());
        let secret = Secret {
            data: Some(data(&pems)),
            ..Secret::default()
        };
        assert_eq!(client_pems(&secret), Some(client));
        assert_eq!(client_pems(&Secret::default()), None);
    }

    /// The DER of the first PEM block, enough to search for a SAN.
    fn der_of(pem: &str) -> Vec<u8> {
        use rustls::pki_types::CertificateDer;
        use rustls::pki_types::pem::PemObject as _;
        CertificateDer::from_pem_slice(pem.as_bytes())
            .unwrap()
            .to_vec()
    }
}
