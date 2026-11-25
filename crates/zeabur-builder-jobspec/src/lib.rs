//! The build Job, as a pure function of a `BuildRun` and what the controller resolved for it, and
//! in [`buildkitd`] the daemon it builds on.
//!
//! One Job per `BuildRun`, `backoffLimit: 0`: retrying is the controller's decision. Its Pod:
//!
//! | container | image | does |
//! |---|---|---|
//! | `fetch` (init) | job image | `git fetch --depth=1 <url> <sha>` into `/workspace/src`, checks `HEAD`, then fetches the submodules at their recorded commits; for an archive source, `zeabur-builder-job fetch archive` |
//! | `plan` (init) | planner image | Railpack or the agent writes `/workspace/plan`; absent for `dockerfile`. When the controller hands it a saved plan (`JobInputs::saved_plan`), the job image's `zeabur-builder-job plan restore` unpacks it there instead |
//! | `build` | job image | `zeabur-builder-job build <file>`, a [`BuildPlan`] as JSON from the Pod's annotation; after a planner ran, it pushes `/workspace/plan` as the plan artifact |
//!
//! A Railpack build runs `buildctl` twice: Railpack's image, pushed by digest only, then a
//! Dockerfile build `FROM` it that makes it run as a non-root user and pushes the tags
//! ([`zeabur_builder_core::railpack`]). The first leaves an untagged manifest in the repository
//! for the registry's garbage collection.
//!
//! The job image is `moby/buildkit` (for `buildctl` and `git`) plus `zeabur-builder-job`, a small
//! static wrapper that runs the `buildctl` command it is given with `--progress rawjson`, prints
//! the progress for people, counts cached steps, reads the image digest from `buildctl`'s metadata
//! file and writes the termination message ([`zeabur_builder_core::result`]). The `buildctl`
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
//! privileges. Every container has the same memory request and limit ([`JobMemory`]).

pub mod buildkitd;

use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Capabilities, Container, DownwardAPIVolumeFile, DownwardAPIVolumeSource, EmptyDirVolumeSource,
    EnvVar, EnvVarSource, KeyToPath, ObjectFieldSelector, PodSecurityContext, PodSpec,
    PodTemplateSpec, ProjectedVolumeSource, ResourceRequirements, SeccompProfile,
    SecretKeySelector, SecretVolumeSource, SecurityContext, ServiceAccountTokenProjection,
    Toleration, Volume, VolumeMount, VolumeProjection,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use zeabur_builder_api::build::{Build, EnvVar as BuildEnvVar, SourceKind};
use zeabur_builder_api::build_run::BuildRun;
use zeabur_builder_api::cluster_builder::{
    AgentPlanner, AnthropicFederation, AzureFederation, ClusterBuilderSpec,
    Frontend as FrontendKind, ModelAuth, OpenAiFederation, Planner, PlannerSpec,
};
use zeabur_builder_api::{annotations, labels};
use zeabur_builder_core::build_plan::{self, BuildPlan, Image, NonRoot, Output, PREVIOUS_DIGEST};
use zeabur_builder_core::repo::{RepoKey, RepoUrlError};
use zeabur_builder_core::result::is_digest;
use zeabur_builder_core::{archive, is_commit_sha, plan};

/// The user the Pod runs as; `moby/buildkit` would otherwise run as root.
pub const RUN_AS: i64 = 1000;

const WORKSPACE: &str = "/workspace";
const SRC: &str = "/workspace/src";
const PLAN: &str = "/workspace/plan";
const OUT: &str = "/workspace/out";
/// Where `zeabur-builder-job build` writes the Railpack plan it rewrites and the second
/// build's Dockerfile, so the plan folder keeps what the planner wrote.
const NON_ROOT: &str = "/workspace/out/non-root";
const GIT_CREDENTIALS: &str = "/etc/zeabur-builder/git";
const BUILDKIT_TLS: &str = "/etc/zeabur-builder/buildkit";
const REGISTRY_AUTH: &str = "/etc/zeabur-builder/registry";
/// The build plan, from the Pod's [`annotations::BUILD_PLAN`].
const BUILD_PLAN_DIR: &str = "/etc/zeabur-builder/build-plan";
const BUILD_PLAN_FILE: &str = "plan.json";
const MODEL_TOKEN_DIR: &str = "/var/run/secrets/zeabur.com/model";
const MODEL_TOKEN: &str = "/var/run/secrets/zeabur.com/model/token";
/// How long a projected model token lasts; the kubelet renews it before it expires.
const MODEL_TOKEN_SECONDS: i64 = 3600;
/// The registry's CA, PEM, in the `fetch` container of an archive source.
pub const REGISTRY_CA_ENV: &str = "ZB_REGISTRY_CA";
/// Prefix of the build container's variables that carry `Build.spec.env`, so a variable named
/// `PATH` or `BUILDKIT_HOST` cannot change how `buildctl` runs.
const SECRET_ENV_PREFIX: &str = "ZB_SECRET_";

