//! The build Job, as a pure function of a `BuildRun` and what the controller resolved for it, and
//! in [`buildkitd`] the daemon it builds on.
//!
//! One Job per `BuildRun`, `backoffLimit: 0`: retrying is the controller's decision. Its Pod:
//!
//! | container | image | does |
//! |---|---|---|
//! | `fetch` (init) | job image | `git fetch --depth=1 <url> <sha>` into `/workspace/src`, then checks `HEAD`; for an archive source, `zeabur-builder-job fetch-archive` |
//! | `plan` (init) | planner image | Railpack writes `/workspace/plan`; absent for `dockerfile` |
//! | `build` | job image | `zeabur-builder-job -- buildctl build …` |
//!
//! A Railpack build runs `buildctl` twice: Railpack's image, pushed by digest only, then a
//! Dockerfile build `FROM` it that makes it run as a non-root user and pushes the tags
//! ([`zeabur_builder_core::railpack`]). The first leaves an untagged manifest in the repository
//! for the registry's garbage collection.
//!
//! The job image is `moby/buildkit` (for `buildctl` and `git`) plus `zeabur-builder-job`, a small
//! static wrapper that runs the `buildctl` command it is given with `--progress rawjson`, prints
//! the progress for people, counts cached steps, reads the image digest from `--metadata-file`
//! and writes the termination message ([`zeabur_builder_core::result`]). The `buildctl`
//! arguments are built here, so they are reviewed and tested with the rest of the Job.
//!
//! An archive source is fetched from [`archive::repository`] of the run's own namespace and
//! `Build`, with the namespace's push credentials, the registry's CA and the run's digest. The
//! `Build` names no repository, so a run never reads another namespace's archive.
//!
//! Every user-controlled value (URL, revision, paths, variable names) reaches a shell only as a
//! positional parameter (`"$1"`), never inside the script text, and has `$` doubled, because the
//! kubelet expands `$(VAR)` in `command` and `args`.
//!
//! The Pod runs as a non-root user with a read-only root filesystem, no capabilities and no
//! service account token: `RUN` steps execute in buildkitd, so the Job needs no
//! privileges.

pub mod buildkitd;

use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Capabilities, Container, EmptyDirVolumeSource, EnvVar, EnvVarSource, KeyToPath,
    PodSecurityContext, PodSpec, PodTemplateSpec, SeccompProfile, SecretKeySelector,
    SecretVolumeSource, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use zeabur_builder_api::build::{Build, EnvVar as BuildEnvVar};
use zeabur_builder_api::build_run::BuildRun;
use zeabur_builder_api::cluster_builder::{BuilderType, ClusterBuilderSpec, FrontendType};
use zeabur_builder_api::{annotations, labels};
use zeabur_builder_core::repo::{RepoKey, RepoUrlError};
use zeabur_builder_core::result::is_digest;
use zeabur_builder_core::{archive, is_commit_sha};

/// The user the Pod runs as; `moby/buildkit` would otherwise run as root.
pub const RUN_AS: i64 = 1000;

const WORKSPACE: &str = "/workspace";
const SRC: &str = "/workspace/src";
const PLAN: &str = "/workspace/plan";
const OUT: &str = "/workspace/out";
const GIT_CREDENTIALS: &str = "/etc/zeabur-builder/git";
const BUILDKIT_TLS: &str = "/etc/zeabur-builder/buildkit";
const REGISTRY_AUTH: &str = "/etc/zeabur-builder/registry";
/// The registry's CA, PEM, in the `fetch` container of an archive source.
pub const REGISTRY_CA_ENV: &str = "ZB_REGISTRY_CA";
/// Prefix of the build container's variables that carry `Build.spec.env`, so a variable named
/// `PATH` or `BUILDKIT_HOST` cannot change how `buildctl` runs.
const SECRET_ENV_PREFIX: &str = "ZB_SECRET_";

/// How `fetch` authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitAuth<'a> {
    /// A public repository.
    Anonymous,
    /// A `kubernetes.io/basic-auth` Secret (`username`, `password`), such as a GitHub App
    /// installation token minted for this run.
    Basic {
        /// The Secret.
        secret: &'a str,
    },
    /// A `kubernetes.io/ssh-auth` Secret with `ssh-privatekey` and `known_hosts`.
    Ssh {
        /// The Secret.
        secret: &'a str,
    },
}

