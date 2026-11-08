//! Remote mode: a `BuildRun` builds on a runner cluster and its image is copied back.
//!
//! The controller talks to the runner's API server as `remote:<installation>`, with its projected
//! service account token, and writes only in `zbr-<installation>` there:
//!
//! 1. A suspended `Build` mirrors the local one's Git source; its name carries a hash of the local
//!    namespace and name. A `BuildRun` named after the local run's UID pins the commit.
//! 2. The local run follows the remote run's phase. A failed or cancelled remote run finishes it.
//! 3. A succeeded remote run gets a pull grant (`builder.zeabur.com/pull-token-request`). The
//!    controller puts the pull token and its own push token in `<run>-import` and starts the import
//!    Job, named like the run, which copies the image by digest into the default registry.
//! 4. When the Job succeeds the run succeeds with the remote digest, and the remote run is deleted.
//!    The runner deletes the remote `Build` after its retention.
//!
//! Every step reads the remote objects again, so a restart or a repeated reconcile continues where
//! the last one stopped.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use aws_lc_rs::digest::{SHA256, digest};
use jiff::Timestamp;
use k8s_openapi::ByteString;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, DeleteParams, ObjectMeta, Patch, PatchParams};
use kube::{Client, Resource as _, ResourceExt as _};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;
use serde_json::json;
use zeabur_builder_api::build::Build;
use zeabur_builder_api::build_run::{BuildRun, BuildRunSpec, BuildRunStatus, Phase, Reason};
use zeabur_builder_api::{annotations, labels, remote};
use zeabur_builder_jobspec::ImportInputs;

use super::{Error, apply_status, create_if_missing, finish, not_found, now, registry_auth};
use crate::{decide, registry};

/// How often a run that waits on the runner is looked at again.
const POLL: Duration = Duration::from_secs(10);
/// How long before a pull grant expires the controller asks for a new one.
const GRANT_MARGIN: Duration = Duration::from_mins(5);
const MANAGER: &str = "zeabur-builder-remote";
/// The longest part of a local `Build` name kept in the remote name.
const NAME_PREFIX: usize = 40;

/// Where remote builds go, from the command line.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// The runner's API server, `https://...`.
    pub(crate) server: String,
    /// The runner API server's CA, PEM, unless a public CA signed it.
    pub(crate) ca: Option<String>,
    /// The projected service account token, audience `zeabur-builder-runner`.
    pub(crate) token_file: PathBuf,
    /// This installation's id on the runner.
    pub(crate) installation: String,
    /// The runner's registry host, as the import Job reaches it.
    pub(crate) registry: String,
}

/// A client of the runner's API server. kube re-reads the token file as the kubelet rotates it.
pub(crate) fn client(settings: &Settings) -> Result<Client, String> {
    let url = settings
        .server
        .parse()
        .map_err(|e| format!("--remote-server: {e}"))?;
    let mut config = kube::Config::new(url);
    config.default_namespace = remote::namespace(&settings.installation);
    if let Some(ca) = &settings.ca {
        let mut roots = Vec::new();
        for cert in CertificateDer::pem_slice_iter(ca.as_bytes()) {
            roots.push(cert.map_err(|e| format!("--remote-ca-file: {e}"))?.to_vec());
        }
        config.root_cert = Some(roots);
    }
    config.auth_info.token_file = Some(settings.token_file.display().to_string());
    Client::try_from(config).map_err(|e| format!("cannot build the runner client: {e}"))
}

/// The remote `Build`'s name: the local name, shortened, and a hash of namespace and name.
pub(crate) fn build_name(namespace: &str, name: &str) -> String {
    let hash = digest(&SHA256, format!("{namespace}/{name}").as_bytes());
    let first = hash
        .as_ref()
        .first_chunk::<4>()
        .copied()
        .unwrap_or_default();
    let short = format!("{:08x}", u32::from_be_bytes(first));
    let prefix: String = name.chars().take(NAME_PREFIX).collect();
    format!("{}-{short}", prefix.trim_end_matches('-'))
}

/// The remote `BuildRun`'s name.
pub(crate) fn run_name(run: &BuildRun) -> String {
    format!("r-{}", run.metadata.uid.as_deref().unwrap_or_default())
}

