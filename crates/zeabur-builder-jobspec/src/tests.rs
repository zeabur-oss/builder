//! Golden tests: the whole Job for each builder is compared with a committed file, so every change
//! to what runs in a build shows up in review. Regenerate with
//! `ZEABUR_BUILDER_BLESS=1 cargo nextest run -p zeabur-builder-jobspec`.

use std::collections::BTreeMap;

use pretty_assertions::assert_eq;
use zeabur_builder_api::build::{
    ArchiveSource, BuildSpec, Concurrency, EnvVarSource, GitSource, SourceKind, SourceSpec,
    TriggerSpec,
};
use zeabur_builder_api::build_run::{BuildRunSpec, Trigger};
use zeabur_builder_api::cluster_builder::{
    AgentPlanner, AnthropicFederation, Frontend, FrontendSpec, MAX_OPTS, ModelAuth, ModelSpec,
    Planner, PlannerSpec,
};
use zeabur_builder_api::common::SecretKeyRef;

use super::*;

const SHA: &str = "3e1a9c2d4f0b5a6c7d8e9f0a1b2c3d4e5f6a7b8c";
/// The `Build`'s uid, which names its saved plan.
const UID: &str = "0b6e3f52-8c1d-4a7e-b0f9-1d2c3e4f5a6b";
const DIGEST: &str = "sha256:5d2a8f1c0b9e7d6a4c3b2a1f0e9d8c7b6a5f4e3d2c1b0a9f8e7d6c5b4a3f2e1d";
const RAILPACK: &str = "v0.43.0";

