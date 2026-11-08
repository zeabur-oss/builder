//! Golden tests: the whole Job for each builder is compared with a committed file, so every change
//! to what runs in a build shows up in review. Regenerate with
//! `ZEABUR_BUILDER_BLESS=1 cargo nextest run -p zeabur-builder-jobspec`.

use std::collections::BTreeMap;

use pretty_assertions::assert_eq;
use zeabur_builder_api::build::{
    ArchiveSource, BuildSpec, Concurrency, EnvVarSource, GitSource, SourceSpec, TriggerSpec,
};
use zeabur_builder_api::build_run::{BuildRunSpec, Trigger};
use zeabur_builder_api::cluster_builder::{FrontendSpec, PlannerSpec};
use zeabur_builder_api::common::SecretKeyRef;

use super::*;

const SHA: &str = "3e1a9c2d4f0b5a6c7d8e9f0a1b2c3d4e5f6a7b8c";
const DIGEST: &str = "sha256:5d2a8f1c0b9e7d6a4c3b2a1f0e9d8c7b6a5f4e3d2c1b0a9f8e7d6c5b4a3f2e1d";
const RAILPACK: &str = "v0.43.0";

fn build(url: &str, context_dir: Option<&str>, env: Vec<BuildEnvVar>) -> Build {
    let mut build = Build::new(
        "api",
        BuildSpec {
            source: SourceSpec {
                git: Some(GitSource {
                    url: url.to_owned(),
                    git_ref: "main".to_owned(),
                    context_dir: context_dir.map(str::to_owned),
                    dockerfile: None,
                }),
                archive: None,
                credentials_ref: None,
                github_app: None,
            },
            builder_ref: "dockerfile".to_owned(),
            output: None,
            env,
            triggers: TriggerSpec::default(),
            concurrency: Concurrency::default(),
            history_limit: None,
            notify: Vec::new(),
            suspend: false,
        },
    );
    build.metadata.namespace = Some("team-a".to_owned());
    build
}

fn archive_build(namespace: &str) -> Build {
    let mut build = build("https://unused.example", None, Vec::new());
    build.spec.source = SourceSpec {
        git: None,
        archive: Some(ArchiveSource {
            digest: Some(DIGEST.to_owned()),
            context_dir: Some("app".to_owned()),
            dockerfile: None,
        }),
        credentials_ref: None,
        github_app: None,
    };
    build.metadata.namespace = Some(namespace.to_owned());
    build
}

fn archive_run(namespace: &str) -> BuildRun {
    let mut run = build_run();
    run.spec.revision = DIGEST.to_owned();
    run.metadata.namespace = Some(namespace.to_owned());
    run
}

fn build_run() -> BuildRun {
    let mut run = BuildRun::new(
        "api-x7k2p",
        BuildRunSpec {
            build_ref: "api".to_owned(),
            revision: SHA.to_owned(),
            git_ref: None,
            trigger: Some(Trigger::Webhook),
        },
    );
    run.metadata.namespace = Some("team-a".to_owned());
    run.metadata.uid = Some("6f1c2b8e-0d7a-4c1e-9a43-2f5b7e8d9c10".to_owned());
    run
}

fn dockerfile() -> ClusterBuilderSpec {
    ClusterBuilderSpec {
        kind: BuilderType::Dockerfile,
        planner: None,
        frontend: FrontendSpec {
            kind: FrontendType::Dockerfile,
            source: None,
            opts: BTreeMap::new(),
        },
    }
}

fn railpack() -> ClusterBuilderSpec {
    ClusterBuilderSpec {
        kind: BuilderType::Railpack,
        planner: Some(PlannerSpec {
            image: format!("ghcr.io/zeabur-oss/builder-railpack:{RAILPACK}"),
            timeout: None,
            resources: None,
        }),
        frontend: FrontendSpec {
            kind: FrontendType::Gateway,
            source: Some(format!("ghcr.io/railwayapp/railpack-frontend:{RAILPACK}")),
            opts: BTreeMap::new(),
        },
    }
}

