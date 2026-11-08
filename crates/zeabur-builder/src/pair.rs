//! Pairs a requesting installation with this runner cluster (`remote-pair`).
//!
//! The runner's API server trusts the installation's builder controller through one `jwt`
//! authenticator of its `AuthenticationConfiguration`. The keys are a pinned copy of the
//! installation's JWKS, served from `--pins-url`, unless `--direct` points the authenticator at the
//! installation's own API server. Builds go to the namespace `zbr-<installation>`, where the
//! user `remote:<installation>` may manage `Build`s and `BuildRun`s and read pull grants.

use std::path::{Path, PathBuf};

use clap::Args;
use serde_json::{Value, json};
use zeabur_builder_api::{GROUP, labels, remote};

/// The longest installation id, so `zbr-<id>` and the names derived from it stay short.
const MAX_ID: usize = 20;

#[derive(Debug, Args)]
pub(crate) struct PairArgs {
    /// The installation's id, such as `inst-a`: lowercase letters, digits and inner dashes.
    #[arg(long)]
    installation: String,
    /// The installation's `/.well-known/openid-configuration`, from
    /// `kubectl get --raw /.well-known/openid-configuration`.
    #[arg(long)]
    discovery: PathBuf,
    /// The installation's `/openid/v1/jwks`, from `kubectl get --raw /openid/v1/jwks`. Not used
    /// with `--direct`.
    #[arg(long, required_unless_present = "direct")]
    jwks: Option<PathBuf>,
    /// The HTTPS base URL that serves the pinned copies, as the API server reaches it.
    #[arg(long, required_unless_present = "direct")]
    pins_url: Option<String>,
    /// The CA of `--pins-url`, or of the installation's API server with `--direct`, PEM.
    #[arg(long)]
    ca: PathBuf,
    /// Where to write the pinned copies, as `<installation>/...` under it.
    #[arg(long, default_value = ".")]
    out: PathBuf,
    /// Fetch the keys from the installation's API server at this URL instead of a pinned copy.
    #[arg(long)]
    direct: Option<String>,
    /// The controller's service account in the installation, as `<namespace>/<name>`.
    #[arg(long, default_value = "zeabur-builder/zeabur-builder")]
    controller: String,
}

/// Lowercase letters, digits and inner dashes, at most [`MAX_ID`] long.
pub(crate) fn check_id(id: &str) -> Result<(), String> {
    let valid = !id.is_empty()
        && id.len() <= MAX_ID
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "installation `{id}` must be 1 to {MAX_ID} lowercase letters, digits or inner dashes"
        ))
    }
}

/// The issuer of a discovery document. The default issuer is refused: the runner's API server
/// uses it itself, and two installations could not be told apart.
pub(crate) fn issuer(discovery: &Value) -> Result<String, String> {
    let issuer = discovery
        .get("issuer")
        .and_then(Value::as_str)
        .ok_or("the discovery document has no issuer")?;
    if !issuer.starts_with("https://") {
        return Err(format!("issuer `{issuer}` is not an https URL"));
    }
    if issuer.starts_with("https://kubernetes.default.svc") {
        return Err(format!(
            "issuer `{issuer}` is the Kubernetes default: give the installation's API server its own \
             --service-account-issuer"
        ));
    }
    Ok(issuer.to_owned())
}

/// Whether a JWKS document has at least one RSA or EC public key and no private part.
pub(crate) fn check_jwks(jwks: &Value) -> Result<(), String> {
    let keys = jwks
        .get("keys")
        .and_then(Value::as_array)
        .filter(|keys| !keys.is_empty())
        .ok_or("the JWKS document has no keys")?;
    for key in keys {
        match key.get("kty").and_then(Value::as_str) {
            Some("RSA" | "EC") => {}
            other => return Err(format!("key type {other:?} is not RSA or EC")),
        }
        if key.get("d").is_some() {
            return Err("the JWKS document holds a private key".to_owned());
        }
    }
    Ok(())
}

