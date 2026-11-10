use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::{JobCondition, JobStatus};
use k8s_openapi::api::core::v1::{
    ContainerState, ContainerStateRunning, ContainerStateTerminated, PodStatus,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use zeabur_builder_api::build::{
    ArchiveSource, BuildSpec, GitHubAppSource, GitSource, NotifyTarget, NotifyType, OutputSpec,
    SourceSpec, TriggerSpec,
};
use zeabur_builder_api::build_run::BuildRunSpec;

use super::*;

const A: &str = "3e1a9c2d4f0b5a6c7d8e9f0a1b2c3d4e5f6a7b8c";
const B: &str = "0123456789abcdef0123456789abcdef01234567";
const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn set_ref(build: &mut Build, git_ref: &str) {
    if let Some(git) = &mut build.spec.source.git {
        git.git_ref = git_ref.to_owned();
    }
}

fn now() -> Timestamp {
    "2026-10-22T08:00:00Z".parse().unwrap()
}

fn build() -> Build {
    let mut build = Build::new(
        "api",
        BuildSpec {
            source: SourceSpec {
                git: Some(GitSource {
                    url: "https://github.com/acme/api".to_owned(),
                    git_ref: "main".to_owned(),
                    context_dir: None,
                    dockerfile: None,
                }),
                archive: None,
                credentials_ref: None,
                github_app: None,
            },
            builder_ref: "dockerfile".to_owned(),
            output: None,
            env: Vec::new(),
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

fn run(name: &str, revision: &str, phase: Option<Phase>, at: &str) -> BuildRun {
    let mut run = BuildRun::new(
        name,
        BuildRunSpec {
            build_ref: "api".to_owned(),
            revision: revision.to_owned(),
            git_ref: None,
            trigger: None,
        },
    );
    run.metadata.creation_timestamp = Some(Time(at.parse().unwrap()));
    run.status = Some(BuildRunStatus {
        phase,
        completion_time: is_finished(phase).then(|| at.to_owned()),
        ..BuildRunStatus::default()
    });
    run
}

#[test]
fn durations() {
    assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
    assert_eq!(parse_duration("1m30s"), Some(Duration::from_secs(90)));
    assert_eq!(parse_duration("1.5h"), Some(Duration::from_mins(90)));
    assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
    assert_eq!(parse_duration(""), None);
    assert_eq!(parse_duration("10"), None);
    assert_eq!(parse_duration("m"), None);
    assert_eq!(parse_duration("1d"), None);
}

#[test]
fn conditions_keep_their_transition_time() {
    let mut conditions = Vec::new();
    let earlier: Timestamp = "2026-10-22T07:00:00Z".parse().unwrap();
    set_condition(
        &mut conditions,
        "Ready",
        ConditionStatus::True,
        ConditionReason::Succeeded,
        "",
        earlier,
        Some(1),
    );
    set_condition(
        &mut conditions,
        "Ready",
        ConditionStatus::True,
        ConditionReason::Succeeded,
        "again",
        now(),
        Some(2),
    );
    assert_eq!(conditions.len(), 1);
    assert_eq!(conditions[0].last_transition_time, earlier.to_string());
    assert_eq!(conditions[0].message, "again");
    set_condition(
        &mut conditions,
        "Ready",
        ConditionStatus::False,
        ConditionReason::BuildFailed,
        "",
        now(),
        Some(2),
    );
    assert_eq!(conditions[0].last_transition_time, now().to_string());
}

#[test]
fn resolves_once_per_interval_or_new_generation() {
    let status = BuildStatus {
        remote_revision: Some(A.to_owned()),
        last_resolved_at: Some("2026-10-22T07:59:30Z".to_owned()),
        observed_generation: Some(1),
        ..BuildStatus::default()
    };
    let minute = Duration::from_mins(1);
    assert!(!needs_resolve(Some(&status), Some(1), false, minute, now()));
    assert!(needs_resolve(Some(&status), Some(2), false, minute, now()));
    assert!(needs_resolve(
        Some(&status),
        Some(1),
        false,
        Duration::from_secs(30),
        now()
    ));
    assert!(needs_resolve(None, Some(1), false, minute, now()));
    assert!(needs_resolve(Some(&status), Some(1), true, minute, now()));
}

#[test]
fn a_woken_build_records_the_webhook_as_its_trigger() {
    let build = build();
    assert_eq!(
        next_run(&build, B, &[], Trigger::Webhook).create,
        Some(Trigger::Webhook)
    );
    let runs = [run(
        "api-1",
        B,
        Some(Phase::Succeeded),
        "2026-10-22T07:00:00Z",
    )];
    assert_eq!(
        next_run(&build, B, &runs, Trigger::Webhook),
        NextRun::default(),
        "a delivery for a commit already built builds nothing"
    );
}

#[test]
fn webhooks_wake_from_status_and_wait_out_the_gap() {
    let status = |hook: Option<&str>, resolved: Option<&str>| BuildStatus {
        last_webhook_at: hook.map(str::to_owned),
        last_resolved_at: resolved.map(str::to_owned),
        ..BuildStatus::default()
    };
    assert_eq!(wake(None, now()), Wake::Idle);
    assert_eq!(
        wake(Some(&status(None, Some("2026-10-22T07:00:00Z"))), now()),
        Wake::Idle
    );
    assert_eq!(
        wake(
            Some(&status(
                Some("2026-10-22T07:59:00Z"),
                Some("2026-10-22T07:59:30Z")
            )),
            now()
        ),
        Wake::Idle,
        "resolved after the delivery"
    );
    assert_eq!(
        wake(
            Some(&status(
                Some("2026-10-22T07:59:59Z"),
                Some("2026-10-22T07:59:55Z")
            )),
            now()
        ),
        Wake::Now
    );
    assert_eq!(
        wake(
            Some(&status(
                Some("2026-10-22T07:59:59Z"),
                Some("2026-10-22T07:59:58Z")
            )),
            now()
        ),
        Wake::After(Duration::from_secs(3))
    );
    assert_eq!(
        wake(Some(&status(Some("2026-10-22T07:59:59Z"), None)), now()),
        Wake::Now
    );
}

#[test]
fn app_sources_poll_less_when_webhooks_arrive() {
    let mut build = build();
    assert_eq!(default_poll(&build, true), DEFAULT_POLL);
    build.spec.source.github_app = Some(GitHubAppSource { installation_id: 1 });
    assert_eq!(default_poll(&build, true), APP_POLL);
    assert_eq!(default_poll(&build, false), DEFAULT_POLL);
}

#[test]
fn a_new_commit_replaces_the_running_build() {
    let build = build();
    let runs = [run(
        "api-1",
        A,
        Some(Phase::Building),
        "2026-10-22T07:00:00Z",
    )];
    assert_eq!(
        next_run(&build, B, &runs, Trigger::Poll),
        NextRun {
            create: Some(Trigger::Poll),
            cancel: vec!["api-1".to_owned()]
        }
    );
    assert_eq!(
        next_run(&build, A, &runs, Trigger::Poll),
        NextRun::default()
    );
}

#[test]
fn queue_waits_and_failures_are_not_retried() {
    let mut build = build();
    build.spec.concurrency = Concurrency::Queue;
    let running = [run(
        "api-1",
        A,
        Some(Phase::Building),
        "2026-10-22T07:00:00Z",
    )];
    assert_eq!(
        next_run(&build, B, &running, Trigger::Poll),
        NextRun::default()
    );
    let failed = [run("api-1", A, Some(Phase::Failed), "2026-10-22T07:00:00Z")];
    assert_eq!(
        next_run(&build, A, &failed, Trigger::Poll),
        NextRun::default()
    );
    assert_eq!(
        next_run(&build, B, &failed, Trigger::Poll).create,
        Some(Trigger::Poll)
    );

    build.spec.suspend = true;
    assert_eq!(
        next_run(&build, B, &failed, Trigger::Poll),
        NextRun::default()
    );
}

#[test]
fn a_built_commit_is_not_built_again_after_pruning() {
    let mut build = build();
    build.status = Some(BuildStatus {
        latest_revision: Some(A.to_owned()),
        ..BuildStatus::default()
    });
    assert_eq!(next_run(&build, A, &[], Trigger::Poll), NextRun::default());
}

fn rebuild(mut meta: ObjectMeta, value: &str) -> ObjectMeta {
    meta.annotations
        .get_or_insert_default()
        .insert(annotations::REBUILD.to_owned(), value.to_owned());
    meta
}

#[test]
fn a_new_rebuild_value_builds_the_commit_again() {
    let mut build = build();
    build.status = Some(BuildStatus {
        latest_revision: Some(A.to_owned()),
        ..BuildStatus::default()
    });
    let mut first = run("api-1", A, Some(Phase::Succeeded), "2026-10-22T07:00:00Z");
    assert_eq!(
        next_run(&build, A, &[first.clone()], Trigger::Poll),
        NextRun::default()
    );

    build.metadata = rebuild(build.metadata, "1");
    assert_eq!(pending_rebuild(&build, &[first.clone()]), Some("1"));
    assert_eq!(
        next_run(&build, A, &[first.clone()], Trigger::Poll),
        NextRun {
            create: Some(Trigger::Manual),
            cancel: Vec::new()
        }
    );
    assert_eq!(
        next_run(&build, A, &[], Trigger::Poll).create,
        Some(Trigger::Manual),
        "after pruning too"
    );

    first.metadata = rebuild(first.metadata, "1");
    let by_hand = run("api-2", A, Some(Phase::Failed), "2026-10-22T07:30:00Z");
    let runs = [first, by_hand];
    assert_eq!(pending_rebuild(&build, &runs), None);
    assert_eq!(
        next_run(&build, A, &runs, Trigger::Poll),
        NextRun::default()
    );

    build.metadata = rebuild(build.metadata, "2");
    assert_eq!(
        next_run(&build, A, &runs, Trigger::Poll).create,
        Some(Trigger::Manual)
    );
    assert_eq!(
        next_run(&build, B, &runs, Trigger::Poll).create,
        Some(Trigger::Poll),
        "a new commit answers the rebuild too"
    );
}

#[test]
fn a_rebuild_follows_concurrency_and_waits_for_suspend() {
    let mut build = build();
    build.metadata = rebuild(build.metadata, "2");
    let mut running = run("api-1", A, Some(Phase::Building), "2026-10-22T07:00:00Z");
    running.metadata = rebuild(running.metadata, "1");
    let runs = [running.clone()];
    assert_eq!(
        next_run(&build, A, &runs, Trigger::Poll),
        NextRun {
            create: Some(Trigger::Manual),
            cancel: vec!["api-1".to_owned()]
        }
    );

    build.spec.concurrency = Concurrency::Queue;
    assert_eq!(
        next_run(&build, A, &runs, Trigger::Poll),
        NextRun::default()
    );
    assert_eq!(pending_rebuild(&build, &runs), Some("2"));

    build.spec.suspend = true;
    assert_eq!(pending_rebuild(&build, &runs), None);
    let mut done = running;
    done.status = Some(BuildRunStatus {
        phase: Some(Phase::Succeeded),
        ..BuildRunStatus::default()
    });
    assert_eq!(
        next_run(&build, A, &[done.clone()], Trigger::Poll),
        NextRun::default()
    );
    build.spec.suspend = false;
    assert_eq!(
        next_run(&build, A, &[done], Trigger::Poll).create,
        Some(Trigger::Manual)
    );
}

#[test]
fn pruning_keeps_the_answer_to_the_last_rebuild() {
    let mut answered = run("api-1", A, Some(Phase::Failed), "2026-10-22T01:00:00Z");
    answered.metadata = rebuild(answered.metadata, "1");
    let runs = [
        answered,
        run("api-2", A, Some(Phase::Failed), "2026-10-22T02:00:00Z"),
        run("api-3", A, Some(Phase::Failed), "2026-10-22T03:00:00Z"),
    ];
    assert_eq!(prune(&runs, 1), ["api-2"]);
}

#[test]
fn pruning_keeps_the_newest_success() {
    let runs = [
        run("api-1", A, Some(Phase::Succeeded), "2026-10-22T01:00:00Z"),
        run("api-2", B, Some(Phase::Failed), "2026-10-22T02:00:00Z"),
        run("api-3", B, Some(Phase::Failed), "2026-10-22T03:00:00Z"),
        run("api-4", B, Some(Phase::Cancelled), "2026-10-22T04:00:00Z"),
        run("api-5", B, Some(Phase::Building), "2026-10-22T05:00:00Z"),
    ];
    assert_eq!(prune(&runs, 2), ["api-2"]);
    assert_eq!(prune(&runs, 1), ["api-2", "api-3"]);
    assert_eq!(prune(&runs, 10), Vec::<String>::new());
}

#[test]
fn concurrency_follows_the_namespace() {
    assert_eq!(concurrency_limit(None, 3), 3);
    assert_eq!(concurrency_limit(Some("5"), 3), 5);
    assert_eq!(concurrency_limit(Some(" 1 "), 3), 1);
    for bad in ["0", "-1", "many", ""] {
        assert_eq!(concurrency_limit(Some(bad), 3), 3, "{bad}");
    }
}

fn started(namespace: &str, name: &str, at: &str) -> BuildRun {
    let mut run = waiting(namespace, name, at);
    if let Some(status) = run.status.as_mut() {
        status.job_ref = Some(name.to_owned());
    }
    run
}

fn waiting(namespace: &str, name: &str, at: &str) -> BuildRun {
    let mut run = run(name, A, Some(Phase::Pending), at);
    run.metadata.namespace = Some(namespace.to_owned());
    run
}

#[test]
fn a_namespace_and_the_cluster_each_cap_running_builds() {
    let mine = waiting("team-a", "api-b", "2026-10-22T07:59:00Z");
    let running = [started("team-a", "api-a", "2026-10-22T07:58:00Z")];
    assert!(admission(&[], &mine, 1, 1).is_ok());
    assert!(admission(&running, &mine, 1, 5).is_err(), "namespace full");
    assert!(admission(&running, &mine, 2, 5).is_ok());
    let elsewhere = [started("team-b", "web-a", "2026-10-22T07:58:00Z")];
    assert!(admission(&elsewhere, &mine, 1, 1).is_err(), "cluster full");
    assert!(admission(&elsewhere, &mine, 1, 2).is_ok());
}

#[test]
fn the_oldest_waiting_run_gets_a_free_slot() {
    let old = waiting("team-b", "web-a", "2026-10-22T07:00:00Z");
    let new = waiting("team-a", "api-a", "2026-10-22T07:30:00Z");
    let runs = [new.clone(), old.clone()];
    assert!(admission(&runs, &new, 1, 1).is_err());
    assert!(admission(&runs, &old, 1, 1).is_ok());
    let mut blocked = old;
    if let Some(status) = blocked.status.as_mut() {
        status.reason = Some(Reason::RegistryCredentialsMissing);
    }
    assert!(
        admission(&[new.clone(), blocked], &new, 1, 1).is_ok(),
        "a run stuck on something else holds no place in line"
    );
}

#[test]
fn an_admitted_run_holds_its_slot_while_its_daemon_starts() {
    let mut old = waiting("team-a", "api-a", "2026-10-22T07:00:00Z");
    if let Some(status) = old.status.as_mut() {
        status.reason = Some(Reason::BuildkitdStarting);
        status.buildkitd_instance = Some("buildkitd-team-a-0".to_owned());
    }
    let new = waiting("team-b", "web-a", "2026-10-22T07:30:00Z");
    let runs = [old.clone(), new.clone()];
    assert!(admission(&runs, &new, 1, 1).is_err(), "cluster full");
    assert!(admission(&runs, &old, 1, 1).is_ok());
    let sibling = waiting("team-a", "api-b", "2026-10-22T07:30:00Z");
    assert!(
        admission(&[old.clone(), sibling.clone()], &sibling, 1, 5).is_err(),
        "namespace full"
    );
}

#[test]
fn a_run_that_finishes_before_its_job_gives_its_slot_back() {
    let mut old = run("api-a", A, Some(Phase::Failed), "2026-10-22T07:00:00Z");
    old.metadata.namespace = Some("team-a".to_owned());
    if let Some(status) = old.status.as_mut() {
        status.buildkitd_instance = Some("buildkitd-team-a-0".to_owned());
    }
    let new = waiting("team-b", "web-a", "2026-10-22T07:30:00Z");
    assert!(admission(&[old, new.clone()], &new, 1, 1).is_ok());
}

#[test]
fn a_run_waits_for_the_previous_one_to_stop() {
    let named = |name: &str| ObjectMeta {
        name: Some(name.to_owned()),
        labels: Some(BTreeMap::from([(
            labels::BUILD_RUN.to_owned(),
            name.to_owned(),
        )])),
        ..ObjectMeta::default()
    };
    let mut old_job = job(None);
    old_job.metadata = named("api-a");
    let mut finished_job = job(Some(("Complete", "True")));
    finished_job.metadata = named("api-a");
    let mut old_pod = pod(Vec::new(), Vec::new());
    old_pod.metadata = named("api-a");
    let mut done_pod = old_pod.clone();
    done_pod.status = Some(PodStatus {
        phase: Some("Succeeded".to_owned()),
        ..PodStatus::default()
    });
    assert_eq!(
        previous_running(&[old_job.clone()], &[], "api-b").as_deref(),
        Some("api-a")
    );
    assert_eq!(
        previous_running(&[], &[old_pod], "api-b").as_deref(),
        Some("api-a"),
        "the Job is gone but its Pod is not"
    );
    assert_eq!(previous_running(&[old_job], &[], "api-a"), None);
    assert_eq!(
        previous_running(&[finished_job], &[done_pod], "api-b"),
        None
    );
}

#[test]
fn namespace_annotations_change_one_build_and_the_count() {
    let defaults = Resources {
        cpu_request: Some("100m".to_owned()),
        memory_request: Some("256Mi".to_owned()),
        cpu_limit: Some("1".to_owned()),
        memory_limit: Some("1Gi".to_owned()),
    };
    let set = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(
        limits(&BTreeMap::new(), 1, &defaults, &Ceiling::default()),
        Limits {
            concurrent: 1,
            resources: defaults.clone(),
            clamped: Vec::new()
        }
    );
    let raised = limits(
        &set(&[
            (annotations::BUILD_CPU, "2"),
            (annotations::BUILD_MEMORY, " 4Gi "),
            (annotations::MAX_CONCURRENT_BUILDS, "2"),
        ]),
        1,
        &defaults,
        &Ceiling::default(),
    );
    assert_eq!(raised.concurrent, 2);
    assert_eq!(raised.resources.cpu_limit.as_deref(), Some("2"));
    assert_eq!(raised.resources.memory_limit.as_deref(), Some("4Gi"));
    assert_eq!(raised.resources.memory_request, defaults.memory_request);
    let bad = limits(
        &set(&[
            (annotations::BUILD_CPU, "lots"),
            (annotations::BUILD_MEMORY, "0"),
        ]),
        1,
        &defaults,
        &Ceiling::default(),
    );
    assert_eq!(bad.resources, defaults);
}

#[test]
fn a_ceiling_holds_a_namespace_to_the_cluster_limit_and_says_so() {
    let defaults = Resources {
        cpu_limit: Some("1".to_owned()),
        memory_limit: Some("1Gi".to_owned()),
        ..Resources::default()
    };
    let annotations = BTreeMap::from([
        (annotations::BUILD_CPU.to_owned(), "8".to_owned()),
        (annotations::BUILD_MEMORY.to_owned(), "2Gi".to_owned()),
    ]);
    let ceiling = Ceiling {
        cpu: Some("4".to_owned()),
        memory: Some("4096Mi".to_owned()),
    };
    let held = limits(&annotations, 1, &defaults, &ceiling);
    assert_eq!(held.resources.cpu_limit.as_deref(), Some("4"));
    assert_eq!(held.resources.memory_limit.as_deref(), Some("2Gi"));
    assert_eq!(
        held.clamped,
        ["This build asked for 8 CPU; the cluster allows at most 4 per build, so it gets 4."]
    );

    let open = limits(&annotations, 1, &defaults, &Ceiling::default());
    assert_eq!(open.resources.cpu_limit.as_deref(), Some("8"));
    assert_eq!(open.clamped, Vec::<String>::new());
}

#[test]
fn a_memory_kill_since_the_run_started_is_out_of_memory() {
    let killed = |at: &str| {
        let mut pod = pod(Vec::new(), Vec::new());
        pod.status = Some(PodStatus {
            container_statuses: Some(vec![ContainerStatus {
                name: "buildkitd".to_owned(),
                last_state: Some(ContainerState {
                    terminated: Some(ContainerStateTerminated {
                        reason: Some("OOMKilled".to_owned()),
                        finished_at: Some(Time(at.parse().unwrap())),
                        ..ContainerStateTerminated::default()
                    }),
                    ..ContainerState::default()
                }),
                ..ContainerStatus::default()
            }]),
            ..PodStatus::default()
        });
        pod
    };
    let since = Some("2026-10-22T07:00:00Z".parse().unwrap());
    assert!(oom_killed(&killed("2026-10-22T07:30:00Z"), since));
    assert!(!oom_killed(&killed("2026-10-22T06:00:00Z"), since));
    assert!(!oom_killed(&pod(Vec::new(), Vec::new()), since));
}

#[test]
fn buildkitd_idles_after_the_last_run() {
    let ten = Duration::from_mins(10);
    assert!(idle(&[], now(), ten));
    let old = run("api-a", A, Some(Phase::Succeeded), "2026-10-22T07:40:00Z");
    assert!(idle(std::slice::from_ref(&old), now(), ten));
    let recent = run("api-b", B, Some(Phase::Failed), "2026-10-22T07:55:00Z");
    assert!(!idle(&[old.clone(), recent], now(), ten));
    let waiting = run("api-c", B, Some(Phase::Pending), "2026-10-22T06:00:00Z");
    assert!(!idle(&[old.clone(), waiting], now(), ten));
    let mut new = run("api-d", B, None, "2026-10-22T07:59:59Z");
    new.status = None;
    assert!(!idle(&[old, new], now(), ten));
}

#[test]
fn image_and_tags() {
    let mut build = build();
    let run = run("api-x7k2p", A, None, "2025-10-21T15:00:00Z");
    assert_eq!(
        image(&build, "registry.local:5000/"),
        "registry.local:5000/team-a/api"
    );
    assert_eq!(tags(&build, &run).unwrap(), ["main-1761058800-3e1a9c2"]);
    set_ref(&mut build, "refs/heads/feature/x");
    build.spec.output = Some(OutputSpec {
        image: Some("ghcr.io/acme/api".to_owned()),
        extra_tags: vec!["latest".to_owned()],
        ..OutputSpec::default()
    });
    assert_eq!(image(&build, "registry.local:5000"), "ghcr.io/acme/api");
    assert_eq!(
        tags(&build, &run).unwrap(),
        ["feature-x-1761058800-3e1a9c2", "latest"]
    );
}

#[test]
fn archive_tags_use_the_digest() {
    let mut build = build();
    build.spec.source.git = None;
    build.spec.source.archive = Some(ArchiveSource {
        digest: Some(DIGEST.to_owned()),
        context_dir: None,
        dockerfile: None,
    });
    let run = run("api-x7k2p", DIGEST, None, "2025-10-21T15:00:00Z");
    assert_eq!(tags(&build, &run).unwrap(), ["archive-1761058800-0123456"]);
}

#[test]
fn default_branch_tags_and_message() {
    let mut build = build();
    set_ref(&mut build, "HEAD");
    let mut run = run("api-x7k2p", A, None, "2025-10-21T15:00:00Z");
    run.spec.git_ref = Some("refs/heads/master".to_owned());
    assert_eq!(tags(&build, &run).unwrap(), ["master-1761058800-3e1a9c2"]);
    assert_eq!(
        resolved_message("HEAD", Some("refs/heads/master"), A),
        format!("HEAD (master) is {A}")
    );
    assert_eq!(
        resolved_message("main", Some("refs/heads/main"), A),
        format!("main is {A}")
    );
    assert_eq!(
        resolved_message("refs/heads/main", Some("refs/heads/main"), A),
        format!("refs/heads/main is {A}")
    );
}

#[test]
fn summary_points_at_the_newest_success() {
    let mut ok = run("api-1", A, Some(Phase::Succeeded), "2026-10-22T01:00:00Z");
    if let Some(s) = ok.status.as_mut() {
        s.tags = vec!["main-1-3e1a9c2".to_owned()];
        s.image_digest = Some(DIGEST.to_owned());
    }
    let mut bad = run("api-2", B, Some(Phase::Failed), "2026-10-22T02:00:00Z");
    if let Some(s) = bad.status.as_mut() {
        s.reason = Some(Reason::BuildFailed);
        s.message = Some("exit code: 2".to_owned());
    }
    let mut status = BuildStatus::default();
    summarise(&mut status, &[ok.clone()], "r/team-a/api", now(), Some(1));
    assert_eq!(
        status.latest_image.as_deref(),
        Some(format!("r/team-a/api:main-1-3e1a9c2@{DIGEST}").as_str())
    );
    assert_eq!(status.conditions[0].status, ConditionStatus::True);

    summarise(&mut status, &[ok, bad], "r/team-a/api", now(), Some(1));
    assert_eq!(status.latest_revision.as_deref(), Some(A));
    assert_eq!(status.last_build_run_ref.as_deref(), Some("api-2"));
    assert_eq!(status.conditions[0].status, ConditionStatus::False);
    assert_eq!(status.conditions[0].reason, "BuildFailed");
    assert_eq!(status.conditions[0].message, "api-2: exit code: 2");
}

fn succeeded(name: &str, revision: &str, at: &str, tags: &[&str]) -> BuildRun {
    let mut run = run(name, revision, Some(Phase::Succeeded), at);
    if let Some(s) = run.status.as_mut() {
        s.tags = tags.iter().map(|t| (*t).to_owned()).collect();
        s.image_digest = Some(DIGEST.to_owned());
    }
    run
}

#[test]
fn the_newest_success_is_notified_once() {
    let mut build = build();
    let old = succeeded("api-1", A, "2026-10-22T01:00:00Z", &["main-1-3e1a9c2"]);
    let mut new = succeeded("api-2", B, "2026-10-22T02:00:00Z", &["main-2-0123456"]);
    let failed = run("api-3", A, Some(Phase::Failed), "2026-10-22T03:00:00Z");
    let runs = [old.clone(), new.clone(), failed.clone()];
    assert!(notify_due(&build, &runs).is_none(), "no targets");

    build.spec.notify = vec![NotifyTarget {
        kind: NotifyType::Keel,
        url: "http://keel.keel:9300/v1/webhooks/native".to_owned(),
        secret_ref: None,
    }];
    assert_eq!(
        notify_due(&build, &runs).and_then(|r| r.metadata.name.as_deref()),
        Some("api-2")
    );
    if let Some(s) = new.status.as_mut() {
        s.notified_at = Some(now().to_string());
    }
    assert!(notify_due(&build, &[old, new, failed]).is_none());
}

/// An older commit's run that finishes after a newer commit's run (a `Queue` run, a `BuildRun`
/// made by hand, or a cancellation that lost the race with the Job) keeps the newer image.
#[test]
fn an_older_commit_finishing_late_does_not_replace_the_newer_image() {
    let mut build = build();
    build.spec.notify = vec![NotifyTarget {
        kind: NotifyType::Keel,
        url: "http://keel.keel:9300/v1/webhooks/native".to_owned(),
        secret_ref: None,
    }];
    let mut older = succeeded("api-1", A, "2026-10-22T01:00:00Z", &["main-1-3e1a9c2"]);
    if let Some(s) = older.status.as_mut() {
        s.completion_time = Some("2026-10-22T03:00:00Z".to_owned());
    }
    let mut newer = succeeded("api-2", B, "2026-10-22T02:00:00Z", &["main-2-0123456"]);
    if let Some(s) = newer.status.as_mut() {
        s.notified_at = Some("2026-10-22T02:10:00Z".to_owned());
    }
    let runs = [older, newer];

    let mut status = BuildStatus::default();
    summarise(&mut status, &runs, "r/team-a/api", now(), Some(1));
    assert_eq!(status.latest_revision.as_deref(), Some(B));
    assert_eq!(
        status.latest_image.as_deref(),
        Some(format!("r/team-a/api:main-2-0123456@{DIGEST}").as_str())
    );
    assert!(
        notify_due(&build, &runs).is_none(),
        "Keel is not told of the older image"
    );
    assert!(
        next_run(&build_with_status(&status), B, &runs, Trigger::Poll)
            .create
            .is_none()
    );
    assert_eq!(prune(&runs, 0), ["api-1"]);
}

fn build_with_status(status: &BuildStatus) -> Build {
    let mut build = build();
    build.status = Some(status.clone());
    build
}

#[test]
fn keel_gets_the_repository_and_the_template_tag() {
    let run = succeeded(
        "api-1",
        A,
        "2026-10-22T01:00:00Z",
        &["main-1-3e1a9c2", "latest"],
    );
    let payload = keel_payload("registry.local:5000/team-a/api", &run).unwrap();
    assert_eq!(
        serde_json::to_value(payload).unwrap(),
        serde_json::json!({"name": "registry.local:5000/team-a/api", "tag": "main-1-3e1a9c2"})
    );
    let untagged = succeeded("api-2", A, "2026-10-22T01:00:00Z", &[]);
    assert_eq!(keel_payload("r/team-a/api", &untagged), None);
}

fn job(condition: Option<(&str, &str)>) -> Job {
    Job {
        status: Some(JobStatus {
            conditions: condition.map(|(kind, reason)| {
                vec![JobCondition {
                    type_: kind.to_owned(),
                    status: "True".to_owned(),
                    reason: Some(reason.to_owned()),
                    ..JobCondition::default()
                }]
            }),
            ..JobStatus::default()
        }),
        ..Job::default()
    }
}

fn container(name: &str, state: ContainerState) -> ContainerStatus {
    ContainerStatus {
        name: name.to_owned(),
        state: Some(state),
        ..ContainerStatus::default()
    }
}

fn terminated(code: i32, message: Option<&str>) -> ContainerState {
    ContainerState {
        terminated: Some(ContainerStateTerminated {
            exit_code: code,
            message: message.map(str::to_owned),
            reason: Some(if code == 0 { "Completed" } else { "Error" }.to_owned()),
            ..ContainerStateTerminated::default()
        }),
        ..ContainerState::default()
    }
}

fn running() -> ContainerState {
    ContainerState {
        running: Some(ContainerStateRunning::default()),
        ..ContainerState::default()
    }
}

fn pod(init: Vec<ContainerStatus>, main: Vec<ContainerStatus>) -> Pod {
    Pod {
        status: Some(PodStatus {
            init_container_statuses: Some(init),
            container_statuses: Some(main),
            ..PodStatus::default()
        }),
        ..Pod::default()
    }
}

#[test]
fn phases_follow_the_pod() {
    let none = job(None);
    assert_eq!(observe(&none, None), Observed::Running(Phase::Pending));
    let fetching = pod(vec![container("fetch", running())], vec![]);
    assert_eq!(
        observe(&none, Some(&fetching)),
        Observed::Running(Phase::Planning)
    );
    let building = pod(
        vec![container("fetch", terminated(0, None))],
        vec![container("build", running())],
    );
    assert_eq!(
        observe(&none, Some(&building)),
        Observed::Running(Phase::Building)
    );
}

#[test]
fn exposed_ports_reach_the_build_status_from_the_newest_success() {
    let message = format!(
        r#"{{"result":"succeeded","digest":"{DIGEST}","tags":["main-1-3e1a9c2"],"exposedPorts":["3000/tcp","53/udp"]}}"#
    );
    let done = pod(
        vec![container("fetch", terminated(0, None))],
        vec![container("build", terminated(0, Some(&message)))],
    );
    let Observed::Finished(run_status) = observe(&job(Some(("Complete", ""))), Some(&done)) else {
        panic!("finished expected")
    };
    assert_eq!(run_status.exposed_ports, ["3000/tcp", "53/udp"]);

    let mut ok = run("api-1", A, Some(Phase::Succeeded), "2026-10-22T01:00:00Z");
    ok.status = Some(*run_status);
    let mut portless = run("api-2", B, Some(Phase::Succeeded), "2026-10-22T02:00:00Z");
    if let Some(s) = portless.status.as_mut() {
        s.tags = vec!["main-2-aaaaaaa".to_owned()];
        s.image_digest = Some(DIGEST.to_owned());
    }
    let mut status = BuildStatus::default();
    summarise(&mut status, &[ok.clone()], "r/team-a/api", now(), Some(1));
    assert_eq!(status.exposed_ports, ["3000/tcp", "53/udp"]);

    summarise(&mut status, &[ok, portless], "r/team-a/api", now(), Some(1));
    assert_eq!(status.exposed_ports, Vec::<String>::new());
}

#[test]
fn success_carries_the_digest() {
    let message = format!(
        r#"{{"result":"succeeded","digest":"{DIGEST}","tags":["main-1-3e1a9c2"],"phases":{{"fetch":1.0,"build":12.34}},"cache":{{"total":5,"cached":3}}}}"#
    );
    let done = pod(
        vec![container("fetch", terminated(0, None))],
        vec![container("build", terminated(0, Some(&message)))],
    );
    let Observed::Finished(status) = observe(&job(Some(("Complete", ""))), Some(&done)) else {
        panic!("finished expected")
    };
    assert_eq!(status.phase, Some(Phase::Succeeded));
    assert_eq!(status.image_digest.as_deref(), Some(DIGEST));
    assert_eq!(status.tags, ["main-1-3e1a9c2"]);
    assert_eq!(status.phase_durations["build"], "12.3s");
    assert_eq!(
        status.cache_stats,
        Some(CacheStats {
            total: 5,
            cached: 3
        })
    );
}

#[test]
fn failures_name_their_phase() {
    let failed_job = job(Some(("Failed", "BackoffLimitExceeded")));
    let fetch = pod(
        vec![container(
            "fetch",
            terminated(128, Some("fatal: couldn't find remote ref")),
        )],
        vec![],
    );
    let Observed::Finished(status) = observe(&failed_job, Some(&fetch)) else {
        panic!()
    };
    assert_eq!(status.reason, Some(Reason::SourceFailed));

    let plan = pod(
        vec![
            container("fetch", terminated(0, None)),
            container("plan", terminated(1, None)),
        ],
        vec![],
    );
    let Observed::Finished(status) = observe(&failed_job, Some(&plan)) else {
        panic!()
    };
    assert_eq!(status.reason, Some(Reason::PlanFailed));
    assert_eq!(status.message.as_deref(), Some("Error"));

    let build = pod(
        vec![container("fetch", terminated(0, None))],
        vec![container(
            "build",
            terminated(
                1,
                Some(r#"{"result":"failed","reason":"BuildFailed","message":"denied"}"#),
            ),
        )],
    );
    let Observed::Finished(status) = observe(&failed_job, Some(&build)) else {
        panic!()
    };
    assert_eq!(status.reason, Some(Reason::BuildFailed));
    assert_eq!(status.message.as_deref(), Some("denied"));

    let Observed::Finished(status) =
        observe(&job(Some(("Failed", "DeadlineExceeded"))), Some(&build))
    else {
        panic!()
    };
    assert_eq!(status.reason, Some(Reason::Timeout));

    let Observed::Finished(status) = observe(&failed_job, None) else {
        panic!()
    };
    assert_eq!(status.reason, Some(Reason::InfraError));
}

#[test]
fn a_job_is_used_once_its_run_records_it() {
    let mut job = job(None);
    job.metadata.name = Some("app-x1".to_owned());
    job.metadata.creation_timestamp = Some(Time(now()));
    let unrecorded = BuildRunStatus::default();
    assert_eq!(recorded(unrecorded.clone(), &job, now()), None);

    let started = BuildRunStatus {
        job_ref: Some("app-x1".to_owned()),
        buildkitd_instance: Some("buildkitd-0".to_owned()),
        ..BuildRunStatus::default()
    };
    assert_eq!(recorded(started.clone(), &job, now()), Some(started));

    let later = now() + START_GRACE;
    let adopted = recorded(unrecorded, &job, later).unwrap();
    assert_eq!(adopted.job_ref.as_deref(), Some("app-x1"));
    assert_eq!(adopted.start_time, Some(now().to_string()));
}

#[test]
fn a_run_name_follows_the_build_the_commit_and_the_rebuild_value() {
    let mut build = build();
    build.metadata.uid = Some("uid-1".to_owned());
    let name = run_name(&build, A);
    assert_eq!(name, run_name(&build, A), "two replicas name the same run");
    assert!(
        name.starts_with("api-") && name.len() == "api-".len() + 5,
        "{name}"
    );
    assert_ne!(name, run_name(&build, B), "a new commit");
    let mut rebuilt = build.clone();
    rebuilt.metadata.annotations = Some(BTreeMap::from([(
        annotations::REBUILD.to_owned(),
        "1".to_owned(),
    )]));
    assert_ne!(name, run_name(&rebuilt, A), "a rebuild");
    let mut recreated = build;
    recreated.metadata.uid = Some("uid-2".to_owned());
    assert_ne!(name, run_name(&recreated, A), "a Build created again");
}

#[test]
fn a_ref_that_does_not_resolve_is_asked_less_often() {
    let minute = Duration::from_mins(1);
    let failing_since = |since: &str| {
        let mut status = BuildStatus::default();
        set_condition(
            &mut status.conditions,
            "SourceReady",
            ConditionStatus::False,
            ConditionReason::ResolveFailed,
            "",
            since.parse().unwrap(),
            None,
        );
        status
    };
    let cases = [
        ("resolves", BuildStatus::default(), minute, minute),
        (
            "failing for 3m",
            failing_since("2026-10-22T07:57:00Z"),
            minute,
            Duration::from_mins(3),
        ),
        (
            "failing for 2h",
            failing_since("2026-10-22T06:00:00Z"),
            minute,
            MAX_BACKOFF,
        ),
        (
            "an interval above the cap",
            failing_since("2026-10-22T06:00:00Z"),
            Duration::from_mins(30),
            Duration::from_mins(30),
        ),
    ];
    for (case, status, interval, expected) in cases {
        assert_eq!(next_poll(&status, interval, now()), expected, "{case}");
    }
}

#[test]
fn a_build_owns_its_default_image_and_archives() {
    let mut build = build();
    let found = owned(&build, "zot.zot.svc:5000/").unwrap();
    assert_eq!(
        found,
        Owned {
            host: "zot.zot.svc:5000".to_owned(),
            image: Some("team-a/api".to_owned()),
            source: "team-a/api/source".to_owned(),
        }
    );
    build.spec.output = Some(OutputSpec {
        image: Some("zot.zot.svc:5000/team-a/api:v1".to_owned()),
        ..OutputSpec::default()
    });
    assert_eq!(
        owned(&build, "zot.zot.svc:5000").unwrap().image.as_deref(),
        Some("team-a/api")
    );
}

#[test]
fn a_build_cannot_claim_another_repository_on_the_registry() {
    let mut build = build();
    for image in [
        "zot.zot.svc:5000/team-b/api",
        "zot.zot.svc:5000/team/api",
        "zot.zot.svc:5000/team-a/api-x",
        "zot.zot.svc:5000/team-a/api/source",
        "zot.zot.svc:5000/team-a/other",
    ] {
        build.spec.output = Some(OutputSpec {
            image: Some(image.to_owned()),
            ..OutputSpec::default()
        });
        assert!(owned(&build, "zot.zot.svc:5000").is_err(), "{image}");
    }
}

#[test]
fn an_image_elsewhere_is_not_owned() {
    let mut build = build();
    build.spec.output = Some(OutputSpec {
        image: Some("ghcr.io/acme/api".to_owned()),
        ..OutputSpec::default()
    });
    let found = owned(&build, "registry.local:5000/builds").unwrap();
    assert_eq!(found.image, None);
    assert_eq!(found.source, "builds/team-a/api/source");
}

#[test]
fn a_deleted_run_is_not_created_again() {
    let mut build = build();
    build.status = Some(BuildStatus {
        last_run_revision: Some(B.to_owned()),
        ..BuildStatus::default()
    });
    assert_eq!(next_run(&build, B, &[], Trigger::Poll), NextRun::default());
    assert_eq!(
        next_run(&build, A, &[], Trigger::Poll).create,
        Some(Trigger::Poll)
    );
    build
        .metadata
        .annotations
        .get_or_insert_default()
        .insert(annotations::REBUILD.to_owned(), "1".to_owned());
    assert_eq!(
        next_run(&build, B, &[], Trigger::Poll).create,
        Some(Trigger::Manual),
        "a rebuild still builds it"
    );
}

#[test]
fn tokens_count_as_expired_once_the_registry_clock_has_passed_them_too() {
    let until: Timestamp = "2026-11-01T10:20:00Z".parse().unwrap();
    let at = |t: &str| t.parse::<Timestamp>().unwrap();
    assert!(!tokens_expired(until, at("2026-11-01T10:20:30Z")));
    assert!(!tokens_expired(until, at("2026-11-01T10:21:00Z")));
    assert!(tokens_expired(until, at("2026-11-01T10:21:01Z")));
}

#[test]
fn a_missing_builder_and_a_found_one_set_builder_ready() {
    let (status, reason, message) = builder_ready(true, "dockerfile");
    assert_eq!(status, ConditionStatus::True);
    assert_eq!(reason.to_string(), "Found");
    assert_eq!(message, "");

    let (status, reason, message) = builder_ready(false, "railpack");
    assert_eq!(status, ConditionStatus::False);
    assert_eq!(reason.to_string(), "BuilderNotFound");
    assert_eq!(message, "no ClusterBuilder `railpack`");
}

#[test]
fn an_archive_source_sets_source_ready_by_its_digest() {
    let archive = |digest: Option<&str>| ArchiveSource {
        digest: digest.map(str::to_owned),
        context_dir: None,
        dockerfile: None,
    };
    let reason = |archive: Option<&ArchiveSource>| {
        let (status, reason, _, digest) = archive_source(archive);
        (status, reason.to_string(), digest)
    };
    assert_eq!(
        reason(None),
        (ConditionStatus::False, "NoSource".to_owned(), None)
    );
    assert_eq!(
        reason(Some(&archive(None))),
        (ConditionStatus::False, "WaitingForUpload".to_owned(), None)
    );
    assert_eq!(
        reason(Some(&archive(Some(DIGEST)))),
        (
            ConditionStatus::True,
            "Archive".to_owned(),
            Some(DIGEST.to_owned())
        )
    );
}

#[test]
fn ready_names_the_outcome_of_the_latest_finished_run() {
    let ready = |runs: &[BuildRun]| {
        let mut status = BuildStatus::default();
        summarise(&mut status, runs, "r/team-a/api", now(), Some(1));
        status.conditions[0].reason.clone()
    };
    assert_eq!(ready(&[]), "NotBuilt");

    let ok = run("api-1", A, Some(Phase::Succeeded), "2026-10-22T01:00:00Z");
    assert_eq!(ready(&[ok]), "Succeeded");

    let mut failed = run("api-2", A, Some(Phase::Failed), "2026-10-22T02:00:00Z");
    assert_eq!(ready(&[failed.clone()]), "Failed");

    for (reason, written) in [
        (Reason::Timeout, "Timeout"),
        (
            Reason::RegistryCredentialsMissing,
            "RegistryCredentialsMissing",
        ),
        (Reason::Unknown, "Unknown"),
    ] {
        failed.status.as_mut().unwrap().reason = Some(reason);
        assert_eq!(ready(&[failed.clone()]), written);
    }
}