fn inputs<'a>(
    run: &'a BuildRun,
    build: &'a Build,
    name: &'a str,
    builder: &'a ClusterBuilderSpec,
    tags: &'a [String],
    git_auth: GitAuth<'a>,
) -> JobInputs<'a> {
    JobInputs {
        build_run: run,
        build,
        builder_name: name,
        builder,
        image: "registry.internal/team-a/api",
        registry: "registry.internal",
        registry_ca: None,
        registry_http: false,
        tags,
        buildkitd_addr: "tcp://buildkitd-team-a-0.buildkitd-team-a.builder-system.svc:1234",
        job_image: "ghcr.io/zeabur-oss/builder-job:v0.0.1",
        git_auth,
        buildkit_tls_secret: "zeabur-builder-buildkit-client",
        registry_auth_secret: "api-x7k2p-registry",
        deadline_seconds: 3600,
        ttl_seconds: 600,
    }
}

fn golden(name: &str, expected: &str, job: &Job) {
    let actual = serde_json::to_string_pretty(job).unwrap() + "\n";
    if std::env::var_os("ZEABUR_BUILDER_BLESS").is_some() {
        let path = format!("{}/golden/{name}.json", env!("CARGO_MANIFEST_DIR"));
        std::fs::write(path, &actual).unwrap();
        return;
    }
    assert_eq!(expected, actual, "golden/{name}.json is stale");
}

fn tags() -> Vec<String> {
    vec!["main-1761058800-3e1a9c2".to_owned(), "latest".to_owned()]
}

#[test]
fn dockerfile_in_a_monorepo() {
    let run = build_run();
    let mut build = build(
        "https://github.com/acme/mono",
        Some("services/api"),
        Vec::new(),
    );
    if let Some(git) = &mut build.spec.source.git {
        git.dockerfile = Some("docker/Dockerfile.prod".to_owned());
    }
    let builder = dockerfile();
    let tags = tags();
    let job = job(&inputs(
        &run,
        &build,
        "dockerfile",
        &builder,
        &tags,
        GitAuth::Anonymous,
    ))
    .unwrap();
    golden(
        "dockerfile",
        include_str!("../golden/dockerfile.json"),
        &job,
    );
}

#[test]
fn railpack_with_secrets() {
    let run = build_run();
    let env = vec![
        BuildEnvVar {
            name: "NODE_ENV".to_owned(),
            value: Some("production".to_owned()),
            value_from: None,
        },
        BuildEnvVar {
            name: "NPM_TOKEN".to_owned(),
            value: None,
            value_from: Some(EnvVarSource {
                secret_key_ref: SecretKeyRef {
                    name: "npm".to_owned(),
                    key: "token".to_owned(),
                },
            }),
        },
    ];
    let build = build("https://github.com/acme/web", None, env);
    let builder = railpack();
    let tags = tags();
    let auth = GitAuth::Basic {
        secret: "api-x7k2p-git",
    };
    let job = job(&inputs(&run, &build, "railpack", &builder, &tags, auth)).unwrap();
    golden("railpack", include_str!("../golden/railpack.json"), &job);
}

#[test]
fn dockerfile_over_ssh() {
    let run = build_run();
    let build = build("git@git.example.com:team/api.git", None, Vec::new());
    let builder = dockerfile();
    let tags = tags();
    let auth = GitAuth::Ssh {
        secret: "deploy-key",
    };
    let job = job(&inputs(&run, &build, "dockerfile", &builder, &tags, auth)).unwrap();
    golden("ssh", include_str!("../golden/ssh.json"), &job);
}

#[test]
fn archive_with_a_private_registry_ca() {
    let run = archive_run("team-a");
    let build = archive_build("team-a");
    let builder = dockerfile();
    let tags = vec!["archive-1761058800-5d2a8f1".to_owned()];
    let mut inputs = inputs(
        &run,
        &build,
        "dockerfile",
        &builder,
        &tags,
        GitAuth::Anonymous,
    );
    inputs.registry_ca = Some("-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n");
    golden(
        "archive",
        include_str!("../golden/archive.json"),
        &job(&inputs).unwrap(),
    );
}

