//! Golden tests: the whole Job for each builder is compared with a committed file, so every change
//! to what runs in a build shows up in review. Regenerate with
//! `ZEABUR_BUILDER_BLESS=1 cargo nextest run -p zeabur-builder-jobspec`.

use std::collections::BTreeMap;

use pretty_assertions::assert_eq;
use zeabur_builder_api::build::{
    ArchiveSource, BuildSpec, Concurrency, EnvVarSource, GitSource, MAX_ENV, MAX_EXTRA_TAGS,
    MAX_PATH, MAX_URL, SourceKind, SourceSpec, TriggerSpec,
};
use zeabur_builder_api::build_run::{BuildRunSpec, Trigger};
use zeabur_builder_api::cluster_builder::{
    AgentPlanner, AnthropicFederation, AzureFederation, Frontend, FrontendSpec, MAX_IMAGE,
    MAX_OPT_KEY, MAX_OPT_VALUE, MAX_OPTS, ModelAuth, ModelSpec, Planner, PlannerSpec,
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
    let build_plan = build_plan_of(job);
    let mut job = job.clone();
    if let Some(annotations) = job
        .spec
        .as_mut()
        .and_then(|spec| spec.template.metadata.as_mut())
        .and_then(|metadata| metadata.annotations.as_mut())
    {
        annotations.remove(annotations::BUILD_PLAN);
    }
    let actual = serde_json::to_string_pretty(&(job, build_plan)).unwrap() + "\n";
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

/// The plan container gets the build variables too, for the agent's trial builds.
#[test]
fn agent_with_a_model_key() {
    let run = build_run();
    let env = vec![BuildEnvVar {
        name: "NPM_TOKEN".to_owned(),
        value: None,
        value_from: Some(EnvVarSource {
            secret_key_ref: SecretKeyRef {
                name: "npm".to_owned(),
                key: "token".to_owned(),
            },
        }),
    }];
    let mut build = build("https://github.com/acme/web", None, env);
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

/// An Azure run gets the variables the Azure SDKs read and a token for Microsoft Entra's
/// audience.
#[test]
fn agent_with_azure_federation() {
    let run = build_run();
    let mut build = build("https://github.com/acme/web", None, Vec::new());
    build.metadata.uid = Some(UID.to_owned());
    let mut builder = agent();
    if let Planner::Agent { agent } = &mut builder.planner {
        agent.model.provider = "azure.openai".to_owned();
        agent.model.auth = ModelAuth::AzureFederation {
            azure_federation: AzureFederation {
                service_account_name: "builder-agent".to_owned(),
                tenant_id: "tenant-1".to_owned(),
                client_id: "client-1".to_owned(),
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
        "agent-azure-federation",
        include_str!("../golden/agent-azure-federation.json"),
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
fn the_largest_build_fits_its_pod() {
    // A 4-byte character stays 4 bytes in JSON, a quote or a backslash becomes 2, and a control
    // character becomes 6 (`\u0001`).
    for filler in ["\u{1d54f}", "\"", "\\", "\u{1}"] {
        fits_its_pod(filler);
    }
}

fn fits_its_pod(filler: &str) {
    // A Pod's annotations hold at most 256 KiB, and Linux refuses `exec` when one argument passes
    // 32 pages (128 KiB on 4 KiB pages).
    const MAX_ANNOTATIONS: usize = 256 * 1024;
    const MAX_ARG: usize = 128 * 1024;
    let wide = |chars: usize| filler.repeat(chars);
    let run = build_run();
    let env = (0..MAX_ENV)
        .map(|i| BuildEnvVar {
            name: format!("{i:0>128}").replace('0', "A"),
            value: Some(String::new()),
            value_from: None,
        })
        .collect();
    let url = format!("https://github.com/acme/{}", "a".repeat(MAX_URL - 30));
    let context = "c".repeat(MAX_PATH);
    let build = build(&url, Some(&context), env);
    let mut builder = railpack();
    builder.frontend.opts = (0..MAX_OPTS)
        .map(|i| {
            (
                format!("{i:0>3}{}", wide(MAX_OPT_KEY - 3)),
                wide(MAX_OPT_VALUE),
            )
        })
        .collect();
    let tags: Vec<String> = (0..=MAX_EXTRA_TAGS)
        .map(|i| format!("{i}{}", "t".repeat(127)))
        .collect();
    let image = format!("registry.internal/{}", "i".repeat(MAX_IMAGE - 18));
    let job = job(&JobInputs {
        image: &image,
        ..inputs(
            &run,
            &build,
            "railpack",
            &builder,
            &tags,
            GitAuth::Anonymous,
        )
    })
    .unwrap();
    let plan = build_plan_of(&job);
    assert!(
        plan.builds[0]
            .command
            .iter()
            .any(|arg| arg.ends_with(&wide(MAX_OPT_VALUE)))
    );
    let template = job.spec.unwrap().template;
    let annotations = template.metadata.unwrap().annotations.unwrap();
    let size: usize = annotations.iter().map(|(k, v)| k.len() + v.len()).sum();
    assert!(
        size < MAX_ANNOTATIONS,
        "with {filler:?}, the Pod's annotations hold {size} bytes"
    );
    let pod = template.spec.unwrap();
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

/// A directory under the workspace's `target/` where [`FETCH_SCRIPT`] runs with `git` from `PATH`.
struct FetchDir(std::path::PathBuf);

impl FetchDir {
    fn new(name: &str) -> Self {
        let dir =
            std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/tmp")).join(
                format!("zeabur-builder-jobspec-{name}-{}", std::process::id()),
            );
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(dir.join("home")).unwrap();
        Self(dir)
    }

    fn git(&self, dir: &str, args: &[&str]) -> std::process::Output {
        self.command("git")
            .current_dir(self.0.join(dir))
            .args(args)
            .output()
            .unwrap()
    }

    /// A repository `name` with `file` and one commit, and its SHA.
    fn repo(&self, name: &str, file: &str, text: &str) -> String {
        std::fs::create_dir_all(self.0.join(name)).unwrap();
        self.git(name, &["init", "-q"]);
        std::fs::write(self.0.join(name).join(file), text).unwrap();
        self.commit(name)
    }

    fn commit(&self, name: &str) -> String {
        self.git(name, &["add", "-A"]);
        assert!(self.git(name, &["commit", "-qm", "c"]).status.success());
        let sha = self.git(name, &["rev-parse", "HEAD"]).stdout;
        String::from_utf8(sha).unwrap().trim().to_owned()
    }

    fn command(&self, program: &str) -> std::process::Command {
        let mut command = std::process::Command::new(program);
        command
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_AUTHOR_NAME", "a")
            .env("GIT_AUTHOR_EMAIL", "a@example.com")
            .env("GIT_COMMITTER_NAME", "a")
            .env("GIT_COMMITTER_EMAIL", "a@example.com");
        command
    }

    /// Runs [`FETCH_SCRIPT`] with its paths under this directory.
    fn fetch(&self, url: &str, sha: &str) -> std::process::Output {
        let root = self.0.to_str().unwrap();
        let script = FETCH_SCRIPT
            .replace("/etc/zeabur-builder/git", &format!("{root}/git"))
            .replace("/workspace", &format!("{root}/workspace"));
        self.command("sh")
            .args(["-c", &script, "fetch", url, sha])
            .output()
            .unwrap()
    }

    fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.0.join(path)).unwrap()
    }
}

impl Drop for FetchDir {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.0));
    }
}

#[test]
fn fetch_checks_out_submodules_at_their_recorded_commits() {
    let dir = FetchDir::new("submodules");
    dir.repo("icons", "icon.svg", "<svg/>\n");
    dir.repo("theme", "style.css", "v1\n");
    dir.git("theme", &["submodule", "-q", "add", "../icons", "icons"]);
    dir.commit("theme");
    dir.repo("site", "hugo.toml", "theme = 'paper'\n");
    dir.git(
        "site",
        &["submodule", "-q", "add", "../theme", "themes/paper"],
    );
    let sha = dir.commit("site");
    std::fs::write(dir.0.join("theme/style.css"), "v2\n").unwrap();
    dir.commit("theme");

    let url = format!("file://{}/site", dir.0.display());
    let out = dir.fetch(&url, &sha);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(dir.read("workspace/src/themes/paper/style.css"), "v1\n");
    assert_eq!(
        dir.read("workspace/src/themes/paper/icons/icon.svg"),
        "<svg/>\n"
    );
}

#[test]
fn fetch_fails_naming_a_submodule_it_cannot_fetch() {
    let dir = FetchDir::new("missing-submodule");
    dir.repo("theme", "style.css", "v1\n");
    dir.repo("site", "hugo.toml", "theme = 'paper'\n");
    dir.git(
        "site",
        &["submodule", "-q", "add", "../theme", "themes/paper"],
    );
    dir.git(
        "site",
        &[
            "config",
            "-f",
            ".gitmodules",
            "submodule.themes/paper.url",
            "../gone",
        ],
    );
    let sha = dir.commit("site");

    let url = format!("file://{}/site", dir.0.display());
    let out = dir.fetch(&url, &sha);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("'themes/paper'"));
}

#[test]
fn fetch_sends_basic_auth_only_to_the_repository_s_host() {
    let dir = FetchDir::new("credentials");
    std::fs::create_dir_all(dir.0.join("git")).unwrap();
    std::fs::write(dir.0.join("git/username"), "x-access-token").unwrap();
    std::fs::write(dir.0.join("git/password"), "secret").unwrap();
    // Nothing listens on port 1, so the fetch fails after the script configures Git.
    dir.fetch("https://me@127.0.0.1:1/acme/app.git", SHA);

    for (host, sent) in [
        ("127.0.0.1:1", true),
        ("127.0.0.1:2", false),
        ("github.com", false),
    ] {
        let mut fill = dir
            .command("git")
            .args(["credential", "fill"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::io::Write::write_all(
            fill.stdin.as_mut().unwrap(),
            format!("protocol=https\nhost={host}\npath=acme/theme.git\n\n").as_bytes(),
        )
        .unwrap();
        let out = fill.wait_with_output().unwrap();
        let answer = String::from_utf8_lossy(&out.stdout);
        assert_eq!(answer.contains("password=secret"), sent, "{host}: {answer}");
    }
}