/// The pinned discovery document: the installation's, with `jwks_uri` at the pin.
pub(crate) fn pinned_discovery(issuer: &str, jwks_uri: &str) -> Value {
    json!({
        "issuer": issuer,
        "jwks_uri": jwks_uri,
        "response_types_supported": ["id_token"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256", "ES256"],
    })
}

/// One `jwt` entry of `AuthenticationConfiguration`.
pub(crate) fn authenticator(
    installation: &str,
    issuer: &str,
    discovery_url: &str,
    ca: &str,
    subject: &str,
) -> Value {
    json!({
        "issuer": {
            "url": issuer,
            "discoveryURL": discovery_url,
            "certificateAuthority": ca,
            "audiences": [remote::AUDIENCE],
            "audienceMatchPolicy": "MatchAny",
        },
        "claimValidationRules": [{
            "expression": format!("claims.sub == '{subject}'"),
            "message": format!("only the builder controller of {installation}"),
        }],
        "claimMappings": {
            "username": {"expression": format!("'{}'", remote::user(installation))},
            "groups": {"expression": format!("['{}']", remote::GROUP)},
        },
    })
}

/// The installation's namespace and what its controller may do there.
pub(crate) fn rbac(installation: &str) -> Vec<Value> {
    let namespace = remote::namespace(installation);
    let labels = json!({"app.kubernetes.io/managed-by": "zeabur-builder", labels::REMOTE_INSTALLATION: installation});
    vec![
        json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {"name": namespace, "labels": labels},
        }),
        json!({
            "apiVersion": "rbac.authorization.k8s.io/v1",
            "kind": "Role",
            "metadata": {"name": "zeabur-builder-remote", "namespace": namespace, "labels": labels},
            "rules": [
                {
                    "apiGroups": [GROUP],
                    "resources": ["builds", "buildruns"],
                    "verbs": ["create", "get", "list", "watch", "patch", "update", "delete"],
                },
                {"apiGroups": [""], "resources": ["secrets"], "verbs": ["get"]},
            ],
        }),
        json!({
            "apiVersion": "rbac.authorization.k8s.io/v1",
            "kind": "RoleBinding",
            "metadata": {"name": "zeabur-builder-remote", "namespace": namespace, "labels": labels},
            "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": "zeabur-builder-remote"},
            "subjects": [{"apiGroup": "rbac.authorization.k8s.io", "kind": "User", "name": remote::user(installation)}],
        }),
    ]
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    std::fs::write(path, text + "\n").map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes the pins and `authenticator.json` under `--out`, and returns the RBAC manifests.
pub(crate) fn run(args: &PairArgs) -> Result<String, String> {
    check_id(&args.installation)?;
    let (namespace, name) = args
        .controller
        .split_once('/')
        .ok_or("--controller must be `<namespace>/<name>`")?;
    let subject = format!("system:serviceaccount:{namespace}:{name}");
    let issuer = issuer(&read_json(&args.discovery)?)?;
    let ca =
        std::fs::read_to_string(&args.ca).map_err(|e| format!("{}: {e}", args.ca.display()))?;
    let dir = args.out.join(&args.installation);

    let discovery_url = if let Some(direct) = &args.direct {
        format!(
            "{}/.well-known/openid-configuration",
            direct.trim_end_matches('/')
        )
    } else {
        let (Some(jwks), Some(pins_url)) = (&args.jwks, &args.pins_url) else {
            return Err("--jwks and --pins-url are required without --direct".to_owned());
        };
        let jwks = read_json(jwks)?;
        check_jwks(&jwks)?;
        let base = format!("{}/{}", pins_url.trim_end_matches('/'), args.installation);
        write_json(&dir.join("jwks.json"), &jwks)?;
        write_json(
            &dir.join(".well-known/openid-configuration"),
            &pinned_discovery(&issuer, &format!("{base}/jwks.json")),
        )?;
        format!("{base}/.well-known/openid-configuration")
    };
    let entry = authenticator(&args.installation, &issuer, &discovery_url, &ca, &subject);
    write_json(&dir.join("authenticator.json"), &entry)?;

    let mut documents = Vec::new();
    for document in rbac(&args.installation) {
        documents.push(serde_json::to_string_pretty(&document).map_err(|e| e.to_string())?);
    }
    Ok(format!("---\n{}", documents.join("\n---\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_short_dns_labels() {
        assert!(check_id("inst-a").is_ok());
        for id in ["", "Inst", "a/b", "-a", "a-", "an-installation-id-too-long"] {
            assert!(check_id(id).is_err(), "{id}");
        }
    }

    #[test]
    fn refuses_the_default_issuer() {
        let shared = json!({"issuer": "https://kubernetes.default.svc.cluster.local"});
        assert!(
            issuer(&shared)
                .unwrap_err()
                .contains("--service-account-issuer")
        );
        assert!(issuer(&json!({"issuer": "http://a.example"})).is_err());
        let own = json!({"issuer": "https://a.builder.example"});
        assert_eq!(issuer(&own).unwrap(), "https://a.builder.example");
    }

    #[test]
    fn refuses_keys_that_cannot_verify_or_are_private() {
        assert!(check_jwks(&json!({"keys": [{"kty": "RSA", "n": "x", "e": "AQAB"}]})).is_ok());
        assert!(check_jwks(&json!({"keys": []})).is_err());
        assert!(check_jwks(&json!({"keys": [{"kty": "oct", "k": "x"}]})).is_err());
        assert!(check_jwks(&json!({"keys": [{"kty": "EC", "d": "x"}]})).is_err());
    }

    #[test]
    fn the_authenticator_maps_one_controller_to_its_installation() {
        let entry = authenticator(
            "inst-a",
            "https://a.builder.example",
            "https://pins/inst-a/.well-known/openid-configuration",
            "CA",
            "system:serviceaccount:zeabur-builder:zeabur-builder",
        );
        assert_eq!(
            entry["issuer"]["audiences"],
            json!(["zeabur-builder-runner"])
        );
        assert_eq!(
            entry["claimValidationRules"][0]["expression"],
            "claims.sub == 'system:serviceaccount:zeabur-builder:zeabur-builder'"
        );
        assert_eq!(
            entry["claimMappings"]["username"]["expression"],
            "'remote:inst-a'"
        );
        assert_eq!(
            entry["claimMappings"]["groups"]["expression"],
            "['zeabur-builder:remote']"
        );
    }

    #[test]
    fn the_role_reaches_only_builds_and_secrets_of_its_namespace() {
        let documents = rbac("inst-a");
        assert_eq!(documents[0]["metadata"]["name"], "zbr-inst-a");
        let rules = documents[1]["rules"].as_array().unwrap();
        assert_eq!(rules[0]["resources"], json!(["builds", "buildruns"]));
        assert_eq!(
            rules[1],
            json!({"apiGroups": [""], "resources": ["secrets"], "verbs": ["get"]})
        );
        assert_eq!(documents[2]["subjects"][0]["name"], "remote:inst-a");
    }
}