/// What the controller resolved for one `BuildRun`.
#[derive(Debug, Clone, Copy)]
pub struct JobInputs<'a> {
    /// The run. Its name, namespace and UID must be set.
    pub build_run: &'a BuildRun,
    /// Its `Build`.
    pub build: &'a Build,
    /// The `ClusterBuilder`'s name, for the label.
    pub builder_name: &'a str,
    /// The `ClusterBuilder`.
    pub builder: &'a ClusterBuilderSpec,
    /// The image repository to push to, without a tag.
    pub image: &'a str,
    /// The default registry, which holds the archives of archive sources.
    pub registry: &'a str,
    /// The default registry's CA, PEM, when a public CA did not sign its certificate.
    pub registry_ca: Option<&'a str>,
    /// Whether the default registry speaks plain HTTP. Only for a test cluster.
    pub registry_http: bool,
    /// The rendered tags, the first one from the template.
    pub tags: &'a [String],
    /// The namespace's buildkitd, `tcp://buildkitd-<ns>-0.buildkitd-<ns>.<builder ns>.svc:1234`.
    pub buildkitd_addr: &'a str,
    /// The job image.
    pub job_image: &'a str,
    /// How `fetch` authenticates.
    pub git_auth: GitAuth<'a>,
    /// The client certificate Secret: `ca.pem`, `cert.pem`, `key.pem` (`buildctl --tlsdir`).
    pub buildkit_tls_secret: &'a str,
    /// The `kubernetes.io/dockerconfigjson` Secret to push with.
    pub registry_auth_secret: &'a str,
    /// `activeDeadlineSeconds`: planner timeout plus build timeout.
    pub deadline_seconds: i64,
    /// `ttlSecondsAfterFinished`, long enough for log collectors to read the last lines.
    pub ttl_seconds: i32,
}