/// The archive repository follows from the run's namespace and `Build`, whatever digest the run
/// names, so a run in `team-b` with `team-a`'s digest reads `team-b`'s repository only.
#[test]
fn an_archive_is_read_only_from_the_runs_own_namespace() {
    let builder = dockerfile();
    let tags = tags();
    for namespace in ["team-a", "team-b"] {
        let run = archive_run(namespace);
        let build = archive_build(namespace);
        let job = job(&inputs(
            &run,
            &build,
            "dockerfile",
            &builder,
            &tags,
            GitAuth::Anonymous,
        ))
        .unwrap();
        let pod = job.spec.unwrap().template.spec.unwrap();
        let fetch = &pod.init_containers.unwrap()[0];
        let command = fetch.command.as_ref().unwrap();
        assert_eq!(
            command[2..],
            [
                format!("registry.internal/{namespace}/api/source"),
                DIGEST.to_owned(),
                "/workspace/src".to_owned()
            ]
        );
        let other = if namespace == "team-a" {
            "team-b"
        } else {
            "team-a"
        };
        assert!(!command.iter().any(|arg| arg.contains(other)));
        let auth = pod
            .volumes
            .unwrap()
            .into_iter()
            .find(|v| v.name == "registry-auth")
            .unwrap();
        assert_eq!(
            auth.secret.unwrap().secret_name.as_deref(),
            Some("api-x7k2p-registry")
        );
    }
}

#[test]
fn a_revision_must_fit_the_source() {
    let tags = tags();
    let builder = dockerfile();
    let build = build("https://github.com/acme/api", None, Vec::new());
    let digest_run = archive_run("team-a");
    assert_eq!(
        job(&inputs(
            &digest_run,
            &build,
            "dockerfile",
            &builder,
            &tags,
            GitAuth::Anonymous
        )),
        Err(JobError::Revision(DIGEST.to_owned()))
    );
    let archive = archive_build("team-a");
    assert_eq!(
        job(&inputs(
            &build_run(),
            &archive,
            "dockerfile",
            &builder,
            &tags,
            GitAuth::Anonymous
        )),
        Err(JobError::Revision(SHA.to_owned()))
    );
}

#[test]
fn refuses_what_admission_should_have_caught() {
    let tags = tags();
    let builder = dockerfile();
    let build = build("https://github.com/acme/api", None, Vec::new());

    let mut run = build_run();
    run.spec.revision = "main".to_owned();
    assert_eq!(
        job(&inputs(
            &run,
            &build,
            "dockerfile",
            &builder,
            &tags,
            GitAuth::Anonymous
        )),
        Err(JobError::Revision("main".to_owned()))
    );
    let run = build_run();
    for url in [
        "ext::sh -c id",
        "file:///etc",
        "/srv/git/api",
        "--upload-pack=id",
    ] {
        let build = self::build(url, None, Vec::new());
        assert!(
            matches!(
                job(&inputs(
                    &run,
                    &build,
                    "dockerfile",
                    &builder,
                    &tags,
                    GitAuth::Anonymous
                )),
                Err(JobError::Url(_))
            ),
            "{url}"
        );
    }

    let mut run = build_run();
    run.metadata.uid = None;
    assert_eq!(
        job(&inputs(
            &run,
            &build,
            "dockerfile",
            &builder,
            &tags,
            GitAuth::Anonymous
        )),
        Err(JobError::Metadata("uid"))
    );

    let run = build_run();
    let mut inconsistent = dockerfile();
    inconsistent.planner = railpack().planner;
    assert!(matches!(
        job(&inputs(
            &run,
            &build,
            "dockerfile",
            &inconsistent,
            &tags,
            GitAuth::Anonymous
        )),
        Err(JobError::Builder(_))
    ));
    assert_eq!(
        job(&inputs(
            &run,
            &build,
            "dockerfile",
            &builder,
            &[],
            GitAuth::Anonymous
        )),
        Err(JobError::NoTags)
    );
}

#[test]
fn user_values_never_enter_scripts() {
    let run = build_run();
    let build = build("https://github.com/acme/a$(id)b", None, Vec::new());
    let builder = dockerfile();
    let tags = tags();
    let job = job(&inputs(
        &run,
        &build,
        "dockerfile",
        &builder,
        &tags,
        GitAuth::Anonymous,
    ))
    .unwrap();
    let pod = job.spec.unwrap().template.spec.unwrap();
    let fetch = &pod.init_containers.unwrap()[0];
    assert_eq!(fetch.command.as_ref().unwrap()[2], FETCH_SCRIPT);
    // Kubernetes expands `$(VAR)` in command and args; `$$` keeps a literal `$`.
    assert_eq!(
        fetch.args.as_ref().unwrap()[0],
        "https://github.com/acme/a$$(id)b"
    );
    let build = &pod.containers[0];
    assert!(
        build
            .command
            .as_ref()
            .unwrap()
            .iter()
            .any(|arg| arg.contains("source=https://github.com/acme/a$$(id)b"))
    );
}