fn build(url: &str, context_dir: Option<&str>, env: Vec<BuildEnvVar>) -> Build {
    let mut build = Build::new(
        "api",
        BuildSpec {
            source: SourceSpec {
                kind: SourceKind::Git {
                    git: GitSource {
                        url: url.to_owned(),
                        git_ref: "main".to_owned(),
                        credentials_ref: None,
                        github_app: None,
                    },
                },
                context_dir: context_dir.map(str::to_owned),
                dockerfile: None,
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
        kind: SourceKind::Archive {
            archive: ArchiveSource {
                digest: Some(DIGEST.to_owned()),
            },
        },
        context_dir: Some("app".to_owned()),
        dockerfile: None,
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
        planner: Planner::Dockerfile,
        frontend: FrontendSpec {
            kind: Frontend::Dockerfile,
            opts: BTreeMap::new(),
        },
    }
}

fn railpack() -> ClusterBuilderSpec {
    ClusterBuilderSpec {
        planner: Planner::Railpack {
            railpack: PlannerSpec {
                image: format!("ghcr.io/zeabur-oss/builder-railpack:{RAILPACK}"),
                timeout: None,
                resources: None,
            },
        },
        frontend: FrontendSpec {
            kind: Frontend::Gateway {
                source: format!("ghcr.io/railwayapp/railpack-frontend:{RAILPACK}"),
            },
            opts: BTreeMap::new(),
        },
    }
}

const AGENT_IMAGE: &str = "ghcr.io/zeabur-oss/builder-agent:v0.0.1";

fn agent() -> ClusterBuilderSpec {
    ClusterBuilderSpec {
        planner: Planner::Agent {
            agent: AgentPlanner {
                planner: PlannerSpec {
                    image: AGENT_IMAGE.to_owned(),
                    timeout: None,
                    resources: None,
                },
                model: ModelSpec {
                    provider: "anthropic".to_owned(),
                    model: "claude-opus-5-5".to_owned(),
                    base_url: None,
                    auth: ModelAuth::ApiKey {
                        api_key: SecretKeyRef {
                            name: "model".to_owned(),
                            key: "anthropic".to_owned(),
                        },
                    },
                },
            },
        },
        frontend: FrontendSpec {
            kind: Frontend::Dockerfile,
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
        placement: &NOWHERE,
        memory: &MEMORY,
        saved_plan: None,
    }
}

static NOWHERE: Placement = Placement {
    node_selector: BTreeMap::new(),
    tolerations: Vec::new(),
};

static MEMORY: std::sync::LazyLock<JobMemory> = std::sync::LazyLock::new(|| JobMemory {
    request: "128Mi".to_owned(),
    limit: "512Mi".to_owned(),
});

/// Compares `job` with `golden/<name>.json`, with the build plan shown as JSON, not as a string.
fn golden(name: &str, expected: &str, job: &Job) {
    let mut job = serde_json::to_value(job).unwrap();
    let plan = &mut job["spec"]["template"]["metadata"]["annotations"][annotations::BUILD_PLAN];
    *plan = serde_json::from_str(plan.as_str().unwrap()).unwrap();
    let actual = serde_json::to_string_pretty(&job).unwrap() + "\n";
    if std::env::var_os("ZEABUR_BUILDER_BLESS").is_some() {
        let path = format!("{}/golden/{name}.json", env!("CARGO_MANIFEST_DIR"));
        std::fs::write(path, &actual).unwrap();
        return;
    }
    assert_eq!(expected, actual, "golden/{name}.json is stale");
}

/// The plan the build container runs.
fn build_plan_of(job: &Job) -> BuildPlan {
    let template = &job.spec.as_ref().unwrap().template;
    let annotations = template.metadata.as_ref().unwrap().annotations.as_ref();
    serde_json::from_str(&annotations.unwrap()[annotations::BUILD_PLAN]).unwrap()
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
    build.spec.source.dockerfile = Some("docker/Dockerfile.prod".to_owned());
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
fn agent_with_a_model_key() {
    let run = build_run();
    let mut build = build("https://github.com/acme/web", None, Vec::new());
    build.metadata.uid = Some(UID.to_owned());
    let builder = agent();
    let tags = tags();
    let job = job(&inputs(
        &run,
        &build,
        "agent",
        &builder,
        &tags,
        GitAuth::Anonymous,
    ))
    .unwrap();
    golden("agent", include_str!("../golden/agent.json"), &job);
}

/// A federated run runs as the service account the builder names, and only the plan container
/// mounts its token, for Anthropic's audience.
#[test]
fn agent_with_anthropic_federation() {
    let run = build_run();
    let mut build = build("https://github.com/acme/web", None, Vec::new());
    build.metadata.uid = Some(UID.to_owned());
    let mut builder = agent();
    if let Planner::Agent { agent } = &mut builder.planner {
        agent.model.auth = ModelAuth::AnthropicFederation {
            anthropic_federation: AnthropicFederation {
                service_account_name: "builder-agent".to_owned(),
                federation_rule_id: "fdrl_1".to_owned(),
                organization_id: "org".to_owned(),
                workspace_id: Some("wrkspc_1".to_owned()),
                service_account_id: None,
            },
        };
    }
    let tags = tags();
    let job = job(&inputs(
        &run,
        &build,
        "agent",
        &builder,
        &tags,
        GitAuth::Anonymous,
    ))
    .unwrap();
    golden(
        "agent-federation",
        include_str!("../golden/agent-federation.json"),
        &job,
    );
}

#[test]
fn a_reused_plan_is_restored_from_the_build_s_own_repository_instead_of_planning() {
    let run = build_run();
    let mut build = build("https://github.com/acme/web", None, Vec::new());
    build.metadata.uid = Some(UID.to_owned());
    let builder = agent();
    let tags = tags();
    let job_with = |saved_plan: Option<&str>| {
        job(&JobInputs {
            saved_plan,
            ..inputs(&run, &build, "agent", &builder, &tags, GitAuth::Anonymous)
        })
        .unwrap()
    };
    let annotation = |job: &Job, key: &str| job.metadata.annotations.as_ref()?.get(key).cloned();
    let pod = |job: Job| job.spec.unwrap().template.spec.unwrap();
    let plan_of = |spec: &PodSpec| {
        spec.init_containers
            .iter()
            .flatten()
            .find(|c| c.name == "plan")
            .cloned()
            .unwrap()
    };
    let reads_registry = |c: &Container| {
        c.volume_mounts
            .iter()
            .flatten()
            .any(|m| m.name == "registry-auth")
    };
    let saves = |job: &Job| {
        build_plan_of(job).save_plan
            == Some(Image {
                repository: "registry.internal/team-a/api".to_owned(),
                tag: format!("zeabur-plan-{UID}"),
            })
    };

    let fresh_job = job_with(None);
    assert_eq!(
        annotation(&fresh_job, annotations::IMAGE).as_deref(),
        Some("registry.internal/team-a/api")
    );
    assert_eq!(annotation(&fresh_job, annotations::SAVED_PLAN), None);
    assert!(saves(&fresh_job));
    let fresh = pod(fresh_job);
    let planner = plan_of(&fresh);
    assert_eq!(planner.image.as_deref(), Some(AGENT_IMAGE));
    assert!(!reads_registry(&planner));

    let digest = "sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    let reused_job = job_with(Some(digest));
    assert_eq!(
        annotation(&reused_job, annotations::SAVED_PLAN).as_deref(),
        Some(digest)
    );
    assert!(!saves(&reused_job));
    let reused = pod(reused_job);
    let restore = plan_of(&reused);
    assert_eq!(
        restore.command.as_deref().unwrap(),
        [
            "zeabur-builder-job",
            "plan",
            "restore",
            "registry.internal/team-a/api",
            digest,
            "/workspace/plan",
        ]
    );
    assert!(reads_registry(&restore));
    // The kubelet splits 12 KiB of termination messages over the Pod's containers, and the build
    // result needs 4096 bytes: the build container and at most two init containers.
    assert!(reused.init_containers.unwrap().len() <= 2);
}

#[test]
fn every_builder_with_a_planner_saves_its_plan() {
    let run = build_run();
    let mut build = build("https://github.com/acme/web", None, Vec::new());
    build.metadata.uid = Some(UID.to_owned());
    let tags = tags();
    let saves = |name: &str, builder: &ClusterBuilderSpec| {
        let job = job(&inputs(
            &run,
            &build,
            name,
            builder,
            &tags,
            GitAuth::Anonymous,
        ))
        .unwrap();
        build_plan_of(&job).save_plan.is_some()
    };

    assert!(saves("agent", &agent()));
    assert!(saves("railpack", &railpack()));
    assert!(!saves("dockerfile", &dockerfile()));
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
        let fetch = &pod.init_containers.as_ref().unwrap()[0];
        let command = fetch.command.as_ref().unwrap();
        assert_eq!(
            command[3..],
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
fn a_dockerfile_frontend_image_reads_the_dockerfile_the_builder_names() {
    let tags = tags();
    let run = build_run();
    let mut build = build("https://github.com/acme/mono", Some("api"), Vec::new());
    build.spec.source.dockerfile = Some("docker/Dockerfile.prod".to_owned());
    let image = Frontend::Gateway {
        source: "docker/dockerfile:1".to_owned(),
    };
    let mut repository = dockerfile();
    repository.frontend.kind = image.clone();
    let mut planned = agent();
    planned.frontend.kind = image;
    for (builder, dir, filename) in [
        (repository, "/workspace/src/api/docker", "Dockerfile.prod"),
        (planned, "/workspace/plan", "Dockerfile"),
    ] {
        let job = job(&inputs(
            &run,
            &build,
            "custom",
            &builder,
            &tags,
            GitAuth::Anonymous,
        ))
        .unwrap();
        let command = &build_plan_of(&job).builds[0].command;
        for expected in [
            "gateway.v0".to_owned(),
            "source=docker/dockerfile:1".to_owned(),
            format!("dockerfile={dir}"),
            format!("filename={filename}"),
        ] {
            assert!(command.contains(&expected), "{expected} in {command:?}");
        }
    }
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
    // The kubelet does not expand the plan, which reaches the build container as a file.
    assert_eq!(
        build_plan_of(&job).builds[0].output.annotations["org.opencontainers.image.source"],
        "https://github.com/acme/a$(id)b"
    );
    let pod = job.spec.unwrap().template.spec.unwrap();
    let fetch = &pod.init_containers.unwrap()[0];
    assert_eq!(fetch.command.as_ref().unwrap()[2], FETCH_SCRIPT);
    // Kubernetes expands `$(VAR)` in command and args; `$$` keeps a literal `$`.
    assert_eq!(
        fetch.args.as_ref().unwrap()[0],
        "https://github.com/acme/a$$(id)b"
    );
}

/// The build node of `docs/builds/build-nodes.mdx`.
pub(crate) fn build_node() -> Placement {
    Placement {
        node_selector: BTreeMap::from([(
            "builder.zeabur.com/builds".to_owned(),
            "true".to_owned(),
        )]),
        tolerations: vec![Toleration {
            key: Some("builder.zeabur.com/builds".to_owned()),
            operator: Some("Equal".to_owned()),
            value: Some("true".to_owned()),
            effect: Some("NoSchedule".to_owned()),
            toleration_seconds: None,
        }],
    }
}

#[test]
fn jobs_run_on_the_build_node() {
    let run = build_run();
    let build = build("https://github.com/acme/api", None, Vec::new());
    let builder = dockerfile();
    let tags = tags();
    let placement = build_node();
    let mut inputs = inputs(
        &run,
        &build,
        "dockerfile",
        &builder,
        &tags,
        GitAuth::Anonymous,
    );
    inputs.placement = &placement;
    let pod = job(&inputs).unwrap().spec.unwrap().template.spec.unwrap();
    assert_eq!(pod.node_selector, Some(placement.node_selector));
    assert_eq!(pod.tolerations, Some(placement.tolerations));
}

#[test]
fn a_memory_request_never_exceeds_the_limit() {
    let run = build_run();
    let build = build("https://github.com/acme/api", None, Vec::new());
    let builder = railpack();
    let tags = tags();
    let memory = JobMemory {
        request: "1Gi".to_owned(),
        limit: "256Mi".to_owned(),
    };
    let mut inputs = inputs(
        &run,
        &build,
        "railpack",
        &builder,
        &tags,
        GitAuth::Anonymous,
    );
    inputs.memory = &memory;
    let pod = job(&inputs).unwrap().spec.unwrap().template.spec.unwrap();
    let mut containers = pod.init_containers.unwrap();
    containers.extend(pod.containers);
    for container in containers {
        let resources = container.resources.unwrap();
        let want = BTreeMap::from([("memory".to_owned(), Quantity("256Mi".to_owned()))]);
        assert_eq!(resources.requests, Some(want.clone()), "{}", container.name);
        assert_eq!(resources.limits, Some(want), "{}", container.name);
    }
}

#[test]
fn the_largest_plan_passes_no_argument_over_the_kernel_limit() {
    // Linux refuses `exec` when one argument passes 32 pages (128 KiB on 4 KiB pages).
    const MAX_ARG: usize = 128 * 1024;
    let run = build_run();
    let build = build("https://github.com/acme/web", None, Vec::new());
    let mut builder = dockerfile();
    builder.frontend.opts = (0..MAX_OPTS)
        .map(|i| (format!("build-arg:A{i}"), "x".repeat(4096)))
        .collect();
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
    let last = format!("build-arg:A{}={}", MAX_OPTS - 1, "x".repeat(4096));
    assert!(build_plan_of(&job).builds[0].command.contains(&last));
    let pod = job.spec.unwrap().template.spec.unwrap();
    for container in pod.init_containers.unwrap().iter().chain(&pod.containers) {
        for arg in container.command.iter().chain(&container.args).flatten() {
            assert!(
                arg.len() < MAX_ARG,
                "{} has a {} byte argument",
                container.name,
                arg.len()
            );
        }
    }
}