/// Why a Job cannot be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JobError {
    /// The `BuildRun` has no name, namespace or UID yet.
    #[error("the BuildRun has no {0}")]
    Metadata(&'static str),
    /// The `BuildRun`'s name does not fit the `job-name` label (63 characters).
    #[error("the BuildRun name `{0}` is longer than 63 characters")]
    Name(String),
    /// No tag to push.
    #[error("no tags to push")]
    NoTags,
    /// The revision is not a full commit SHA for a Git source, or not a digest for an archive.
    #[error("the revision `{0}` does not fit the source")]
    Revision(String),
    /// The `Build` sets neither `git` nor `archive`.
    #[error("the Build has no source")]
    NoSource,
    /// The repository URL is not one `fetch` may use.
    #[error(transparent)]
    Url(#[from] RepoUrlError),
    /// The builder has a planner when it should not, or lacks one, or a gateway frontend has no
    /// source. Admission rules prevent this; the check is repeated for `ClusterBuilder`s written
    /// before a rule existed.
    #[error("the ClusterBuilder is inconsistent: {0}")]
    Builder(&'static str),
}

/// Builds the Job.
///
/// # Errors
///
/// When the `BuildRun` lacks metadata, has a name too long for a Job, or the builder is
/// inconsistent.
pub fn job(inputs: &JobInputs<'_>) -> Result<Job, JobError> {
    let meta = &inputs.build_run.metadata;
    let name = meta.name.as_deref().ok_or(JobError::Metadata("name"))?;
    let namespace = meta
        .namespace
        .as_deref()
        .ok_or(JobError::Metadata("namespace"))?;
    let uid = meta.uid.as_deref().ok_or(JobError::Metadata("uid"))?;
    if name.len() > 63 {
        return Err(JobError::Name(name.to_owned()));
    }
    if inputs.tags.is_empty() {
        return Err(JobError::NoTags);
    }
    let revision = &inputs.build_run.spec.revision;
    let source = &inputs.build.spec.source;
    let repo = match (&source.git, &source.archive) {
        (Some(git), _) if is_commit_sha(revision) => Some(RepoKey::parse(&git.url)?),
        (None, Some(_)) if is_digest(revision) => None,
        (None, None) => return Err(JobError::NoSource),
        _ => return Err(JobError::Revision(revision.clone())),
    };

    let labels = BTreeMap::from([
        (
            labels::BUILD.to_owned(),
            inputs.build_run.spec.build_ref.clone(),
        ),
        (labels::BUILD_RUN.to_owned(), name.to_owned()),
        (labels::BUILDER.to_owned(), inputs.builder_name.to_owned()),
        (labels::MANAGED_BY.to_owned(), labels::MANAGER.to_owned()),
    ]);
    let annotations = BTreeMap::from([(annotations::REVISION.to_owned(), revision.clone())]);

    let fetch = source.git.as_ref().map_or_else(
        || archive_container(inputs, namespace),
        |git| fetch_container(inputs, &git.url),
    );
    let mut init_containers = vec![fetch];
    if let Some(plan) = plan_container(inputs)? {
        init_containers.push(plan);
    }

    let pod = PodSpec {
        restart_policy: Some("Never".to_owned()),
        automount_service_account_token: Some(false),
        enable_service_links: Some(false),
        security_context: Some(PodSecurityContext {
            run_as_non_root: Some(true),
            run_as_user: Some(RUN_AS),
            run_as_group: Some(RUN_AS),
            fs_group: Some(RUN_AS),
            seccomp_profile: Some(SeccompProfile {
                type_: "RuntimeDefault".to_owned(),
                ..SeccompProfile::default()
            }),
            ..PodSecurityContext::default()
        }),
        init_containers: Some(init_containers),
        containers: vec![build_container(inputs, repo.as_ref())?],
        volumes: Some(volumes(inputs)),
        ..PodSpec::default()
    };

    Ok(Job {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            labels: Some(labels.clone()),
            annotations: Some(annotations.clone()),
            owner_references: Some(vec![OwnerReference {
                api_version: format!("{}/v1alpha1", zeabur_builder_api::GROUP),
                kind: "BuildRun".to_owned(),
                name: name.to_owned(),
                uid: uid.to_owned(),
                controller: Some(true),
                block_owner_deletion: Some(true),
            }]),
            ..ObjectMeta::default()
        },
        spec: Some(JobSpec {
            backoff_limit: Some(0),
            active_deadline_seconds: Some(inputs.deadline_seconds),
            ttl_seconds_after_finished: Some(inputs.ttl_seconds),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    annotations: Some(annotations),
                    ..ObjectMeta::default()
                }),
                spec: Some(pod),
            },
            ..JobSpec::default()
        }),
        status: None,
    })
}

/// Fetches exactly one commit; `$1` is the URL and `$2` the SHA, both checked before the Job is
/// built. `git fetch <sha>` needs the server to allow it, which GitHub,
/// GitLab, Gitea and `git daemon` (protocol v2) do; the checkout is then verified against the SHA.
const FETCH_SCRIPT: &str = r#"set -eu
start=$(date +%s)
mkdir -p /workspace/plan /workspace/out
if [ -f /etc/zeabur-builder/git/ssh-privatekey ]; then
  install -m 0600 /etc/zeabur-builder/git/ssh-privatekey /tmp/id
  export GIT_SSH_COMMAND="ssh -i /tmp/id -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=/etc/zeabur-builder/git/known_hosts"
fi
if [ -f /etc/zeabur-builder/git/password ]; then
  git config --global credential.helper '!f() { echo username=$(cat /etc/zeabur-builder/git/username); echo password=$(cat /etc/zeabur-builder/git/password); }; f'
fi
git init -q /workspace/src
git -C /workspace/src fetch -q --depth=1 --no-tags -- "$1" "$2"
git -C /workspace/src checkout -q --detach FETCH_HEAD
test "$(git -C /workspace/src rev-parse HEAD)" = "$2"
echo "{\"fetch\":$(( $(date +%s) - start ))}" > /workspace/out/fetch.json
"#;