#[test]
fn import_copies_by_digest_under_the_build_hardening() {
    let tags = vec!["v1".to_owned(), "$(id)".to_owned()];
    let owner = OwnerReference {
        api_version: "builder.zeabur.com/v1alpha1".to_owned(),
        kind: "BuildRun".to_owned(),
        name: "api-x7k2p".to_owned(),
        uid: "6f1c2b8e-0d7a-4c1e-9a43-2f5b7e8d9c10".to_owned(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    };
    let from = format!("zot.remote:5000/exports/acme/x7k2p@{DIGEST}");
    let job = import_job(&ImportInputs {
        name: "api-x7k2p-import",
        namespace: "team-a",
        owner: Some(&owner),
        job_image: "ghcr.io/zeabur-oss/builder-job:v0.2.1",
        from: &from,
        to: "zot.local/team-a/api",
        tags: &tags,
        registry_auth_secret: "api-x7k2p-registry",
        registry_ca: Some("-----BEGIN CERTIFICATE-----"),
        registry_http: false,
        deadline_seconds: 900,
        ttl_seconds: 3600,
    });
    assert_eq!(
        job.metadata.owner_references.as_deref(),
        Some([owner].as_slice())
    );
    let spec = job.spec.unwrap();
    assert_eq!(spec.backoff_limit, Some(0));
    assert_eq!(spec.active_deadline_seconds, Some(900));
    assert_eq!(spec.ttl_seconds_after_finished, Some(3600));
    let pod = spec.template.spec.unwrap();

    // What the chart's admission policy checks.
    assert_eq!(pod.automount_service_account_token, Some(false));
    assert_eq!(pod.service_account_name, None);
    assert!(!pod.host_network.unwrap_or(false));
    let security = pod.security_context.unwrap();
    assert_eq!(security.run_as_non_root, Some(true));
    assert_eq!(security.run_as_user, Some(RUN_AS));
    for volume in pod.volumes.as_ref().unwrap() {
        assert!(volume.empty_dir.is_some() || volume.secret.is_some());
    }
    assert!(pod.init_containers.is_none());
    let [copy] = pod.containers.as_slice() else {
        panic!("one container");
    };
    assert_eq!(
        copy.image.as_deref(),
        Some("ghcr.io/zeabur-oss/builder-job:v0.2.1")
    );
    let context = copy.security_context.as_ref().unwrap();
    assert_eq!(context.allow_privilege_escalation, Some(false));
    assert_eq!(context.read_only_root_filesystem, Some(true));
    assert_eq!(context.privileged, None);
    assert_eq!(context.capabilities.as_ref().unwrap().add, None);

    assert_eq!(
        copy.command.as_ref().unwrap(),
        &[
            "zeabur-builder-job",
            "copy",
            "--work-dir",
            "/workspace/copy",
            "--from",
            from.as_str(),
            "--to",
            "zot.local/team-a/api",
            "--tag",
            "v1",
            "--tag",
            "$$(id)",
        ]
    );
    let env: BTreeMap<_, _> = copy
        .env
        .as_ref()
        .unwrap()
        .iter()
        .map(|e| (e.name.as_str(), e.value.as_deref().unwrap()))
        .collect();
    assert_eq!(env["DOCKER_CONFIG"], "/etc/zeabur-builder/registry");
    assert_eq!(env[REGISTRY_CA_ENV], "-----BEGIN CERTIFICATE-----");
    let auth = pod
        .volumes
        .unwrap()
        .into_iter()
        .find(|v| v.name == "registry-auth")
        .unwrap()
        .secret
        .unwrap();
    assert_eq!(auth.secret_name.as_deref(), Some("api-x7k2p-registry"));
    assert_eq!(auth.items.unwrap()[0].path, "config.json");
}