/// The nodes build Jobs and buildkitd Pods run on. Empty: wherever the scheduler puts them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Placement {
    /// Labels the node must have, such as `builder.zeabur.com/builds: "true"`.
    pub node_selector: BTreeMap<String, String>,
    /// Taints the Pods tolerate, such as the build node's.
    pub tolerations: Vec<Toleration>,
}

impl Placement {
    fn node_selector(&self) -> Option<BTreeMap<String, String>> {
        (!self.node_selector.is_empty()).then(|| self.node_selector.clone())
    }

    fn tolerations(&self) -> Option<Vec<Toleration>> {
        (!self.tolerations.is_empty()).then(|| self.tolerations.clone())
    }
}

/// The memory of each container of the build Job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobMemory {
    /// `requests.memory`, or `limit` when that is lower.
    pub request: String,
    /// `limits.memory`.
    pub limit: String,
}

impl JobMemory {
    fn requirements(&self) -> ResourceRequirements {
        let request = buildkitd::within(Some(&self.request), Some(&self.limit))
            .unwrap_or_else(|| self.request.clone());
        let memory = |quantity: String| BTreeMap::from([("memory".to_owned(), Quantity(quantity))]);
        ResourceRequirements {
            requests: Some(memory(request)),
            limits: Some(memory(self.limit.clone())),
            ..ResourceRequirements::default()
        }
    }
}

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
    /// The nodes the Job runs on.
    pub placement: &'a Placement,
    /// The memory of each container.
    pub memory: &'a JobMemory,
    /// The digest of the saved plan in `image` the Job restores instead of running the planner.
    /// The controller passes it when `Build.status.plan` names this builder and `image`.
    pub saved_plan: Option<&'a str>,
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
    /// The repository URL is not one `fetch` may use.
    #[error(transparent)]
    Url(#[from] RepoUrlError),
}

/// What the builder and the saved plan make of one Job, decided once.
struct Resolved<'a> {
    /// The `plan` container.
    plan: Plan<'a>,
    /// How the `build` container reads the plan.
    frontend: Frontend<'a>,
}