fn fetch_container(inputs: &JobInputs<'_>, url: &str) -> Container {
    let mut mounts = workspace_mounts();
    if !matches!(inputs.git_auth, GitAuth::Anonymous) {
        mounts.push(mount("git-credentials", GIT_CREDENTIALS));
    }
    Container {
        name: "fetch".to_owned(),
        image: Some(inputs.job_image.to_owned()),
        command: Some(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            FETCH_SCRIPT.to_owned(),
            "fetch".to_owned(),
        ]),
        args: Some(vec![literal(url), inputs.build_run.spec.revision.clone()]),
        env: Some(vec![
            env("HOME", "/tmp"),
            env("GIT_TERMINAL_PROMPT", "0"),
            env("GIT_CONFIG_NOSYSTEM", "1"),
            // `ext::` and `file://` URLs would run commands or read the Pod's files.
            env("GIT_ALLOW_PROTOCOL", "https:http:ssh:git"),
        ]),
        volume_mounts: Some(mounts),
        // Git's error is on stderr; the controller shows its last lines as the run's message.
        termination_message_policy: Some("FallbackToLogsOnError".to_owned()),
        ..hardened()
    }
}

/// Fetches the run's archive from the repository of its own namespace and `Build`, with the
/// namespace's push credentials, into `/workspace/src`.
fn archive_container(inputs: &JobInputs<'_>, namespace: &str) -> Container {
    let repository =
        archive::repository(inputs.registry, namespace, &inputs.build_run.spec.build_ref);
    let mut command = vec!["zeabur-builder-job".to_owned(), "fetch-archive".to_owned()];
    if inputs.registry_http {
        command.push("--plain-http".to_owned());
    }
    command.extend([
        literal(&repository),
        inputs.build_run.spec.revision.clone(),
        SRC.to_owned(),
    ]);
    let mut env_vars = vec![env("HOME", "/tmp"), env("DOCKER_CONFIG", REGISTRY_AUTH)];
    if let Some(ca) = inputs.registry_ca {
        env_vars.push(env(REGISTRY_CA_ENV, &literal(ca)));
    }
    let mut mounts = workspace_mounts();
    mounts.push(mount("registry-auth", REGISTRY_AUTH));
    Container {
        name: "fetch".to_owned(),
        image: Some(inputs.job_image.to_owned()),
        command: Some(command),
        env: Some(env_vars),
        volume_mounts: Some(mounts),
        termination_message_policy: Some("FallbackToLogsOnError".to_owned()),
        ..hardened()
    }
}

/// Runs a planner and records how long it took.
const PLAN_SCRIPT: &str = r#"set -eu
start=$(date +%s)
"$@"
echo "{\"plan\":$(( $(date +%s) - start ))}" > /workspace/out/plan.json
"#;

fn plan_container(inputs: &JobInputs<'_>) -> Result<Option<Container>, JobError> {
    let builder = inputs.builder;
    let planner = match (builder.kind, &builder.planner) {
        (BuilderType::Dockerfile, None) => return Ok(None),
        (BuilderType::Dockerfile, Some(_)) => {
            return Err(JobError::Builder("dockerfile with a planner"));
        }
        (BuilderType::Railpack, None) => {
            return Err(JobError::Builder("planner missing"));
        }
        (BuilderType::Railpack, Some(planner)) => planner,
    };
    let context = context_path(inputs);
    let mounts = workspace_mounts();
    let mut env_vars = vec![env("HOME", "/tmp")];
    let mut args = vec![
        "railpack".to_owned(),
        "prepare".to_owned(),
        literal(&context),
        "--plan-out".to_owned(),
        format!("{PLAN}/railpack-plan.json"),
        "--info-out".to_owned(),
        format!("{PLAN}/railpack-info.json"),
    ];
    // Railpack records only the names in the plan; values come from the build's secrets.
    for var in &inputs.build.spec.env {
        args.push("--env".to_owned());
        args.push(format!("{}=$({SECRET_ENV_PREFIX}{})", var.name, var.name));
        env_vars.push(secret_env(var));
    }
    Ok(Some(Container {
        name: "plan".to_owned(),
        image: Some(planner.image.clone()),
        command: Some(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            PLAN_SCRIPT.to_owned(),
            "plan".to_owned(),
        ]),
        args: Some(args),
        env: Some(env_vars),
        volume_mounts: Some(mounts),
        termination_message_policy: Some("FallbackToLogsOnError".to_owned()),
        ..hardened()
    }))
}

