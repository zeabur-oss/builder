//! What the build container runs: the `buildctl` builds and the steps around them.
//!
//! `zeabur-builder-jobspec` writes a [`BuildPlan`] as JSON, the one argument of
//! `zeabur-builder-job build`, which reads it back. Neither side parses a command line.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// In a build after the first, the digest of the one before it.
pub const PREVIOUS_DIGEST: &str = "{digest}";

/// The build step of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildPlan {
    /// The registry speaks HTTP, not HTTPS.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plain_http: bool,
    /// Variables whose values are hashed into `build-arg:secrets-hash` of the first build, so
    /// Railpack's steps that read a secret run again when its value changes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets_hash_env: Vec<String>,
    /// Railpack's non-root rewrite, before the first build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_root: Option<NonRoot>,
    /// Where the planner's folder is pushed after a success ([`crate::plan`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_plan: Option<Image>,
    /// The builds, in order. The run's image is the last one's.
    pub builds: Vec<Build>,
}

/// Railpack's non-root rewrite ([`crate::railpack`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NonRoot {
    /// The plan folder the planner wrote.
    pub plan: String,
    /// Where the rewritten plan and the second build's Dockerfile go.
    pub out: String,
}

/// A tagged image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Image {
    /// `<host>/<name>`.
    pub repository: String,
    /// The tag, without the colon.
    pub tag: String,
}

/// One `buildctl build`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Build {
    /// `buildctl` and its arguments, without `--metadata-file` and `--output`.
    /// [`PREVIOUS_DIGEST`] stands for the digest of the build before.
    pub command: Vec<String>,
    /// The file `buildctl` writes the image digest to.
    pub metadata_file: String,
    /// The image it pushes.
    pub output: Output,
}

impl Build {
    /// The command line, with `previous` in place of [`PREVIOUS_DIGEST`].
    #[must_use]
    pub fn args(&self, previous: Option<&str>) -> Vec<String> {
        let mut args: Vec<String> = self
            .command
            .iter()
            .map(|arg| previous.map_or_else(|| arg.clone(), |d| arg.replace(PREVIOUS_DIGEST, d)))
            .collect();
        args.extend([
            "--metadata-file".to_owned(),
            self.metadata_file.clone(),
            "--output".to_owned(),
            self.output.csv(),
        ]);
        args
    }
}

/// The image a build pushes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Output {
    /// `<host>/<name>`.
    pub repository: String,
    /// The tags. With none, the image is pushed by digest only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// OCI annotations of the image.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

impl Output {
    /// The `--output` value: one CSV record, as `buildctl` reads it with Go's `encoding/csv`.
    #[must_use]
    pub fn csv(&self) -> String {
        let names = if self.tags.is_empty() {
            self.repository.clone()
        } else {
            self.tags
                .iter()
                .map(|tag| format!("{}:{tag}", self.repository))
                .collect::<Vec<_>>()
                .join(",")
        };
        let mut fields = vec![
            "type=image".to_owned(),
            format!("name={names}"),
            "push=true".to_owned(),
        ];
        if self.tags.is_empty() {
            fields.push("push-by-digest=true".to_owned());
        }
        fields.push("oci-mediatypes=true".to_owned());
        fields.extend(
            self.annotations
                .iter()
                .map(|(key, value)| format!("annotation.{key}={value}")),
        );
        fields
            .iter()
            .map(|field| csv_field(field))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// `field`, quoted when it holds a comma or a quote.
fn csv_field(field: &str) -> String {
    if field.contains([',', '"']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn build(tags: &[&str]) -> Build {
        Build {
            command: vec![
                "buildctl".to_owned(),
                "--opt".to_owned(),
                format!("context:app=docker-image://r/a@{PREVIOUS_DIGEST}"),
            ],
            metadata_file: "/workspace/out/metadata.json".to_owned(),
            output: Output {
                repository: "r.local:5000/a/api".to_owned(),
                tags: tags.iter().map(|&tag| tag.to_owned()).collect(),
                annotations: BTreeMap::from([(
                    "org.opencontainers.image.revision".to_owned(),
                    "3e1a9c2".to_owned(),
                )]),
            },
        }
    }

    #[test]
    fn tags_share_one_quoted_name_field() {
        assert_eq!(
            build(&["main-1-abc", "latest"]).output.csv(),
            "type=image,\"name=r.local:5000/a/api:main-1-abc,r.local:5000/a/api:latest\",\
             push=true,oci-mediatypes=true,annotation.org.opencontainers.image.revision=3e1a9c2"
        );
    }

    #[test]
    fn an_untagged_image_is_pushed_by_digest() {
        assert_eq!(
            build(&[]).output.csv(),
            "type=image,name=r.local:5000/a/api,push=true,push-by-digest=true,\
             oci-mediatypes=true,annotation.org.opencontainers.image.revision=3e1a9c2"
        );
    }

    #[test]
    fn a_later_build_names_the_image_the_one_before_built() {
        let build = build(&["v1"]);
        assert_eq!(
            build.args(Some("sha256:abc"))[2],
            "context:app=docker-image://r/a@sha256:abc"
        );
        assert_eq!(build.args(None)[2], build.command[2]);
        assert_eq!(
            build.args(None)[3..5],
            ["--metadata-file", "/workspace/out/metadata.json"]
        );
    }

    #[test]
    fn reads_what_it_writes() {
        let plan = BuildPlan {
            plain_http: true,
            secrets_hash_env: vec!["ZB_SECRET_A".to_owned()],
            non_root: Some(NonRoot {
                plan: "/workspace/plan".to_owned(),
                out: "/workspace/out/non-root".to_owned(),
            }),
            save_plan: Some(Image {
                repository: "r/a".to_owned(),
                tag: "zeabur-plan-1".to_owned(),
            }),
            builds: vec![build(&[]), build(&["v1"])],
        };
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(serde_json::from_str::<BuildPlan>(&json).unwrap(), plan);
        assert!(serde_json::from_str::<BuildPlan>(r#"{"builds":[],"extra":1}"#).is_err());
    }
}