/// The `plan` container.
enum Plan<'a> {
    /// None: the repository has its own Dockerfile.
    None,
    /// `zeabur-builder-job plan restore` unpacks the saved plan with this digest instead of
    /// planning.
    Restore(&'a str),
    /// `railpack prepare`.
    Railpack(&'a PlannerSpec),
    /// `zeabur-builder-agent`.
    Agent(&'a AgentPlanner),
}

/// How the `build` container reads the plan.
enum Frontend<'a> {
    /// A Dockerfile frontend, `dockerfile.v0` or an image, reading `filename` in `dir`.
    Dockerfile {
        image: Option<&'a str>,
        dir: String,
        filename: String,
    },
    /// Railpack's frontend image. It writes an image that runs as root, so a second build
    /// makes it run as a non-root user ([`non_root_build`]).
    Railpack { image: &'a str },
}

/// Resolves the builder for this run; `context` is the build context in the Pod.
fn resolve<'a>(inputs: &JobInputs<'a>, context: &str) -> Resolved<'a> {
    let builder = inputs.builder;
    let plan = match (&builder.planner, inputs.saved_plan.filter(|d| is_digest(d))) {
        (Planner::Dockerfile, _) => Plan::None,
        (Planner::Railpack { .. } | Planner::Agent { .. }, Some(digest)) => Plan::Restore(digest),
        (Planner::Railpack { railpack }, None) => Plan::Railpack(railpack),
        (Planner::Agent { agent }, None) => Plan::Agent(agent),
    };
    let image = match &builder.frontend.kind {
        FrontendKind::Dockerfile => None,
        FrontendKind::Gateway { source } => Some(source.as_str()),
    };
    let frontend = match (&builder.planner, image) {
        (Planner::Railpack { .. }, Some(image)) => Frontend::Railpack { image },
        (Planner::Dockerfile, image) => {
            let path = inputs
                .build
                .spec
                .source
                .dockerfile
                .as_deref()
                .unwrap_or("Dockerfile");
            let (dir, filename) = path
                .rsplit_once('/')
                .map_or((None, path), |(d, f)| (Some(d), f));
            Frontend::Dockerfile {
                image,
                dir: dir.map_or_else(|| context.to_owned(), |d| format!("{context}/{d}")),
                filename: filename.to_owned(),
            }
        }
        (Planner::Railpack { .. } | Planner::Agent { .. }, image) => Frontend::Dockerfile {
            image,
            dir: PLAN.to_owned(),
            filename: "Dockerfile".to_owned(),
        },
    };
    Resolved { plan, frontend }
}

/// Builds the Job.
///
/// # Errors
///
/// When the `BuildRun` lacks metadata or has a name too long for a Job, the revision does not fit
/// the source, or the repository URL is not one `fetch` may use.
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
    let repo = match &source.kind {
        SourceKind::Git { git } if is_commit_sha(revision) => Some(RepoKey::parse(&git.url)?),
        SourceKind::Archive { .. } if is_digest(revision) => None,
        SourceKind::Git { .. } | SourceKind::Archive { .. } => {
            return Err(JobError::Revision(revision.clone()));
        }
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
    let context = context_path(inputs);
    let resolved = resolve(inputs, &context);
    let annotations = job_annotations(inputs, &resolved, revision);

    let fetch = match &source.kind {
        SourceKind::Git { git } => fetch_container(inputs, &git.url),
        SourceKind::Archive { .. } => archive_container(inputs, namespace),
    };
    let mut init_containers = vec![fetch];
    init_containers.extend(plan_container(inputs, &resolved, &context));
    let resources = inputs.memory.requirements();
    for container in &mut init_containers {
        container.resources = Some(resources.clone());
    }
    let plan = build_plan(inputs, &resolved, &context, repo.as_ref());
    let mut pod_annotations = annotations.clone();
    pod_annotations.insert(
        annotations::BUILD_PLAN.to_owned(),
        serde_json::to_string(&plan).unwrap_or_default(),
    );
    let mut build = build_container(inputs);
    build.resources = Some(resources);
    let token = model_token(&resolved.plan);

    let pod = PodSpec {
        restart_policy: Some("Never".to_owned()),
        automount_service_account_token: Some(false),
        service_account_name: token.as_ref().map(|token| token.service_account.to_owned()),
        enable_service_links: Some(false),
        security_context: Some(pod_security()),
        init_containers: Some(init_containers),
        containers: vec![build],
        volumes: Some(volumes(inputs, token.as_ref())),
        node_selector: inputs.placement.node_selector(),
        tolerations: inputs.placement.tolerations(),
        ..PodSpec::default()
    };

    Ok(Job {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            labels: Some(labels.clone()),
            annotations: Some(annotations),
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
                    annotations: Some(pod_annotations),
                    ..ObjectMeta::default()
                }),
                spec: Some(pod),
            },
            ..JobSpec::default()
        }),
        status: None,
    })
}