fn build_container(inputs: &JobInputs<'_>, repo: Option<&RepoKey>) -> Result<Container, JobError> {
    let context = context_path(inputs);
    let non_root = non_root(inputs.builder);
    let mut wrapper = vec!["zeabur-builder-job".to_owned()];
    if inputs.registry_http {
        wrapper.push("--plain-http".to_owned());
    }
    let metadata = if non_root {
        format!("{OUT}/railpack-metadata.json")
    } else {
        format!("{OUT}/metadata.json")
    };
    let mut buildctl = buildctl(inputs, &metadata);
    buildctl.extend(["--local".to_owned(), format!("context={context}")]);
    frontend_args(inputs, &context, &mut wrapper, &mut buildctl)?;
    for var in &inputs.build.spec.env {
        buildctl.extend([
            "--secret".to_owned(),
            format!("id={},env={SECRET_ENV_PREFIX}{}", var.name, var.name),
        ]);
    }
    if non_root {
        buildctl.extend(output(inputs, repo, &[inputs.image.to_owned()], true));
    } else {
        buildctl.extend(output(inputs, repo, &tagged(inputs), false));
    }
    if non_root {
        wrapper.extend(["--non-root".to_owned(), PLAN.to_owned()]);
    }
    wrapper.push("--".to_owned());
    wrapper.extend(buildctl.iter().map(|arg| literal(arg)));
    if non_root {
        wrapper.push("--then".to_owned());
        wrapper.extend(non_root_build(inputs, repo).iter().map(|arg| literal(arg)));
    }

    let mut env_vars = vec![env("HOME", "/tmp"), env("DOCKER_CONFIG", REGISTRY_AUTH)];
    if let Some(ca) = inputs.registry_ca {
        env_vars.push(env(REGISTRY_CA_ENV, &literal(ca)));
    }
    env_vars.extend(inputs.build.spec.env.iter().map(secret_env));
    let mut mounts = workspace_mounts();
    mounts.push(mount("buildkit-client-tls", BUILDKIT_TLS));
    mounts.push(mount("registry-auth", REGISTRY_AUTH));
    Ok(Container {
        name: "build".to_owned(),
        image: Some(inputs.job_image.to_owned()),
        command: Some(wrapper),
        env: Some(env_vars),
        volume_mounts: Some(mounts),
        termination_message_policy: Some("File".to_owned()),
        termination_message_path: Some("/dev/termination-log".to_owned()),
        ..hardened()
    })
}

/// Whether the build ends with [`non_root_build`]: Railpack's frontend writes an image that runs
/// as root ([`zeabur_builder_core::railpack`]).
fn non_root(builder: &ClusterBuilderSpec) -> bool {
    builder.kind == BuilderType::Railpack && builder.frontend.kind == FrontendType::Gateway
}

/// `buildctl … build` up to the frontend.
fn buildctl(inputs: &JobInputs<'_>, metadata: &str) -> Vec<String> {
    vec![
        "buildctl".to_owned(),
        "--addr".to_owned(),
        inputs.buildkitd_addr.to_owned(),
        "--tlsdir".to_owned(),
        BUILDKIT_TLS.to_owned(),
        "build".to_owned(),
        "--progress".to_owned(),
        "rawjson".to_owned(),
        "--metadata-file".to_owned(),
        metadata.to_owned(),
    ]
}

/// The second build: the Railpack image, pushed by digest only, made to run as
/// [`IMAGE_UID`](zeabur_builder_core::railpack::IMAGE_UID) by the Dockerfile `zeabur-builder-job
/// --non-root` writes, and pushed with the tags. `{digest}` is the first build's digest.
fn non_root_build(inputs: &JobInputs<'_>, repo: Option<&RepoKey>) -> Vec<String> {
    let dir = format!("{PLAN}/non-root");
    let mut buildctl = buildctl(inputs, &format!("{OUT}/metadata.json"));
    buildctl.extend([
        "--frontend".to_owned(),
        "dockerfile.v0".to_owned(),
        "--local".to_owned(),
        format!("context={dir}"),
        "--local".to_owned(),
        format!("dockerfile={dir}"),
        "--opt".to_owned(),
        format!("context:app=docker-image://{}@{{digest}}", inputs.image),
    ]);
    buildctl.extend(output(inputs, repo, &tagged(inputs), false));
    buildctl
}

