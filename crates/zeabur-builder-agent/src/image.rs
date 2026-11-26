//! The user a trial build's image runs as, read from the OCI image layout that
//! `buildctl --output type=oci,dest=-` writes ([`image-layout.md`]).
//!
//! BuildKit returns no image config to `buildctl` without an export: it passes on only the
//! frontend's `frontend.` metadata ([`solver.go`]). The tarball holds every layer, so the reader
//! keeps only small JSON blobs and skips the rest as it streams past.
//!
//! The kubelet starts a container under `runAsNonRoot` only if the image's user is a numeric UID
//! other than 0. It treats an image without a user as root ([`helpers.go`],
//! [`security_context_others.go`]). containerd reads the UID from the part of `User` before `:`
//! ([`image_status.go`]).
//!
//! [`image-layout.md`]: https://github.com/opencontainers/image-spec/blob/v1.1.1/image-layout.md
//! [`solver.go`]: https://github.com/moby/buildkit/blob/v0.33.1/solver/llbsolver/solver.go
//! [`helpers.go`]: https://github.com/kubernetes/kubernetes/blob/v1.34.0/pkg/kubelet/kuberuntime/helpers.go
//! [`security_context_others.go`]: https://github.com/kubernetes/kubernetes/blob/v1.34.0/pkg/kubelet/kuberuntime/security_context_others.go
//! [`image_status.go`]: https://github.com/containerd/containerd/blob/v2.1.4/internal/cri/server/images/image_status.go

use std::collections::BTreeMap;
use std::io::Read;

use serde::Deserialize;
use serde::de::DeserializeOwned;

/// The largest blob kept: indexes, manifests and configs are far smaller.
const MAX_BLOB: u64 = 1 << 20;
/// The most bytes of blobs kept from one tarball.
const MAX_KEPT: usize = 8 << 20;
/// The most indexes followed from `index.json` to a manifest.
const MAX_DEPTH: usize = 4;
/// The highest UID the kubelet accepts (`validation.IsValidUserID`).
const MAX_UID: u64 = i32::MAX as u64;

/// Why the image's config could not be read.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigError {
    #[error("cannot read the image tarball: {0}")]
    Read(#[from] std::io::Error),
    #[error("the image tarball has no {0}")]
    Missing(String),
    #[error("cannot parse {name}: {source}")]
    Parse {
        name: String,
        source: serde_json::Error,
    },
    #[error("{0} is neither an image index nor a manifest")]
    MediaType(String),
    #[error("the image index nests deeper than {MAX_DEPTH} levels")]
    TooDeep,
}

#[derive(Deserialize)]
struct Index {
    manifests: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct Manifest {
    config: Descriptor,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    media_type: String,
    digest: String,
}

/// An image config ([`config.md`]), with the one field read here.
///
/// [`config.md`]: https://github.com/opencontainers/image-spec/blob/v1.1.1/config.md
#[derive(Deserialize)]
struct Image {
    #[serde(default)]
    config: Option<Config>,
}

#[derive(Deserialize)]
struct Config {
    #[serde(rename = "User", default)]
    user: Option<String>,
}

/// Reads the image layout tarball to its end and returns the config's `User`, empty when it has
/// none.
pub(crate) fn user(mut reader: impl Read) -> Result<String, ConfigError> {
    let blobs = json_blobs(&mut reader);
    // Drain the rest, so `buildctl` does not block on a full pipe after an error.
    std::io::copy(&mut reader, &mut std::io::sink())?;
    config_user(&blobs?)
}

/// `index.json` and the blobs that hold a JSON object, by digest.
fn json_blobs(reader: impl Read) -> Result<BTreeMap<String, Vec<u8>>, ConfigError> {
    let mut archive = tar::Archive::new(reader);
    let mut blobs = BTreeMap::new();
    let mut kept = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let size = entry.size();
        let name = if path == "index.json" {
            path
        } else if let Some(hex) = path.strip_prefix("blobs/sha256/") {
            format!("sha256:{hex}")
        } else {
            continue;
        };
        if size > MAX_BLOB || kept > MAX_KEPT {
            continue;
        }
        let mut blob = Vec::new();
        entry.read_to_end(&mut blob)?;
        // Layers are compressed or tar, never a JSON object.
        if blob.first() == Some(&b'{') {
            kept += blob.len();
            blobs.insert(name, blob);
        }
    }
    Ok(blobs)
}

