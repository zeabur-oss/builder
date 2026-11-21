//! The uploaded-archive source: where its artifact lives, what the artifact must look like, and
//! which tar entries the Job may write.
//!
//! A client packs the source folder as a gzip tar and pushes it as an OCI artifact
//! (`docs/builds/archive-source.mdx`): an image manifest with `artifactType` [`ARTIFACT_TYPE`], the
//! empty config, and one layer of [`LAYER_MEDIA_TYPE`]. This is the form `oras push
//! --artifact-type` writes ([OCI image spec], "Guidelines for artifact usage").
//!
//! The repository is [`repository`]: the registry, the `Build`'s namespace, its name, and
//! `source`. A `Build` names only a digest, so it cannot read the archive of another namespace or
//! of another `Build`. Build names hold no `/`, so `<name>/source` never equals the image
//! repository of another `Build`.
//!
//! [OCI image spec]: https://github.com/opencontainers/image-spec/blob/v1.1.1/manifest.md#guidelines-for-artifact-usage

use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::result::is_digest;

/// The manifest's `artifactType`.
pub const ARTIFACT_TYPE: &str = "application/vnd.zeabur.source.v1+tar";
/// The media type of the only layer: the source folder as a gzip tar.
pub const LAYER_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
/// The manifest's media type.
pub const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
/// The config of an artifact without one ([OCI image spec], "Guidance for an Empty Descriptor").
///
/// [OCI image spec]: https://github.com/opencontainers/image-spec/blob/v1.1.1/manifest.md#guidance-for-an-empty-descriptor
pub const EMPTY_CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.empty.v1+json";
/// The last segment of the repository.
pub const REPOSITORY_SUFFIX: &str = "source";
/// The largest manifest read. The distribution spec asks registries to accept at least 4 MiB.
pub const MAX_MANIFEST_BYTES: u64 = 4 << 20;
/// The largest layer, compressed.
pub const MAX_LAYER_BYTES: u64 = 256 << 20;
/// The most bytes read from the decompressed tar stream, headers included.
pub const MAX_UNPACKED_BYTES: u64 = 1 << 30;

/// The repository of a `Build`'s archives: `<registry>/<namespace>/<build>/source`.
#[must_use]
pub fn repository(registry: &str, namespace: &str, build: &str) -> String {
    format!(
        "{}/{namespace}/{build}/{REPOSITORY_SUFFIX}",
        registry.trim_end_matches('/')
    )
}

/// The layer to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    /// Its digest.
    pub digest: String,
    /// Its size in bytes.
    pub size: u64,
}

/// Why an archive cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ArchiveError {
    /// The manifest's bytes do not hash to the digest the `Build` names.
    #[error("the manifest does not match {0}")]
    Digest(String),
    /// The manifest is not JSON of the expected form.
    #[error("the manifest cannot be read: {0}")]
    Manifest(String),
    /// The manifest is not an archive source.
    #[error("not an archive source: {0}")]
    Form(String),
    /// The layer is larger than [`MAX_LAYER_BYTES`].
    #[error("the archive is {0} bytes; the limit is {MAX_LAYER_BYTES}")]
    TooLarge(u64),
    /// A tar entry would be written outside the source folder.
    #[error("the archive entry `{0}` points outside the source folder")]
    Outside(String),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema_version: u32,
    #[serde(default)]
    media_type: Option<String>,
    #[serde(default)]
    artifact_type: Option<String>,
    config: Descriptor,
    layers: Vec<Descriptor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    media_type: String,
    digest: String,
    size: u64,
}

/// `sha256:` and the hexadecimal SHA-256 of `bytes`.
#[must_use]
pub fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{}", hex(&Sha256::digest(bytes)))
}

/// `bytes` in lowercase hexadecimal.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|b| [b >> 4, b & 0xF])
        .filter_map(|n| char::from_digit(u32::from(n), 16))
        .collect()
}