/// Why a `Build` cannot build remotely: the runner fetches public https Git only and reads no
/// Secret of this cluster.
pub(crate) fn unsupported(build: &Build) -> Option<&'static str> {
    let source = &build.spec.source;
    let Some(git) = &source.git else {
        return Some("remote builds need a Git source");
    };
    if !git.url.starts_with("https://")
        || source.credentials_ref.is_some()
        || source.github_app.is_some()
    {
        return Some("remote builds fetch public https Git repositories only");
    }
    if build.spec.env.iter().any(|e| e.value_from.is_some()) {
        return Some("remote builds take no environment variables from Secrets");
    }
    if build
        .spec
        .output
        .as_ref()
        .is_some_and(|o| o.image.is_some())
    {
        return Some("remote builds push to the default registry only");
    }
    None
}

/// The remote `Build`: suspended, the local source and builder, plain environment variables.
fn remote_build(build: &Build, name: &str) -> serde_json::Value {
    let mut env = Vec::new();
    for var in &build.spec.env {
        env.push(json!({"name": var.name, "value": var.value}));
    }
    json!({
        "apiVersion": Build::api_version(&()),
        "kind": Build::kind(&()),
        "metadata": {
            "name": name,
            "labels": {labels::MANAGED_BY: labels::MANAGER},
        },
        "spec": {
            "suspend": true,
            "builderRef": build.spec.builder_ref,
            "source": {"git": build.spec.source.git},
            "env": env,
        },
    })
}

/// Whether the grant answers `request` and lasts long enough for an import.
fn grant_usable(run: &BuildRun, request: &str, at: Timestamp) -> bool {
    let Some(grant) = run.status.as_ref().and_then(|s| s.pull_token.as_ref()) else {
        return false;
    };
    let Ok(expires) = grant.expires_at.parse::<Timestamp>() else {
        return false;
    };
    grant.request == request
        && expires.duration_since(at).unsigned_abs() > GRANT_MARGIN
        && expires > at
}

/// The remote run's phase, reason and message, as a local status.
fn mirrored(local: &BuildRunStatus, remote: &BuildRunStatus, server: &str) -> BuildRunStatus {
    BuildRunStatus {
        phase: remote.phase.or(Some(Phase::Pending)),
        reason: remote.reason,
        message: Some(remote.message.clone().map_or_else(
            || format!("building on {server}"),
            |m| format!("{server}: {m}"),
        )),
        ..local.clone()
    }
}

type Action = kube::runtime::controller::Action;

fn failed(status: &BuildRunStatus, reason: Reason, message: String) -> BuildRunStatus {
    BuildRunStatus {
        phase: Some(Phase::Failed),
        reason: Some(reason),
        message: Some(message),
        ..status.clone()
    }
}

/// Follows a run that builds remotely.
pub(super) async fn reconcile(
    ctx: &super::Context,
    run: &BuildRun,
    status: BuildRunStatus,
) -> Result<Action, Error> {
    let Some(remote_ctx) = &ctx.remote else {
        return Ok(Action::await_change());
    };
    let namespace = run.namespace().unwrap_or_default();
    let Some(build) = Api::<Build>::namespaced(ctx.client.clone(), &namespace)
        .get_opt(&run.spec.build_ref)
        .await?
    else {
        let message = format!("no Build `{}`", run.spec.build_ref);
        return finish(ctx, run, failed(&status, Reason::InfraError, message)).await;
    };
    if let Some(message) = unsupported(&build) {
        return finish(
            ctx,
            run,
            failed(&status, Reason::SourceFailed, message.to_owned()),
        )
        .await;
    }
    if let Some(job) = Api::<Job>::namespaced(ctx.client.clone(), &namespace)
        .get_opt(&run.name_any())
        .await?
    {
        return imported(ctx, remote_ctx, run, status, &job).await;
    }
    let remote_run = ensure_remote(remote_ctx, &build, run).await?;
    let remote_status = remote_run.status.clone().unwrap_or_default();
    let server = &remote_ctx.settings.server;
    match remote_status.phase {
        Some(Phase::Succeeded) => {
            start_import(ctx, remote_ctx, run, status, &build, &remote_run).await
        }
        Some(Phase::Failed | Phase::Cancelled) => {
            let ended = BuildRunStatus {
                phase: remote_status.phase,
                reason: remote_status.reason,
                message: remote_status.message.map(|m| format!("{server}: {m}")),
                ..status
            };
            finish(ctx, run, ended).await
        }
        None | Some(Phase::Pending | Phase::Planning | Phase::Building) => {
            let next = mirrored(&status, &remote_status, server);
            if next != status {
                let runs: Api<BuildRun> = Api::namespaced(ctx.client.clone(), &namespace);
                apply_status(&runs, &run.name_any(), &next).await?;
            }
            Ok(Action::requeue(POLL))
        }
    }
}