/// `--frontend`, the plan's location and the frontend options.
fn frontend_args(
    inputs: &JobInputs<'_>,
    context: &str,
    wrapper: &mut Vec<String>,
    buildctl: &mut Vec<String>,
) -> Result<(), JobError> {
    let builder = inputs.builder;
    match (builder.frontend.kind, builder.kind) {
        (FrontendType::Dockerfile, BuilderType::Dockerfile) => {
            let path = inputs
                .build
                .spec
                .source
                .dockerfile()
                .unwrap_or("Dockerfile");
            let (dir, file) = path
                .rsplit_once('/')
                .map_or((None, path), |(d, f)| (Some(d), f));
            let dir = dir.map_or_else(|| context.to_owned(), |d| format!("{context}/{d}"));
            buildctl.extend(["--frontend".to_owned(), "dockerfile.v0".to_owned()]);
            buildctl.extend(["--local".to_owned(), format!("dockerfile={dir}")]);
            buildctl.extend(["--opt".to_owned(), format!("filename={file}")]);
        }
        (FrontendType::Dockerfile, BuilderType::Railpack) => {
            buildctl.extend(["--frontend".to_owned(), "dockerfile.v0".to_owned()]);
            buildctl.extend(["--local".to_owned(), format!("dockerfile={PLAN}")]);
            buildctl.extend(["--opt".to_owned(), "filename=Dockerfile".to_owned()]);
        }
        (FrontendType::Gateway, _) => {
            let source = builder
                .frontend
                .source
                .as_deref()
                .ok_or(JobError::Builder("gateway frontend without source"))?;
            buildctl.extend(["--frontend".to_owned(), "gateway.v0".to_owned()]);
            buildctl.extend(["--opt".to_owned(), format!("source={source}")]);
            buildctl.extend(["--local".to_owned(), format!("dockerfile={PLAN}")]);
            if builder.kind == BuilderType::Railpack {
                // Railpack reads build arguments only under `build-arg:`; the wrapper hashes the
                // secret values into `build-arg:secrets-hash` so a changed value invalidates the
                // cache without the value entering the cache key or image metadata.
                for var in &inputs.build.spec.env {
                    wrapper.extend([
                        "--secrets-hash-env".to_owned(),
                        format!("{SECRET_ENV_PREFIX}{}", var.name),
                    ]);
                }
                buildctl.extend([
                    "--opt".to_owned(),
                    format!("build-arg:cache-key={}", cache_key(inputs)),
                ]);
            }
        }
    }
    for (key, value) in &builder.frontend.opts {
        buildctl.extend(["--opt".to_owned(), format!("{key}={value}")]);
    }
    Ok(())
}

/// `<image>:<tag>` for each tag.
fn tagged(inputs: &JobInputs<'_>) -> Vec<String> {
    inputs
        .tags
        .iter()
        .map(|tag| format!("{}:{tag}", inputs.image))
        .collect()
}

/// `--output`, pushing `names`; `by_digest` pushes the image without a tag.
fn output(
    inputs: &JobInputs<'_>,
    repo: Option<&RepoKey>,
    names: &[String],
    by_digest: bool,
) -> [String; 2] {
    // `--output` is one CSV record. The source annotation is the repository key, never the URL,
    // which may carry a token (`https://token@host/…`).
    let mut output = vec![
        "type=image".to_owned(),
        format!("name={}", names.join(",")),
        "push=true".to_owned(),
    ];
    if by_digest {
        output.push("push-by-digest=true".to_owned());
    }
    output.push("oci-mediatypes=true".to_owned());
    if let Some(repo) = repo {
        output.push(format!(
            "annotation.org.opencontainers.image.source=https://{repo}"
        ));
    }
    output.extend([format!(
        "annotation.org.opencontainers.image.revision={}",
        inputs.build_run.spec.revision
    )]);
    [
        "--output".to_owned(),
        output
            .iter()
            .map(|field| csv_field(field))
            .collect::<Vec<_>>()
            .join(","),
    ]
}