/// Checks a manifest against `digest` and returns its one layer.
///
/// The manifest's `artifactType`, or else its config's media type, must be `artifact_type`:
/// [`ARTIFACT_TYPE`] for an archive source, [`crate::plan::ARTIFACT_TYPE`] for a saved plan.
///
/// # Errors
///
/// When the bytes do not hash to `digest`, or the manifest is not of that form.
pub fn layer(manifest: &[u8], digest: &str, artifact_type: &str) -> Result<Layer, ArchiveError> {
    if !is_digest(digest) || sha256(manifest) != digest {
        return Err(ArchiveError::Digest(digest.to_owned()));
    }
    let manifest: Manifest =
        serde_json::from_slice(manifest).map_err(|e| ArchiveError::Manifest(e.to_string()))?;
    if manifest.schema_version != 2 || manifest.media_type.as_deref() != Some(MANIFEST_MEDIA_TYPE) {
        return Err(ArchiveError::Form(format!(
            "the mediaType must be {MANIFEST_MEDIA_TYPE}"
        )));
    }
    let found = manifest
        .artifact_type
        .as_deref()
        .unwrap_or(&manifest.config.media_type);
    if found != artifact_type {
        return Err(ArchiveError::Form(format!(
            "the artifactType is `{found}`, not {artifact_type}"
        )));
    }
    let [layer] = manifest.layers.as_slice() else {
        return Err(ArchiveError::Form(
            "it must have exactly one layer".to_owned(),
        ));
    };
    if layer.media_type != LAYER_MEDIA_TYPE || !is_digest(&layer.digest) {
        return Err(ArchiveError::Form(format!(
            "the layer must be a {LAYER_MEDIA_TYPE} with a sha256 digest"
        )));
    }
    if layer.size > MAX_LAYER_BYTES {
        return Err(ArchiveError::TooLarge(layer.size));
    }
    Ok(Layer {
        digest: layer.digest.clone(),
        size: layer.size,
    })
}

/// The path of a tar entry below the source folder: relative, without `..`, `.` segments removed.
/// The folder itself, such as `./`, gives an empty path.
///
/// # Errors
///
/// When the path is absolute or holds `..`.
pub fn entry_path(name: &Path) -> Result<PathBuf, ArchiveError> {
    let outside = || ArchiveError::Outside(name.display().to_string());
    let mut path = PathBuf::new();
    for component in name.components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(outside());
            }
        }
    }
    Ok(path)
}