/// The result of the import Job: the run succeeds and its remote run goes, or it fails.
async fn imported(
    ctx: &super::Context,
    remote_ctx: &super::Remote,
    run: &BuildRun,
    status: BuildRunStatus,
    job: &Job,
) -> Result<Action, Error> {
    let job_status = job.status.clone().unwrap_or_default();
    if job_status.succeeded.unwrap_or(0) > 0 {
        Api::<BuildRun>::default_namespaced(remote_ctx.client.clone())
            .delete(&run_name(run), &DeleteParams::background())
            .await
            .map(drop)
            .or_else(not_found)?;
        let done = BuildRunStatus {
            phase: Some(Phase::Succeeded),
            reason: None,
            message: Some(format!("built on {}", remote_ctx.settings.server)),
            ..status
        };
        return finish(ctx, run, done).await;
    }
    if job_status.failed.unwrap_or(0) > 0 {
        let message = "the import failed: see the Job's logs".to_owned();
        return finish(ctx, run, failed(&status, Reason::InfraError, message)).await;
    }
    Ok(Action::await_change())
}

/// Applies the remote `Build` and creates the remote run, and returns the run as it is.
async fn ensure_remote(
    remote_ctx: &super::Remote,
    build: &Build,
    run: &BuildRun,
) -> Result<BuildRun, Error> {
    let remote_name = build_name(&build.namespace().unwrap_or_default(), &build.name_any());
    let applied = Api::<Build>::default_namespaced(remote_ctx.client.clone())
        .patch(
            &remote_name,
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(remote_build(build, &remote_name)),
        )
        .await?;
    let owner = OwnerReference {
        api_version: Build::api_version(&()).into_owned(),
        kind: Build::kind(&()).into_owned(),
        name: remote_name.clone(),
        uid: applied.metadata.uid.unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    };
    let wanted = BuildRun {
        metadata: ObjectMeta {
            name: Some(run_name(run)),
            labels: Some(BTreeMap::from([(
                labels::BUILD.to_owned(),
                remote_name.clone(),
            )])),
            owner_references: Some(vec![owner]),
            ..ObjectMeta::default()
        },
        spec: BuildRunSpec {
            build_ref: remote_name,
            ..run.spec.clone()
        },
        status: None,
    };
    let runs: Api<BuildRun> = Api::default_namespaced(remote_ctx.client.clone());
    create_if_missing(&runs, &wanted).await?;
    Ok(runs.get(&run_name(run)).await?)
}

/// The pull token of a usable grant, after asking for one if needed.
async fn pull_token(
    remote_ctx: &super::Remote,
    run: &BuildRun,
    remote_run: &BuildRun,
) -> Result<Option<String>, Error> {
    let at = now();
    let request = run.metadata.uid.clone().unwrap_or_default();
    let current = remote_run
        .annotations()
        .get(annotations::PULL_TOKEN_REQUEST)
        .cloned();
    let runs: Api<BuildRun> = Api::default_namespaced(remote_ctx.client.clone());
    if !grant_usable(remote_run, current.as_deref().unwrap_or_default(), at) {
        let fresh = if current.as_deref() == Some(request.as_str()) {
            format!("{request}-{}", at.as_second())
        } else {
            request
        };
        if current.as_deref() != Some(fresh.as_str()) {
            let patch =
                json!({"metadata": {"annotations": {annotations::PULL_TOKEN_REQUEST: fresh}}});
            runs.patch(
                &run_name(run),
                &PatchParams::default(),
                &Patch::Merge(patch),
            )
            .await?;
        }
        return Ok(None);
    }
    let Some(grant) = remote_run
        .status
        .as_ref()
        .and_then(|s| s.pull_token.as_ref())
    else {
        return Ok(None);
    };
    Ok(Api::<Secret>::default_namespaced(remote_ctx.client.clone())
        .get_opt(&grant.secret_name)
        .await?
        .and_then(|s| s.data)
        .and_then(|mut d| d.remove("token"))
        .and_then(|t| String::from_utf8(t.0).ok()))
}