/// Railpack's cache key: one per `Build`, so its cache mounts are shared by every run of the
/// `Build` and by nothing else.
fn cache_key(inputs: &JobInputs<'_>) -> String {
    let namespace = inputs
        .build
        .metadata
        .namespace
        .as_deref()
        .unwrap_or_default();
    let name = inputs.build.metadata.name.as_deref().unwrap_or_default();
    format!("{namespace}/{name}")
}

/// Quotes a CSV field when it holds a comma or quote, as `buildctl` parses `--output` with
/// `encoding/csv`.
fn csv_field(field: &str) -> String {
    if field.contains([',', '"']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_owned()
    }
}

/// Doubles `$` so the kubelet passes the value through unexpanded.
fn literal(value: &str) -> String {
    value.replace('$', "$$")
}

fn context_path(inputs: &JobInputs<'_>) -> String {
    match inputs
        .build
        .spec
        .source
        .context_dir()
        .map(|d| d.trim_matches('/'))
    {
        None | Some("" | ".") => SRC.to_owned(),
        Some(dir) => format!("{SRC}/{dir}"),
    }
}

fn volumes(inputs: &JobInputs<'_>) -> Vec<Volume> {
    let mut volumes = vec![empty_dir("workspace"), empty_dir("tmp")];
    match inputs.git_auth {
        GitAuth::Anonymous => {}
        GitAuth::Basic { secret } | GitAuth::Ssh { secret } => {
            volumes.push(secret_volume("git-credentials", secret, None));
        }
    }
    volumes.push(secret_volume(
        "buildkit-client-tls",
        inputs.buildkit_tls_secret,
        None,
    ));
    volumes.push(secret_volume(
        "registry-auth",
        inputs.registry_auth_secret,
        Some(vec![KeyToPath {
            key: ".dockerconfigjson".to_owned(),
            path: "config.json".to_owned(),
            mode: None,
        }]),
    ));
    volumes
}

fn workspace_mounts() -> Vec<VolumeMount> {
    vec![mount_rw("workspace", WORKSPACE), mount_rw("tmp", "/tmp")]
}

fn hardened() -> Container {
    Container {
        security_context: Some(SecurityContext {
            allow_privilege_escalation: Some(false),
            read_only_root_filesystem: Some(true),
            capabilities: Some(Capabilities {
                drop: Some(vec!["ALL".to_owned()]),
                add: None,
            }),
            ..SecurityContext::default()
        }),
        ..Container::default()
    }
}

fn env(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.to_owned(),
        value: Some(value.to_owned()),
        value_from: None,
    }
}

fn secret_env(var: &BuildEnvVar) -> EnvVar {
    EnvVar {
        name: format!("{SECRET_ENV_PREFIX}{}", var.name),
        value: var.value.clone(),
        value_from: var.value_from.as_ref().map(|from| EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: from.secret_key_ref.name.clone(),
                key: from.secret_key_ref.key.clone(),
                optional: Some(false),
            }),
            ..EnvVarSource::default()
        }),
    }
}

fn empty_dir(name: &str) -> Volume {
    Volume {
        name: name.to_owned(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Volume::default()
    }
}

fn secret_volume(name: &str, secret: &str, items: Option<Vec<KeyToPath>>) -> Volume {
    Volume {
        name: name.to_owned(),
        secret: Some(SecretVolumeSource {
            secret_name: Some(secret.to_owned()),
            items,
            default_mode: Some(0o440),
            optional: Some(false),
        }),
        ..Volume::default()
    }
}

fn mount(name: &str, path: &str) -> VolumeMount {
    VolumeMount {
        name: name.to_owned(),
        mount_path: path.to_owned(),
        read_only: Some(true),
        ..VolumeMount::default()
    }
}

fn mount_rw(name: &str, path: &str) -> VolumeMount {
    VolumeMount {
        name: name.to_owned(),
        mount_path: path.to_owned(),
        ..VolumeMount::default()
    }
}

#[cfg(test)]
mod tests;