/// Whether a symbolic link at `entry` (from [`entry_path`]) with `target` stays inside the source
/// folder. The target must be relative, and `..` may only lead it and climb no higher than the
/// folder.
///
/// The kernel resolves `a/..` after it follows `a`, so `..` after a name that is itself a link
/// could climb out. Leading `..` segments pass only the real directories that hold `entry`.
#[must_use]
pub fn link_stays_inside(entry: &Path, target: &Path) -> bool {
    let mut depth = entry.components().count().saturating_sub(1);
    let mut named = false;
    for component in target.components() {
        match component {
            Component::Normal(_) => named = true,
            Component::CurDir => {}
            Component::ParentDir if !named => match depth.checked_sub(1) {
                Some(up) => depth = up,
                None => return false,
            },
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const LAYER: &str = "sha256:2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae";

    fn manifest(artifact_type: &str, layers: &[(&str, u64)]) -> String {
        json!({
            "schemaVersion": 2,
            "mediaType": MANIFEST_MEDIA_TYPE,
            "artifactType": artifact_type,
            "config": {
                "mediaType": EMPTY_CONFIG_MEDIA_TYPE,
                "digest": "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
                "size": 2,
                "data": "e30=",
            },
            "layers": layers
                .iter()
                .map(|&(media_type, size)| json!({"mediaType": media_type, "digest": LAYER, "size": size}))
                .collect::<Vec<_>>(),
        })
        .to_string()
    }

    fn digest(bytes: &[u8]) -> String {
        sha256(bytes)
    }

    #[test]
    fn a_saved_plan_and_a_source_are_not_read_as_each_other() {
        let plan = crate::plan::ARTIFACT_TYPE;
        let saved = manifest(plan, &[(LAYER_MEDIA_TYPE, 1234)]);
        let source = manifest(ARTIFACT_TYPE, &[(LAYER_MEDIA_TYPE, 1234)]);

        assert!(layer(saved.as_bytes(), &digest(saved.as_bytes()), plan).is_ok());
        assert!(layer(saved.as_bytes(), &digest(saved.as_bytes()), ARTIFACT_TYPE).is_err());
        assert!(layer(source.as_bytes(), &digest(source.as_bytes()), plan).is_err());
    }

    #[test]
    fn sha256_of_known_input() {
        assert_eq!(sha256(b"foo"), LAYER);
    }

    #[test]
    fn an_oras_artifact_gives_its_layer() {
        let body = manifest(ARTIFACT_TYPE, &[(LAYER_MEDIA_TYPE, 1234)]);
        assert_eq!(
            layer(body.as_bytes(), &digest(body.as_bytes()), ARTIFACT_TYPE),
            Ok(Layer {
                digest: LAYER.to_owned(),
                size: 1234
            })
        );
    }

    #[test]
    fn a_manifest_that_does_not_hash_to_the_digest_is_refused() {
        let body = manifest(ARTIFACT_TYPE, &[(LAYER_MEDIA_TYPE, 1)]);
        let other = digest(b"another manifest");
        assert_eq!(
            layer(body.as_bytes(), &other, ARTIFACT_TYPE),
            Err(ArchiveError::Digest(other.clone()))
        );
        assert!(layer(body.as_bytes(), "sha512:00", ARTIFACT_TYPE).is_err());
    }

    #[test]
    fn manifests_of_another_form_are_refused() {
        let cases = [
            (
                "an image",
                manifest(
                    "application/vnd.oci.image.config.v1+json",
                    &[(LAYER_MEDIA_TYPE, 1)],
                ),
            ),
            (
                "an uncompressed layer",
                manifest(
                    ARTIFACT_TYPE,
                    &[("application/vnd.oci.image.layer.v1.tar", 1)],
                ),
            ),
            (
                "two layers",
                manifest(
                    ARTIFACT_TYPE,
                    &[(LAYER_MEDIA_TYPE, 1), (LAYER_MEDIA_TYPE, 1)],
                ),
            ),
            ("no layer", manifest(ARTIFACT_TYPE, &[])),
            (
                "too large",
                manifest(ARTIFACT_TYPE, &[(LAYER_MEDIA_TYPE, MAX_LAYER_BYTES + 1)]),
            ),
            ("not JSON", "{".to_owned()),
        ];
        for (name, body) in cases {
            assert!(
                layer(body.as_bytes(), &digest(body.as_bytes()), ARTIFACT_TYPE).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn entry_paths_stay_below_the_folder() {
        assert_eq!(
            entry_path(Path::new("./src/main.rs")),
            Ok(PathBuf::from("src/main.rs"))
        );
        assert_eq!(entry_path(Path::new("a/./b/")), Ok(PathBuf::from("a/b")));
        assert_eq!(entry_path(Path::new("./")), Ok(PathBuf::new()));
        for refused in ["/etc/passwd", "../x", "a/../../x", "a/../b"] {
            assert!(entry_path(Path::new(refused)).is_err(), "{refused}");
        }
    }

    #[test]
    fn links_may_not_climb_out() {
        let inside = [
            ("a/link", "b"),
            ("a/link", "../b"),
            ("a/b/link", "../../c"),
            ("link", "./a"),
        ];
        for (entry, target) in inside {
            assert!(
                link_stays_inside(Path::new(entry), Path::new(target)),
                "{entry} -> {target}"
            );
        }
        let outside = [
            ("link", ".."),
            ("a/link", "../../b"),
            ("link", "a/../b"),
            ("link", "/etc/passwd"),
        ];
        for (entry, target) in outside {
            assert!(
                !link_stays_inside(Path::new(entry), Path::new(target)),
                "{entry} -> {target}"
            );
        }
    }
}