/// Starts the import Job of a succeeded remote run.
async fn start_import(
    ctx: &super::Context,
    remote_ctx: &super::Remote,
    run: &BuildRun,
    status: BuildRunStatus,
    build: &Build,
    remote_run: &BuildRun,
) -> Result<Action, Error> {
    let settings = &remote_ctx.settings;
    let namespace = run.namespace().unwrap_or_default();
    let name = run.name_any();
    let Some(digest) = remote_run
        .status
        .as_ref()
        .and_then(|s| s.image_digest.clone())
    else {
        let message = "the remote run recorded no digest".to_owned();
        return finish(ctx, run, failed(&status, Reason::InfraError, message)).await;
    };
    let Some(pull) = pull_token(remote_ctx, run, remote_run).await? else {
        return Ok(Action::requeue(Duration::from_secs(3)));
    };
    let tags = match decide::tags(build, run) {
        Ok(tags) => tags,
        Err(e) => {
            return finish(ctx, run, failed(&status, Reason::InfraError, e.to_string())).await;
        }
    };
    let repositories = match decide::owned(build, &ctx.settings.default_registry) {
        Ok(repositories) => repositories,
        Err(message) => {
            return finish(ctx, run, failed(&status, Reason::InfraError, message)).await;
        }
    };
    let base = match registry_auth(ctx, build, &repositories).await? {
        Ok(base) => base,
        Err(message) => {
            return super::pending(
                ctx,
                run,
                status,
                Reason::RegistryCredentialsMissing,
                message,
            )
            .await;
        }
    };
    let config =
        registry::with_token(&base.0, &settings.registry, &pull).map_err(Error::Invalid)?;
    let secret_name = format!("{name}-import");
    put_secret(ctx, run, &secret_name, config).await?;

    let remote_name = remote_run.spec.build_ref.clone();
    let from = format!(
        "{}/{}/{remote_name}@{digest}",
        settings.registry,
        remote::namespace(&settings.installation)
    );
    let to = decide::image(build, &ctx.settings.default_registry);
    let owner = run.controller_owner_ref(&());
    let job = zeabur_builder_jobspec::import_job(&ImportInputs {
        name: &name,
        namespace: &namespace,
        owner: owner.as_ref(),
        job_image: &ctx.settings.job_image,
        from: &from,
        to: &to,
        tags: &tags,
        registry_auth_secret: &secret_name,
        registry_ca: ctx.settings.registry_ca.as_deref(),
        registry_http: ctx.settings.registry_http,
        deadline_seconds: ctx.settings.job_deadline_seconds,
        ttl_seconds: ctx.settings.job_ttl_seconds,
    });
    create_if_missing(&Api::namespaced(ctx.client.clone(), &namespace), &job).await?;
    let importing = BuildRunStatus {
        phase: Some(Phase::Building),
        reason: None,
        message: Some(format!("importing {from}")),
        job_ref: Some(name.clone()),
        image_digest: Some(digest),
        tags,
        ..status
    };
    let runs: Api<BuildRun> = Api::namespaced(ctx.client.clone(), &namespace);
    apply_status(&runs, &name, &importing).await?;
    tracing::info!(buildrun = %name, %namespace, %from, "import started");
    Ok(Action::await_change())
}