/// Follows `index.json` to the first manifest and returns its config's `User`.
fn config_user(blobs: &BTreeMap<String, Vec<u8>>) -> Result<String, ConfigError> {
    let mut index: Index = parse(blobs, "index.json")?;
    for _ in 0..MAX_DEPTH {
        let first = index
            .manifests
            .into_iter()
            .next()
            .ok_or_else(|| ConfigError::Missing("manifest".to_owned()))?;
        match first.media_type.as_str() {
            "application/vnd.oci.image.index.v1+json"
            | "application/vnd.docker.distribution.manifest.list.v2+json" => {
                index = parse(blobs, &first.digest)?;
            }
            "application/vnd.oci.image.manifest.v1+json"
            | "application/vnd.docker.distribution.manifest.v2+json" => {
                let manifest: Manifest = parse(blobs, &first.digest)?;
                let image: Image = parse(blobs, &manifest.config.digest)?;
                return Ok(image.config.and_then(|c| c.user).unwrap_or_default());
            }
            other => return Err(ConfigError::MediaType(other.to_owned())),
        }
    }
    Err(ConfigError::TooDeep)
}

fn parse<T: DeserializeOwned>(
    blobs: &BTreeMap<String, Vec<u8>>,
    name: &str,
) -> Result<T, ConfigError> {
    let blob = blobs
        .get(name)
        .ok_or_else(|| ConfigError::Missing(name.to_owned()))?;
    serde_json::from_slice(blob).map_err(|source| ConfigError::Parse {
        name: name.to_owned(),
        source,
    })
}

/// Why an image with `user` does not start under `runAsNonRoot`, or `None` when it does.
pub(crate) fn runs_as_root(user: &str) -> Option<String> {
    let uid = user.split(':').next().unwrap_or_default();
    let problem = if uid.is_empty() {
        "the image sets no user, so it runs as root".to_owned()
    } else if !uid.bytes().all(|b| b.is_ascii_digit()) {
        format!("the image runs as `{user}`, a user name: Kubernetes accepts only a numeric UID")
    } else if uid
        .parse::<u64>()
        .is_ok_and(|uid| (1..=MAX_UID).contains(&uid))
    {
        return None;
    } else if uid.bytes().all(|b| b == b'0') {
        "the image runs as root (UID 0)".to_owned()
    } else {
        format!("the image runs as UID {uid}, which is not in 1 to {MAX_UID}")
    };
    Some(format!(
        "{problem}. The service runs with `runAsNonRoot` and would not start. End the final stage \
         with `USER <uid>`, a numeric UID other than 0 such as `USER 1000`, and give that user \
         the files and ports it needs, or use a base image that runs as a numeric non-root UID."
    ))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    /// A tarball with `files`, in order.
    fn tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn layout(user: Option<&str>) -> Vec<u8> {
        let user = user.map_or_else(String::new, |user| format!(r#""User":"{user}""#));
        let config = format!(r#"{{"architecture":"amd64","config":{{{user}}}}}"#);
        let manifest = r#"{"schemaVersion":2,"config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:c0","size":1},"layers":[]}"#;
        let index = r#"{"schemaVersion":2,"manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:a1","size":1}]}"#;
        tarball(&[
            ("oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#),
            ("blobs/sha256/1a", &[0x1F, 0x8B, 0, 0]),
            ("blobs/sha256/c0", config.as_bytes()),
            ("blobs/sha256/a1", manifest.as_bytes()),
            ("index.json", index.as_bytes()),
        ])
    }

    #[test]
    fn reads_the_user_of_the_config() {
        let tarball = layout(Some("1000:1000"));
        assert_eq!(user(tarball.as_slice()).unwrap(), "1000:1000");
    }

    #[test]
    fn a_config_without_a_user_reads_as_empty() {
        let tarball = layout(None);
        assert_eq!(user(tarball.as_slice()).unwrap(), "");
    }

    #[test]
    fn a_tarball_without_an_index_is_an_error() {
        let tarball = tarball(&[("oci-layout", b"{}")]);
        assert!(matches!(
            user(tarball.as_slice()),
            Err(ConfigError::Missing(name)) if name == "index.json"
        ));
    }

    #[test]
    fn only_a_numeric_uid_other_than_0_starts_under_run_as_non_root() {
        let cases = [
            ("", false),
            ("0", false),
            ("0:0", false),
            ("00", false),
            ("root", false),
            ("nginx", false),
            ("+1000", false),
            ("4294967296", false),
            ("1000", true),
            ("101:101", true),
            ("1000:0", true),
            ("65532:nonroot", true),
        ];
        for (user, starts) in cases {
            assert_eq!(runs_as_root(user).is_none(), starts, "{user:?}");
        }
    }
}