/// Fetches exactly one commit and its submodules; `$1` is the URL and `$2` the SHA, both checked
/// before the Job is built. `git fetch <sha>` needs the server to allow it, which GitHub,
/// GitLab, Gitea and `git daemon` (protocol v2) do; the checkout is then verified against the SHA.
///
/// Submodules are fetched shallow at the commits the tree records. A relative submodule URL
/// resolves against `origin`. The basic-auth credentials go only to the host of `$1`
/// ([git-credential], "CREDENTIAL CONTEXTS"). If a submodule cannot be fetched, Git names its
/// path on stderr and the script fails.
///
/// [git-credential]: https://git-scm.com/docs/gitcredentials#_credential_contexts
const FETCH_SCRIPT: &str = r#"set -eu
start=$(date +%s)
mkdir -p /workspace/plan /workspace/out
if [ -f /etc/zeabur-builder/git/ssh-privatekey ]; then
  install -m 0600 /etc/zeabur-builder/git/ssh-privatekey /tmp/id
  export GIT_SSH_COMMAND="ssh -i /tmp/id -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=/etc/zeabur-builder/git/known_hosts"
fi
if [ -f /etc/zeabur-builder/git/password ]; then
  authority=${1#*://}
  authority=${authority%%/*}
  git config --global "credential.${1%%://*}://${authority##*@}.helper" '!f() { echo username=$(cat /etc/zeabur-builder/git/username); echo password=$(cat /etc/zeabur-builder/git/password); }; f'
fi
git init -q /workspace/src
git -C /workspace/src remote add origin -- "$1"
git -C /workspace/src fetch -q --depth=1 --no-tags origin "$2"
git -C /workspace/src checkout -q --detach FETCH_HEAD
test "$(git -C /workspace/src rev-parse HEAD)" = "$2"
git -C /workspace/src submodule update -q --init --recursive --depth=1
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
    let mut command = vec![
        "zeabur-builder-job".to_owned(),
        "fetch".to_owned(),
        "archive".to_owned(),
    ];
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

/// Runs the agent planner, copies its report to the termination message for
/// `BuildRun.status.plan`, and records how long it took. The agent exits 2 when it stops
/// without a Dockerfile.
const AGENT_PLAN_SCRIPT: &str = r#"set -u
start=$(date +%s)
"$@"
code=$?
# BusyBox cp does not overwrite /dev/termination-log.
cat /workspace/plan/info.json > /dev/termination-log
echo "{\"plan\":$(( $(date +%s) - start ))}" > /workspace/out/plan.json
exit "$code"
"#;

/// The Job's and Pod's annotations: the commit, and what the controller reads back when the run
/// finishes (the repository and any saved plan the Job restores).
fn job_annotations(
    inputs: &JobInputs<'_>,
    resolved: &Resolved<'_>,
    revision: &str,
) -> BTreeMap<String, String> {
    let mut found = BTreeMap::from([
        (annotations::REVISION.to_owned(), revision.to_owned()),
        (annotations::IMAGE.to_owned(), inputs.image.to_owned()),
    ]);
    if let Plan::Restore(digest) = resolved.plan {
        found.insert(annotations::SAVED_PLAN.to_owned(), digest.to_owned());
    }
    found
}

fn plan_container(
    inputs: &JobInputs<'_>,
    resolved: &Resolved<'_>,
    context: &str,
) -> Option<Container> {
    match resolved.plan {
        Plan::None => None,
        Plan::Restore(digest) => Some(restore_container(inputs, digest)),
        Plan::Railpack(planner) => Some(railpack_container(inputs, planner, context)),
        Plan::Agent(agent) => Some(agent_container(inputs, agent, context)),
    }
}

/// `zeabur-builder-job plan restore`, which unpacks the saved plan into `/workspace/plan`, with
/// the run's push credentials. Only the digest comes from the saved plan; the repository is the
/// one the Job pushes to, so a run never reads another repository with them.
fn restore_container(inputs: &JobInputs<'_>, digest: &str) -> Container {
    let mut command = vec![
        "zeabur-builder-job".to_owned(),
        "plan".to_owned(),
        "restore".to_owned(),
    ];
    if inputs.registry_http {
        command.push("--plain-http".to_owned());
    }
    command.extend([literal(inputs.image), digest.to_owned(), PLAN.to_owned()]);
    let mut env_vars = vec![env("HOME", "/tmp"), env("DOCKER_CONFIG", REGISTRY_AUTH)];
    if let Some(ca) = inputs.registry_ca {
        env_vars.push(env(REGISTRY_CA_ENV, &literal(ca)));
    }
    let mut mounts = workspace_mounts();
    mounts.push(mount("registry-auth", REGISTRY_AUTH));
    Container {
        name: "plan".to_owned(),
        image: Some(inputs.job_image.to_owned()),
        command: Some(command),
        env: Some(env_vars),
        volume_mounts: Some(mounts),
        termination_message_policy: Some("FallbackToLogsOnError".to_owned()),
        ..hardened()
    }
}

/// `railpack prepare`, which writes the plan and its info into `/workspace/plan`.
fn railpack_container(inputs: &JobInputs<'_>, planner: &PlannerSpec, context: &str) -> Container {
    let mounts = workspace_mounts();
    let mut env_vars = vec![env("HOME", "/tmp")];
    let mut args = vec![
        "railpack".to_owned(),
        "prepare".to_owned(),
        literal(context),
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
    Container {
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
    }
}

/// `zeabur-builder-agent`, which trial-builds on the namespace's buildkitd and writes the
/// Dockerfile into `/workspace/plan`, with the model API key from the namespace's Secret in
/// `ZB_MODEL_API_KEY`.
fn agent_container(inputs: &JobInputs<'_>, agent: &AgentPlanner, context: &str) -> Container {
    let model = &agent.model;
    let mut args = vec![
        "zeabur-builder-agent".to_owned(),
        "--context".to_owned(),
        literal(context),
        "--buildkit".to_owned(),
        inputs.buildkitd_addr.to_owned(),
        "--tlsdir".to_owned(),
        BUILDKIT_TLS.to_owned(),
        "--out".to_owned(),
        PLAN.to_owned(),
        "--provider".to_owned(),
        literal(&model.provider),
        "--model".to_owned(),
        literal(&model.model),
    ];
    if let Some(base_url) = &model.base_url {
        args.extend(["--base-url".to_owned(), literal(base_url)]);
    }
    // Trial builds get the build variables as the final build does; the agent keeps their
    // values out of what the model reads.
    let variables = &inputs.build.spec.env;
    args.extend(
        variables
            .iter()
            .flat_map(|var| ["--secret".to_owned(), var.name.clone()]),
    );
    let mut mounts = workspace_mounts();
    mounts.push(mount("buildkit-client-tls", BUILDKIT_TLS));
    let sign_in = sign_in(&model.auth);
    if sign_in.token.is_some() {
        mounts.push(mount("model-token", MODEL_TOKEN_DIR));
    }
    Container {
        name: "plan".to_owned(),
        image: Some(agent.planner.image.clone()),
        command: Some(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            AGENT_PLAN_SCRIPT.to_owned(),
            "plan".to_owned(),
        ]),
        args: Some(args),
        env: Some(
            [
                vec![env("HOME", "/tmp")],
                sign_in.env,
                variables.iter().map(secret_env).collect(),
            ]
            .concat(),
        ),
        volume_mounts: Some(mounts),
        termination_message_policy: Some("FallbackToLogsOnError".to_owned()),
        ..hardened()
    }
}

/// How an agent run signs in to the model API: its variables, and the projected token it
/// exchanges, if any.
struct SignIn<'a> {
    env: Vec<EnvVar>,
    token: Option<ModelToken<'a>>,
}

/// A service account token for a model provider's audience.
struct ModelToken<'a> {
    service_account: &'a str,
    audience: &'static str,
}

/// The variables each provider's SDKs read, so the agent needs no flags of its own.
fn sign_in(auth: &ModelAuth) -> SignIn<'_> {
    match auth {
        ModelAuth::ApiKey { api_key } => SignIn {
            env: vec![EnvVar {
                name: "ZB_MODEL_API_KEY".to_owned(),
                value: None,
                value_from: Some(EnvVarSource {
                    secret_key_ref: Some(SecretKeySelector {
                        name: api_key.name.clone(),
                        key: api_key.key.clone(),
                        optional: Some(false),
                    }),
                    ..EnvVarSource::default()
                }),
            }],
            token: None,
        },
        ModelAuth::AnthropicFederation {
            anthropic_federation: federation,
        } => {
            let mut vars = vec![
                env(
                    "ANTHROPIC_FEDERATION_RULE_ID",
                    &federation.federation_rule_id,
                ),
                env("ANTHROPIC_ORGANIZATION_ID", &federation.organization_id),
                env("ANTHROPIC_IDENTITY_TOKEN_FILE", MODEL_TOKEN),
            ];
            if let Some(workspace) = &federation.workspace_id {
                vars.push(env("ANTHROPIC_WORKSPACE_ID", workspace));
            }
            if let Some(account) = &federation.service_account_id {
                vars.push(env("ANTHROPIC_SERVICE_ACCOUNT_ID", account));
            }
            SignIn {
                env: vars,
                token: Some(ModelToken {
                    service_account: &federation.service_account_name,
                    audience: AnthropicFederation::AUDIENCE,
                }),
            }
        }
        ModelAuth::OpenaiFederation {
            openai_federation: federation,
        } => SignIn {
            env: vec![
                env(
                    "OPENAI_IDENTITY_PROVIDER_ID",
                    &federation.identity_provider_id,
                ),
                env("OPENAI_SERVICE_ACCOUNT_ID", &federation.service_account_id),
                env("OPENAI_IDENTITY_TOKEN_FILE", MODEL_TOKEN),
            ],
            token: Some(ModelToken {
                service_account: &federation.service_account_name,
                audience: OpenAiFederation::AUDIENCE,
            }),
        },
        ModelAuth::AzureFederation {
            azure_federation: federation,
        } => SignIn {
            env: vec![
                env("AZURE_TENANT_ID", &federation.tenant_id),
                env("AZURE_CLIENT_ID", &federation.client_id),
                env("AZURE_FEDERATED_TOKEN_FILE", MODEL_TOKEN),
            ],
            token: Some(ModelToken {
                service_account: &federation.service_account_name,
                audience: AzureFederation::AUDIENCE,
            }),
        },
    }
}

/// The projected token an agent run exchanges, if it signs in with federation.
fn model_token<'a>(plan: &Plan<'a>) -> Option<ModelToken<'a>> {
    match plan {
        Plan::Agent(agent) => sign_in(&agent.model.auth).token,
        Plan::None | Plan::Restore(_) | Plan::Railpack(_) => None,
    }
}

fn token_volume(token: &ModelToken<'_>) -> Volume {
    Volume {
        name: "model-token".to_owned(),
        projected: Some(ProjectedVolumeSource {
            sources: Some(vec![VolumeProjection {
                service_account_token: Some(ServiceAccountTokenProjection {
                    audience: Some(token.audience.to_owned()),
                    expiration_seconds: Some(MODEL_TOKEN_SECONDS),
                    path: "token".to_owned(),
                }),
                ..VolumeProjection::default()
            }]),
            default_mode: Some(0o440),
        }),
        ..Volume::default()
    }
}

fn build_container(inputs: &JobInputs<'_>) -> Container {
    let mut env_vars = vec![env("HOME", "/tmp"), env("DOCKER_CONFIG", REGISTRY_AUTH)];
    if let Some(ca) = inputs.registry_ca {
        env_vars.push(env(REGISTRY_CA_ENV, &literal(ca)));
    }
    env_vars.extend(inputs.build.spec.env.iter().map(secret_env));
    let mut mounts = workspace_mounts();
    mounts.push(mount("buildkit-client-tls", BUILDKIT_TLS));
    mounts.push(mount("registry-auth", REGISTRY_AUTH));
    mounts.push(mount("build-plan", BUILD_PLAN_DIR));
    Container {
        name: "build".to_owned(),
        image: Some(inputs.job_image.to_owned()),
        command: Some(vec![
            "zeabur-builder-job".to_owned(),
            "build".to_owned(),
            format!("{BUILD_PLAN_DIR}/{BUILD_PLAN_FILE}"),
        ]),
        env: Some(env_vars),
        volume_mounts: Some(mounts),
        termination_message_policy: Some("File".to_owned()),
        termination_message_path: Some("/dev/termination-log".to_owned()),
        ..hardened()
    }
}

/// What `zeabur-builder-job build` runs.
fn build_plan(
    inputs: &JobInputs<'_>,
    resolved: &Resolved<'_>,
    context: &str,
    repo: Option<&RepoKey>,
) -> BuildPlan {
    let non_root = matches!(resolved.frontend, Frontend::Railpack { .. });
    let mut command = buildctl(inputs);
    command.extend(["--local".to_owned(), format!("context={context}")]);
    let secrets_hash_env = frontend_args(inputs, &resolved.frontend, &mut command);
    command.extend(inputs.build.spec.env.iter().flat_map(|var| {
        [
            "--secret".to_owned(),
            format!("id={},env={SECRET_ENV_PREFIX}{}", var.name, var.name),
        ]
    }));
    let builds = if non_root {
        vec![
            build_plan::Build {
                command,
                metadata_file: format!("{OUT}/railpack-metadata.json"),
                output: output(inputs, repo, Vec::new()),
            },
            non_root_build(inputs, repo),
        ]
    } else {
        vec![build_plan::Build {
            command,
            metadata_file: format!("{OUT}/metadata.json"),
            output: output(inputs, repo, inputs.tags.to_vec()),
        }]
    };
    BuildPlan {
        plain_http: inputs.registry_http,
        secrets_hash_env,
        non_root: non_root.then(|| NonRoot {
            plan: PLAN.to_owned(),
            out: NON_ROOT.to_owned(),
        }),
        save_plan: saved_plan(inputs, &resolved.plan),
        builds,
    }
}

/// `buildctl … build` up to the frontend.
fn buildctl(inputs: &JobInputs<'_>) -> Vec<String> {
    vec![
        "buildctl".to_owned(),
        "--addr".to_owned(),
        inputs.buildkitd_addr.to_owned(),
        "--tlsdir".to_owned(),
        BUILDKIT_TLS.to_owned(),
        "build".to_owned(),
        "--progress".to_owned(),
        "rawjson".to_owned(),
    ]
}

/// The second build: the Railpack image, pushed by digest only, made to run as
/// [`IMAGE_UID`](zeabur_builder_core::railpack::IMAGE_UID) by the Dockerfile `zeabur-builder-job`
/// writes for [`NonRoot`], and pushed with the tags.
fn non_root_build(inputs: &JobInputs<'_>, repo: Option<&RepoKey>) -> build_plan::Build {
    let dir = NON_ROOT;
    let mut command = buildctl(inputs);
    command.extend([
        "--frontend".to_owned(),
        "dockerfile.v0".to_owned(),
        "--local".to_owned(),
        format!("context={dir}"),
        "--local".to_owned(),
        format!("dockerfile={dir}"),
        "--opt".to_owned(),
        format!(
            "context:app=docker-image://{}@{PREVIOUS_DIGEST}",
            inputs.image
        ),
    ]);
    build_plan::Build {
        command,
        metadata_file: format!("{OUT}/metadata.json"),
        output: output(inputs, repo, inputs.tags.to_vec()),
    }
}

/// Where a builder with a planner saves the plan it wrote ([`zeabur_builder_core::plan`]):
/// `<image>:zeabur-plan-<Build uid>`. None for `dockerfile` and for a run that reused a plan.
fn saved_plan(inputs: &JobInputs<'_>, plan: &Plan<'_>) -> Option<Image> {
    match plan {
        Plan::None | Plan::Restore(_) => return None,
        Plan::Railpack(_) | Plan::Agent(_) => {}
    }
    let uid = inputs.build.metadata.uid.as_deref()?;
    Some(Image {
        repository: inputs.image.to_owned(),
        tag: plan::saved_tag(uid),
    })
}

/// `--frontend`, the plan's location and the frontend options. Returns the variables whose
/// values Railpack's cache depends on.
fn frontend_args(
    inputs: &JobInputs<'_>,
    frontend: &Frontend<'_>,
    buildctl: &mut Vec<String>,
) -> Vec<String> {
    let mut secrets_hash_env = Vec::new();
    match frontend {
        Frontend::Dockerfile {
            image: None,
            dir,
            filename,
        } => {
            buildctl.extend(["--frontend".to_owned(), "dockerfile.v0".to_owned()]);
            buildctl.extend(["--local".to_owned(), format!("dockerfile={dir}")]);
            buildctl.extend(["--opt".to_owned(), format!("filename={filename}")]);
        }
        Frontend::Dockerfile {
            image: Some(image),
            dir,
            filename,
        } => {
            buildctl.extend(["--frontend".to_owned(), "gateway.v0".to_owned()]);
            buildctl.extend(["--opt".to_owned(), format!("source={image}")]);
            buildctl.extend(["--local".to_owned(), format!("dockerfile={dir}")]);
            buildctl.extend(["--opt".to_owned(), format!("filename={filename}")]);
        }
        Frontend::Railpack { image } => {
            buildctl.extend(["--frontend".to_owned(), "gateway.v0".to_owned()]);
            buildctl.extend(["--opt".to_owned(), format!("source={image}")]);
            buildctl.extend(["--local".to_owned(), format!("dockerfile={NON_ROOT}")]);
            // Railpack reads build arguments only under `build-arg:`; the wrapper hashes the
            // secret values into `build-arg:secrets-hash` so a changed value invalidates the
            // cache without the value entering the cache key or image metadata.
            secrets_hash_env.extend(
                inputs
                    .build
                    .spec
                    .env
                    .iter()
                    .map(|var| format!("{SECRET_ENV_PREFIX}{}", var.name)),
            );
            buildctl.extend([
                "--opt".to_owned(),
                format!("build-arg:cache-key={}", cache_key(inputs)),
            ]);
        }
    }
    buildctl.extend(
        inputs
            .builder
            .frontend
            .opts
            .iter()
            .flat_map(|(key, value)| ["--opt".to_owned(), format!("{key}={value}")]),
    );
    secrets_hash_env
}

/// The image a build pushes, with `tags`; with none, it is pushed by digest only.
fn output(inputs: &JobInputs<'_>, repo: Option<&RepoKey>, tags: Vec<String>) -> Output {
    // The source annotation is the repository key, never the URL, which may carry a token
    // (`https://token@host/…`).
    let mut annotations = BTreeMap::from([(
        "org.opencontainers.image.revision".to_owned(),
        inputs.build_run.spec.revision.clone(),
    )]);
    if let Some(repo) = repo {
        annotations.insert(
            "org.opencontainers.image.source".to_owned(),
            format!("https://{repo}"),
        );
    }
    Output {
        repository: inputs.image.to_owned(),
        tags,
        annotations,
    }
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

/// Doubles `$` so the kubelet passes the value through unexpanded.
fn literal(value: &str) -> String {
    value.replace('$', "$$")
}

fn context_path(inputs: &JobInputs<'_>) -> String {
    match inputs
        .build
        .spec
        .source
        .context_dir
        .as_deref()
        .map(|d| d.trim_matches('/'))
    {
        None | Some("" | ".") => SRC.to_owned(),
        Some(dir) => format!("{SRC}/{dir}"),
    }
}

/// The pod's volumes. Its only service account token is `token`, for a model provider's
/// audience.
fn volumes(inputs: &JobInputs<'_>, token: Option<&ModelToken<'_>>) -> Vec<Volume> {
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
    volumes.push(build_plan_volume());
    volumes.extend(token.map(token_volume));
    volumes
}

/// The Pod's [`annotations::BUILD_PLAN`] as a file.
fn build_plan_volume() -> Volume {
    Volume {
        name: "build-plan".to_owned(),
        downward_api: Some(DownwardAPIVolumeSource {
            items: Some(vec![DownwardAPIVolumeFile {
                path: BUILD_PLAN_FILE.to_owned(),
                field_ref: Some(ObjectFieldSelector {
                    field_path: format!("metadata.annotations['{}']", annotations::BUILD_PLAN),
                    ..ObjectFieldSelector::default()
                }),
                ..DownwardAPIVolumeFile::default()
            }]),
            ..DownwardAPIVolumeSource::default()
        }),
        ..Volume::default()
    }
}

fn pod_security() -> PodSecurityContext {
    PodSecurityContext {
        run_as_non_root: Some(true),
        run_as_user: Some(RUN_AS),
        run_as_group: Some(RUN_AS),
        fs_group: Some(RUN_AS),
        seccomp_profile: Some(SeccompProfile {
            type_: "RuntimeDefault".to_owned(),
            ..SeccompProfile::default()
        }),
        ..PodSecurityContext::default()
    }
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