/// Writes the import Job's credentials, replacing an older grant.
async fn put_secret(
    ctx: &super::Context,
    run: &BuildRun,
    name: &str,
    config: Vec<u8>,
) -> Result<(), Error> {
    let namespace = run.namespace().unwrap_or_default();
    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(namespace.clone()),
            labels: Some(BTreeMap::from([
                (labels::BUILD_RUN.to_owned(), run.name_any()),
                (labels::MANAGED_BY.to_owned(), labels::MANAGER.to_owned()),
            ])),
            owner_references: run.controller_owner_ref(&()).map(|o| vec![o]),
            ..ObjectMeta::default()
        },
        type_: Some("kubernetes.io/dockerconfigjson".to_owned()),
        data: Some(BTreeMap::from([(
            ".dockerconfigjson".to_owned(),
            ByteString(config),
        )])),
        ..Secret::default()
    };
    Api::<Secret>::namespaced(ctx.client.clone(), &namespace)
        .patch(
            name,
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(secret),
        )
        .await?;
    Ok(())
}

/// Deletes the remote run of a cancelled local run.
pub(super) async fn cancel(ctx: &super::Context, run: &BuildRun) -> Result<(), Error> {
    let Some(remote_ctx) = &ctx.remote else {
        return Ok(());
    };
    Api::<BuildRun>::default_namespaced(remote_ctx.client.clone())
        .delete(&run_name(run), &DeleteParams::background())
        .await
        .map(drop)
        .or_else(not_found)
}

#[cfg(test)]
mod tests {
    use zeabur_builder_api::build_run::PullToken;

    use super::*;

    fn build(source: &serde_json::Value) -> Build {
        serde_json::from_value(json!({
            "apiVersion": "builder.zeabur.com/v1alpha1",
            "kind": "Build",
            "metadata": {"name": "api", "namespace": "team-a"},
            "spec": {"builderRef": "railpack", "source": source.clone()},
        }))
        .unwrap()
    }

    #[test]
    fn remote_names_keep_namespaces_apart() {
        let a = build_name("a-b", "c");
        let b = build_name("a", "b-c");
        assert_ne!(a, b);
        assert!(a.starts_with("c-"));
        let long = build_name("team", &"x".repeat(63));
        assert!(long.len() <= NAME_PREFIX + 9);
    }

    #[test]
    fn only_public_https_git_builds_remotely() {
        let public = build(&json!({"git": {"url": "https://github.com/a/b", "ref": "main"}}));
        assert_eq!(unsupported(&public), None);
        let ssh = build(&json!({"git": {"url": "ssh://git@github.com/a/b", "ref": "main"}}));
        assert!(unsupported(&ssh).is_some());
        let private = build(&json!({
            "git": {"url": "https://github.com/a/b", "ref": "main"},
            "credentialsRef": {"name": "git"},
        }));
        assert!(unsupported(&private).is_some());
        let archive = build(&json!({"archive": {"digest": "sha256:00"}}));
        assert!(unsupported(&archive).is_some());
    }

    #[test]
    fn the_remote_build_is_suspended_and_reads_no_secret() {
        let local = build(&json!({"git": {"url": "https://github.com/a/b", "ref": "main"}}));
        let remote = remote_build(&local, "api-12345678");
        assert_eq!(remote["spec"]["suspend"], true);
        assert_eq!(
            remote["spec"]["source"]["git"]["url"],
            "https://github.com/a/b"
        );
        assert!(remote["spec"]["source"].get("credentialsRef").is_none());
    }

    #[test]
    fn a_grant_close_to_expiry_is_asked_again() {
        let at: Timestamp = "2026-11-08T10:00:00Z".parse().unwrap();
        let with = |request: &str, expires: &str| BuildRun {
            status: Some(BuildRunStatus {
                pull_token: Some(PullToken {
                    request: request.to_owned(),
                    secret_name: "r-pull".to_owned(),
                    expires_at: expires.to_owned(),
                }),
                ..BuildRunStatus::default()
            }),
            ..serde_json::from_value(json!({
                "metadata": {"name": "r"},
                "spec": {"buildRef": "api", "revision": "abc"},
            }))
            .unwrap()
        };
        assert!(grant_usable(&with("u", "2026-11-08T10:15:00Z"), "u", at));
        assert!(!grant_usable(&with("u", "2026-11-08T10:03:00Z"), "u", at));
        assert!(!grant_usable(&with("u", "2026-11-08T09:59:00Z"), "u", at));
        assert!(!grant_usable(&with("old", "2026-11-08T10:15:00Z"), "u", at));
    }
}
