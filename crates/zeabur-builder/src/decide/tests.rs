use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::{JobCondition, JobStatus};
use k8s_openapi::api::core::v1::{
    ContainerState, ContainerStateRunning, ContainerStateTerminated, PodCondition, PodStatus,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{OwnerReference, Time};
use zeabur_builder_api::build::{
    ArchiveSource, BuildSpec, GitHubAppSource, GitSource, NotifyTarget, NotifyType, OutputSpec,
    SourceKind, SourceSpec, TriggerSpec,
};
use zeabur_builder_api::build_run::BuildRunSpec;

use super::*;

const A: &str = "3e1a9c2d4f0b5a6c7d8e9f0a1b2c3d4e5f6a7b8c";
const B: &str = "0123456789abcdef0123456789abcdef01234567";
const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const PLAN: &str = "sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

fn git(build: &mut Build) -> &mut GitSource {
    match &mut build.spec.source.kind {
        SourceKind::Git { git } => git,
        SourceKind::Archive { .. } => panic!("an archive source"),
    }
}

fn set_ref(build: &mut Build, git_ref: &str) {
    git(build).git_ref = git_ref.to_owned();
}

fn ts(at: &str) -> Timestamp {
    at.parse().unwrap()
}

fn now() -> Timestamp {
    "2026-10-22T08:00:00Z".parse().unwrap()
}

fn build() -> Build {
    let mut build = Build::new(
        "api",
        BuildSpec {
            source: SourceSpec {
                kind: SourceKind::Git {
                    git: GitSource {
                        url: "https://github.com/acme/api".to_owned(),
                        git_ref: "main".to_owned(),
                        credentials_ref: None,
                        github_app: None,
                    },
                },
                context_dir: None,
                dockerfile: None,
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
    build.metadata.generation = Some(1);
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
    run.metadata.annotations = Some(BTreeMap::from([(
        annotations::BUILD_GENERATION.to_owned(),
        "1".to_owned(),
    )]));
    run.status = Some(BuildRunStatus {
        phase,
        completion_time: is_finished(phase).then(|| ts(at)),
        ..admitted(BuildRunStatus::default())
    });
    run
}

/// `status` with what admission records: the builder and the repository.
fn admitted(status: BuildRunStatus) -> BuildRunStatus {
    BuildRunStatus {
        builder: Some("railpack".to_owned()),
        image: Some("r/team-a/api".to_owned()),
        ..status
    }
}

#[test]
fn durations() {
    for (text, want) in [
        ("90s", Duration::from_secs(90)),
        ("1m30s", Duration::from_secs(90)),
        ("1h30m", Duration::from_mins(90)),
        ("1h1m1s", Duration::from_secs(3661)),
        ("1.5h", Duration::from_mins(90)),
        ("0.5s", Duration::from_millis(500)),
        ("250ms", Duration::from_millis(250)),
        ("1s250ms", Duration::from_millis(1250)),
        ("0s", Duration::ZERO),
    ] {
        assert_eq!(parse_duration(text), Some(want), "{text}");
    }
}

#[test]
fn text_that_is_not_a_duration_is_refused() {
    for text in ["", "10", "m", "1d", "-1s", "1x", "1.5m30s", "30s1m", "1s2s"] {
        assert_eq!(parse_duration(text), None, "{text}");
    }
}

#[test]
fn the_crd_admits_the_durations_the_controller_reads() {
    let pattern = regex::Regex::new(zeabur_builder_api::common::DURATION_PATTERN).unwrap();
    for (text, read) in [
        ("90s", true),
        ("1m30s", true),
        ("1h30m", true),
        ("1h1m1s", true),
        ("1.5h", true),
        ("1.5m", true),
        ("0.5s", true),
        ("250ms", true),
        ("1.5ms", true),
        ("1s250ms", true),
        ("0s", true),
        ("", false),
        ("10", false),
        ("m", false),
        ("1d", false),
        ("-1s", false),
        ("1x", false),
        ("1.5m30s", false),
        ("1.5h30m", false),
        ("30s1m", false),
        ("1s2s", false),
        ("1ms1s", false),
        ("1.s", false),
        ("1.5555s", false),
        ("1234567s", false),
    ] {
        assert_eq!(pattern.is_match(text), read, "pattern: {text}");
        if read {
            assert!(parse_duration(text).is_some(), "parser: {text}");
        }
    }
}

#[test]
fn the_crd_admits_quantities_and_refuses_words() {
    let pattern = regex::Regex::new(zeabur_builder_api::common::QUANTITY_PATTERN).unwrap();
    for text in ["2Gi", "500m", "1", "0.5", "1.5Gi", "1e3", "128M"] {
        assert!(pattern.is_match(text), "{text}");
    }
    for text in ["lots", "", "2 Gi", "Gi", "2GB", "1.5.5"] {
        assert!(!pattern.is_match(text), "{text}");
    }
}

#[test]
fn conditions_keep_their_transition_time() {
    let mut conditions = Vec::new();
    let earlier: Timestamp = "2026-10-22T07:00:00Z".parse().unwrap();
    set_condition(
        &mut conditions,
        ConditionType::Ready,
        ConditionStatus::True,
        ConditionReason::Succeeded,
        "",
        earlier,
        Some(1),
    );
    set_condition(
        &mut conditions,
        ConditionType::Ready,
        ConditionStatus::True,
        ConditionReason::Succeeded,
        "again",
        now(),
        Some(2),
    );
    assert_eq!(conditions.len(), 1);
    assert_eq!(conditions[0].last_transition_time, earlier);
    assert_eq!(conditions[0].message, "again");
    set_condition(
        &mut conditions,
        ConditionType::Ready,
        ConditionStatus::False,
        Reason::BuildFailed,
        "",
        now(),
        Some(2),
    );
    assert_eq!(conditions[0].last_transition_time, now());
}

#[test]
fn resolves_once_per_interval_or_new_generation() {
    let status = BuildStatus {
        remote_revision: Some(A.to_owned()),
        last_resolved_at: Some(ts("2026-10-22T07:59:30Z")),
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
fn a_spec_edit_builds_the_same_commit_again() {
    let mut build = build();
    build.status = Some(BuildStatus {
        last_run_revision: Some(B.to_owned()),
        last_run_generation: Some(1),
        ..BuildStatus::default()
    });
    let runs = [run(
        "api-1",
        B,
        Some(Phase::Succeeded),
        "2026-10-22T07:00:00Z",
    )];
    assert_eq!(
        next_run(&build, B, &runs, Trigger::Poll),
        NextRun::default()
    );
    build.metadata.generation = Some(2);
    assert_eq!(
        next_run(&build, B, &runs, Trigger::Poll).create,
        Some(Trigger::Manual)
    );
    build.status.as_mut().unwrap().last_run_generation = Some(2);
    assert_eq!(
        next_run(&build, B, &[], Trigger::Poll),
        NextRun::default(),
        "deleting the run of the new generation does not build it again"
    );
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
        last_webhook_at: hook.map(ts),
        last_resolved_at: resolved.map(ts),
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
    git(&mut build).github_app = Some(GitHubAppSource { installation_id: 1 });
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
        limits(
            &BTreeMap::new(),
            1,
            &defaults,
            &job_memory(),
            &Ceiling::default()
        ),
        Limits {
            concurrent: 1,
            resources: defaults.clone(),
            job_memory: job_memory(),
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
        &job_memory(),
        &Ceiling::default(),
    );
    assert_eq!(raised.concurrent, 2);
    assert_eq!(raised.resources.cpu_limit.as_deref(), Some("2"));
    assert_eq!(raised.resources.memory_limit.as_deref(), Some("4Gi"));
    assert_eq!(raised.resources.memory_request, defaults.memory_request);
    assert_eq!(raised.job_memory.limit, "4Gi");
    assert_eq!(raised.job_memory.request, job_memory().request);
    let bad = limits(
        &set(&[
            (annotations::BUILD_CPU, "lots"),
            (annotations::BUILD_MEMORY, "0"),
        ]),
        1,
        &defaults,
        &job_memory(),
        &Ceiling::default(),
    );
    assert_eq!(bad.resources, defaults);
    assert_eq!(bad.job_memory, job_memory());
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
    let held = limits(&annotations, 1, &defaults, &job_memory(), &ceiling);
    assert_eq!(held.resources.cpu_limit.as_deref(), Some("4"));
    assert_eq!(held.resources.memory_limit.as_deref(), Some("2Gi"));
    assert_eq!(
        held.clamped,
        ["This build asked for 8 CPU; the cluster allows at most 4 per build, so it gets 4."]
    );

    let open = limits(
        &annotations,
        1,
        &defaults,
        &job_memory(),
        &Ceiling::default(),
    );
    assert_eq!(open.resources.cpu_limit.as_deref(), Some("8"));
    assert_eq!(open.clamped, Vec::<String>::new());
}

fn cluster_builder(generation: i64) -> ClusterBuilder {
    use zeabur_builder_api::cluster_builder::{
        ClusterBuilderSpec, Frontend, FrontendSpec, Planner,
    };
    let mut builder = ClusterBuilder::new(
        "dockerfile",
        ClusterBuilderSpec {
            planner: Planner::Dockerfile,
            frontend: FrontendSpec {
                kind: Frontend::Dockerfile,
                opts: BTreeMap::new(),
            },
        },
    );
    builder.metadata.generation = Some(generation);
    builder
}

fn job_memory() -> JobMemory {
    JobMemory {
        request: "128Mi".to_owned(),
        limit: "512Mi".to_owned(),
    }
}

fn flags() -> Resources {
    Resources {
        cpu_request: Some("100m".to_owned()),
        memory_request: Some("256Mi".to_owned()),
        cpu_limit: Some("1".to_owned()),
        memory_limit: Some("1Gi".to_owned()),
    }
}

fn assert_status_is(status: &ClusterBuilderStatus, expected: impl Serialize) {
    let status = ClusterBuilderStatus {
        conditions: Vec::new(),
        ..status.clone()
    };
    assert_eq!(
        serde_json::to_value(status).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

#[test]
fn a_cluster_builder_status_shows_the_flags_and_no_ceiling_when_none_is_set() {
    let status =
        cluster_builder_status(&cluster_builder(1), &flags(), &Ceiling::default(), 3, now());
    // No ceiling is a missing field, never a zero.
    assert_status_is(
        &status,
        serde_json::json!({
            "buildDefaults": {
                "requests": {"cpu": "100m", "memory": "256Mi"},
                "limits": {"cpu": "1", "memory": "1Gi"},
            },
            "defaultConcurrentBuilds": 3,
        }),
    );
    let ready = &status.conditions[0];
    assert_eq!(
        (ready.kind, ready.status, ready.reason.as_str()),
        (ConditionType::Ready, ConditionStatus::True, "Published")
    );
    assert_eq!(ready.observed_generation, Some(1));

    let unset = Resources {
        cpu_limit: None,
        ..flags()
    };
    let status = cluster_builder_status(&cluster_builder(1), &unset, &Ceiling::default(), 3, now());
    assert_status_is(
        &status,
        serde_json::json!({
            "buildDefaults": {
                "requests": {"cpu": "100m", "memory": "256Mi"},
                "limits": {"memory": "1Gi"},
            },
            "defaultConcurrentBuilds": 3,
        }),
    );
    let none = Resources {
        cpu_limit: None,
        memory_limit: None,
        ..flags()
    };
    let status = cluster_builder_status(&cluster_builder(1), &none, &Ceiling::default(), 3, now());
    assert_status_is(
        &status,
        serde_json::json!({
            "buildDefaults": {"requests": {"cpu": "100m", "memory": "256Mi"}},
            "defaultConcurrentBuilds": 3,
        }),
    );
}

#[test]
fn a_cluster_builder_status_shows_the_ceiling_namespaces_are_held_to() {
    let ceiling = Ceiling {
        cpu: None,
        memory: Some("4Gi".to_owned()),
    };
    let status = cluster_builder_status(&cluster_builder(1), &flags(), &ceiling, 1, now());
    assert_status_is(
        &status,
        serde_json::json!({
            "buildDefaults": {
                "requests": {"cpu": "100m", "memory": "256Mi"},
                "limits": {"cpu": "1", "memory": "1Gi"},
            },
            "buildCeiling": {"memory": "4Gi"},
            "defaultConcurrentBuilds": 1,
        }),
    );
    // A namespace that asks for more gets the ceiling the status shows.
    let annotations = BTreeMap::from([(annotations::BUILD_MEMORY.to_owned(), "16Gi".to_owned())]);
    let held = limits(&annotations, 1, &flags(), &job_memory(), &ceiling);
    assert_eq!(held.job_memory.limit, "4Gi");
    assert_eq!(
        held.resources.memory_limit.map(Quantity),
        status.build_ceiling.and_then(|c| c.memory)
    );
}

#[test]
fn a_cluster_builder_status_written_again_is_the_same() {
    let mut builder = cluster_builder(1);
    let first = cluster_builder_status(&builder, &flags(), &Ceiling::default(), 1, now());
    builder.status = Some(first.clone());
    let later: Timestamp = "2026-10-22T09:00:00Z".parse().unwrap();
    // Unchanged, so the controller writes nothing.
    assert_eq!(
        cluster_builder_status(&builder, &flags(), &Ceiling::default(), 1, later),
        first
    );

    // A new spec: the condition says so and keeps its transition time.
    builder.metadata.generation = Some(2);
    let changed = cluster_builder_status(&builder, &flags(), &Ceiling::default(), 1, later);
    assert_eq!(changed.conditions[0].observed_generation, Some(2));
    assert_eq!(
        changed.conditions[0].last_transition_time,
        first.conditions[0].last_transition_time
    );
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
    let mut plan = killed("2026-10-22T07:30:00Z");
    let status = plan.status.as_mut().unwrap();
    status.init_container_statuses = status.container_statuses.take();
    assert!(oom_killed(&plan, since), "a build Job's plan container");
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
    build.spec.source.kind = SourceKind::Archive {
        archive: ArchiveSource {
            digest: Some(DIGEST.to_owned()),
        },
    };
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
    summarise(&mut status, &[ok.clone()], now(), Some(1));
    assert_eq!(
        status.latest_image.as_deref(),
        Some(format!("r/team-a/api:main-1-3e1a9c2@{DIGEST}").as_str())
    );
    assert_eq!(status.conditions[0].status, ConditionStatus::True);

    summarise(&mut status, &[ok, bad], now(), Some(1));
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
        s.notified_at = Some(now());
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
        s.completion_time = Some(ts("2026-10-22T03:00:00Z"));
    }
    let mut newer = succeeded("api-2", B, "2026-10-22T02:00:00Z", &["main-2-0123456"]);
    if let Some(s) = newer.status.as_mut() {
        s.notified_at = Some(ts("2026-10-22T02:10:00Z"));
    }
    let runs = [older, newer];

    let mut status = BuildStatus::default();
    summarise(&mut status, &runs, now(), Some(1));
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
    let message = serde_json::json!({
        "result": "succeeded",
        "digest": DIGEST,
        "tags": ["main-1-3e1a9c2"],
        "exposedPorts": ["3000/tcp", "53/udp"],
    })
    .to_string();
    let done = pod(
        vec![container("fetch", terminated(0, None))],
        vec![container("build", terminated(0, Some(&message)))],
    );
    let Observed::Finished(run_status) = observe(&job(Some(("Complete", ""))), Some(&done)) else {
        panic!("finished expected")
    };
    assert_eq!(run_status.exposed_ports, ["3000/tcp", "53/udp"]);

    let mut ok = run("api-1", A, Some(Phase::Succeeded), "2026-10-22T01:00:00Z");
    ok.status = Some(admitted(*run_status));
    let mut portless = run("api-2", B, Some(Phase::Succeeded), "2026-10-22T02:00:00Z");
    if let Some(s) = portless.status.as_mut() {
        s.tags = vec!["main-2-aaaaaaa".to_owned()];
        s.image_digest = Some(DIGEST.to_owned());
    }
    let mut status = BuildStatus::default();
    summarise(&mut status, &[ok.clone()], now(), Some(1));
    assert_eq!(status.exposed_ports, ["3000/tcp", "53/udp"]);

    summarise(&mut status, &[ok, portless], now(), Some(1));
    assert_eq!(status.exposed_ports, Vec::<String>::new());
}

#[test]
fn the_plan_a_run_saved_stays_until_a_newer_one() {
    let message = serde_json::json!({
        "result": "succeeded",
        "digest": DIGEST,
        "tags": ["main-1-3e1a9c2"],
        "planDigest": PLAN,
    })
    .to_string();
    let done = pod(
        vec![container("fetch", terminated(0, None))],
        vec![container("build", terminated(0, Some(&message)))],
    );
    let Observed::Finished(run_status) = observe(&job(Some(("Complete", ""))), Some(&done)) else {
        panic!("finished expected")
    };
    let mut planned = run("api-1", A, Some(Phase::Succeeded), "2026-10-22T01:00:00Z");
    planned.status = Some(admitted(*run_status));
    let mut replayed = run("api-2", B, Some(Phase::Succeeded), "2026-10-22T02:00:00Z");
    if let Some(s) = replayed.status.as_mut() {
        s.tags = vec!["main-2-aaaaaaa".to_owned()];
        s.image_digest = Some(DIGEST.to_owned());
    }

    let mut status = BuildStatus::default();
    summarise(&mut status, &[planned], now(), Some(1));
    let recorded = Some(SavedPlan {
        image: format!("r/team-a/api@{PLAN}"),
        builder: "railpack".to_owned(),
    });
    assert_eq!(status.plan, recorded);
    assert_eq!(
        status.latest_image.as_deref(),
        Some(format!("r/team-a/api:main-1-3e1a9c2@{DIGEST}").as_str())
    );

    summarise(&mut status, &[replayed], now(), Some(1));
    assert_eq!(status.plan, recorded);
}

/// The Job a run's status comes from: built by `builder`, pushing to `image`, restoring
/// `saved_plan` when there is one.
fn job_of(builder: &str, image: &str, saved_plan: Option<&str>) -> Job {
    let mut annotations = BTreeMap::from([(annotations::IMAGE.to_owned(), image.to_owned())]);
    if let Some(digest) = saved_plan {
        annotations.insert(annotations::SAVED_PLAN.to_owned(), digest.to_owned());
    }
    Job {
        metadata: ObjectMeta {
            labels: Some(BTreeMap::from([(
                labels::BUILDER.to_owned(),
                builder.to_owned(),
            )])),
            annotations: Some(annotations),
            ..ObjectMeta::default()
        },
        ..job(Some(("Complete", "")))
    }
}

/// A `Build` that moves to another builder or repository after a run planned keeps the plan
/// named for the builder and repository of that run's Job, so the next run does not reuse it.
/// The Job is read when the run finishes, so a Job adopted after a restart counts too.
#[test]
fn a_saved_plan_names_the_builder_and_repository_of_its_job() {
    let mut planned = run("api-1", A, Some(Phase::Succeeded), "2026-10-22T01:00:00Z");
    let finished = BuildRunStatus {
        phase: Some(Phase::Succeeded),
        plan_source: Some(PlanSource::Generated),
        plan_digest: Some(PLAN.to_owned()),
        ..BuildRunStatus::default()
    };
    planned.status = Some(from_job(finished, &job_of("agent", "r/team-a/old", None)));
    let mut status = BuildStatus::default();
    summarise(&mut status, &[planned], now(), Some(1));
    let saved = SavedPlan {
        image: format!("r/team-a/old@{PLAN}"),
        builder: "agent".to_owned(),
    };
    assert_eq!(status.plan, Some(saved));

    let mut build = build();
    build.spec.builder_ref = "agent".to_owned();
    build.status = Some(status);
    assert_eq!(reused_plan(&build, "r/team-a/old").as_deref(), Some(PLAN));
    assert_eq!(reused_plan(&build, "r/team-a/api"), None);
    build.spec.builder_ref = "railpack".to_owned();
    assert_eq!(reused_plan(&build, "r/team-a/old"), None);
}

#[test]
fn a_run_reuses_the_saved_plan_of_the_same_builder_unless_asked_to_plan_again() {
    let mut build = build();
    let image = "r/team-a/api";
    assert_eq!(reused_plan(&build, image), None);

    build.status = Some(BuildStatus {
        plan: Some(SavedPlan {
            image: format!("r/team-a/api@{PLAN}"),
            builder: "dockerfile".to_owned(),
        }),
        ..BuildStatus::default()
    });
    assert_eq!(reused_plan(&build, image).as_deref(), Some(PLAN));

    build.metadata.annotations = Some(BTreeMap::from([(
        annotations::PLAN.to_owned(),
        annotations::PLAN_FRESH.to_owned(),
    )]));
    assert_eq!(reused_plan(&build, image), None);

    build.metadata.annotations = None;
    build.spec.builder_ref = "agent".to_owned();
    assert_eq!(reused_plan(&build, image), None);
}

#[test]
fn success_carries_the_digest() {
    let message = serde_json::json!({
        "result": "succeeded",
        "digest": DIGEST,
        "tags": ["main-1-3e1a9c2"],
        "phases": {"fetch": 1.0, "build": 12.34},
        "cache": {"total": 5, "cached": 3},
    })
    .to_string();
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
fn an_agent_run_waits_for_its_model_key() {
    let secret_ref = SecretKeyRef {
        name: "model".to_owned(),
        key: "anthropic".to_owned(),
    };
    let secret = |key: &str, value: &[u8]| Secret {
        data: Some(BTreeMap::from([(
            key.to_owned(),
            k8s_openapi::ByteString(value.to_vec()),
        )])),
        ..Secret::default()
    };
    assert_eq!(
        model_key(&secret_ref, "team-a", None),
        Err(Refusal::new(
            Reason::ModelKeyMissing,
            "no Secret `model` in `team-a` for the model API key"
        ))
    );
    for wrong in [secret("openai", b"sk"), secret("anthropic", b"")] {
        assert_eq!(
            model_key(&secret_ref, "team-a", Some(&wrong)),
            Err(Refusal::new(
                Reason::ModelKeyMissing,
                "Secret `model` has no key `anthropic` with the model API key"
            ))
        );
    }
    assert_eq!(
        model_key(&secret_ref, "team-a", Some(&secret("anthropic", b"sk"))),
        Ok(())
    );
}

/// An `agent` run checks how it signs in only when it plans; a run that restores a saved plan
/// calls no model and starts without a key.
#[test]
fn only_a_run_that_plans_needs_the_model_key() {
    use zeabur_builder_api::cluster_builder::{
        AgentPlanner, ModelAuth, ModelSpec, Planner, PlannerSpec,
    };
    let auth = ModelAuth::ApiKey {
        api_key: SecretKeyRef {
            name: "model".to_owned(),
            key: "anthropic".to_owned(),
        },
    };
    let mut agent = cluster_builder(1).spec;
    agent.planner = Planner::Agent {
        agent: AgentPlanner {
            planner: PlannerSpec {
                image: "agent".to_owned(),
                timeout: None,
                resources: None,
            },
            model: ModelSpec {
                provider: "anthropic".to_owned(),
                model: "claude-opus-5-5".to_owned(),
                base_url: None,
                auth: auth.clone(),
            },
        },
    };
    assert_eq!(model_auth(&agent, false), Some(&auth));
    assert_eq!(model_auth(&cluster_builder(1).spec, false), None);
    assert_eq!(model_auth(&agent, true), None);
}

#[test]
fn a_federated_run_waits_for_its_service_account() {
    assert_eq!(
        model_service_account("builder-agent", "team-a", true),
        Ok(())
    );
    assert_eq!(
        model_service_account("builder-agent", "team-a", false),
        Err(Refusal::new(
            Reason::ModelKeyMissing,
            "no ServiceAccount `builder-agent` in `team-a` for the model token"
        ))
    );
}

/// A run whose Job restored a saved plan reports that plan as its own, with `planSource: saved`.
#[test]
fn a_restored_run_reports_the_saved_plan() {
    let finished = BuildRunStatus {
        phase: Some(Phase::Succeeded),
        plan_source: Some(PlanSource::Generated),
        ..BuildRunStatus::default()
    };
    let status = from_job(finished, &job_of("agent", "r/team-a/api", Some(PLAN)));
    assert_eq!(status.plan_source, Some(PlanSource::Saved));
    assert_eq!(status.plan_digest.as_deref(), Some(PLAN));
    assert_eq!(status.builder.as_deref(), Some("agent"));
    assert_eq!(status.image.as_deref(), Some("r/team-a/api"));
}

#[test]
fn the_agent_s_plan_report_reaches_the_status() {
    let stopped = pod(
        vec![
            container("fetch", terminated(0, None)),
            container(
                "plan",
                terminated(
                    1,
                    Some(
                        &serde_json::json!({
                            "reason": "NeedsBuildVariable",
                            "detail": "Set DATABASE_URL.",
                            "modelCalled": true,
                            "inputTokens": 1200,
                            "outputTokens": 300,
                        })
                        .to_string(),
                    ),
                ),
            ),
        ],
        vec![],
    );
    let Observed::Finished(status) = observe(
        &job(Some(("Failed", "BackoffLimitExceeded"))),
        Some(&stopped),
    ) else {
        panic!("finished expected")
    };
    assert_eq!(status.reason, Some(Reason::PlanFailed));
    assert_eq!(status.message.as_deref(), Some("Set DATABASE_URL."));
    assert_eq!(
        status.failed_step.as_ref().map(|s| s.message.as_str()),
        Some("Set DATABASE_URL.")
    );
    assert_eq!(
        status.plan,
        Some(PlanReport {
            dockerfile: None,
            stopped: Some(PlanStop::NeedsBuildVariable),
            detail: Some("Set DATABASE_URL.".to_owned()),
            model_called: true,
            input_tokens: 1200,
            output_tokens: 300,
            port: None,
            language: String::new(),
            version: String::new(),
            build_variables: Vec::new(),
            runtime_variables: Vec::new(),
        })
    );

    let long = "x".repeat(300);
    let planned = serde_json::json!({
        "source": "agent",
        "details": {
            "port": 3000,
            "language": "node",
            "version": "24.9",
            "buildVariables": ["NPM_TOKEN", long],
        },
        "modelCalled": true,
        "inputTokens": 9000,
        "outputTokens": 800,
    })
    .to_string();
    let built = serde_json::json!({
        "result": "succeeded",
        "digest": DIGEST,
        "tags": ["main-1-3e1a9c2"],
    })
    .to_string();
    let done = pod(
        vec![
            container("fetch", terminated(0, None)),
            container("plan", terminated(0, Some(&planned))),
        ],
        vec![container("build", terminated(0, Some(&built)))],
    );
    let Observed::Finished(status) = observe(&job(Some(("Complete", ""))), Some(&done)) else {
        panic!("finished expected")
    };
    assert_eq!(status.phase, Some(Phase::Succeeded));
    let plan = status.plan.unwrap();
    assert_eq!(plan.dockerfile, Some(DockerfileSource::Agent));
    assert_eq!(plan.port, Some(3000));
    assert_eq!(plan.language, "node");
    assert_eq!(plan.build_variables, ["NPM_TOKEN"]);
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
    let failed_step = status.failed_step.as_ref().unwrap();
    assert_eq!(failed_step.name, "fetch");
    assert_eq!(failed_step.state, StepState::Failed);
    assert_eq!(failed_step.message, "fatal: couldn't find remote ref");
    assert_eq!(status.steps, std::slice::from_ref(failed_step));

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
    assert_eq!(
        status
            .failed_step
            .as_ref()
            .map(|s| (s.name.as_str(), s.state)),
        Some(("plan", StepState::Failed))
    );

    let build = pod(
        vec![container("fetch", terminated(0, None))],
        vec![container(
            "build",
            terminated(
                1,
                Some(
                    &serde_json::json!({
                        "result": "failed",
                        "reason": "BuildFailed",
                        "message": "denied",
                    })
                    .to_string(),
                ),
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
fn a_job_of_the_run_is_adopted_at_once() {
    let mut job = job(None);
    job.metadata.name = Some("app-x1".to_owned());
    job.metadata.creation_timestamp = Some(Time(now()));

    let started = BuildRunStatus {
        job_ref: Some("app-x1".to_owned()),
        buildkitd_instance: Some("buildkitd-0".to_owned()),
        ..BuildRunStatus::default()
    };
    assert_eq!(recorded(started.clone(), &job), started);

    let adopted = recorded(BuildRunStatus::default(), &job);
    assert_eq!(adopted.job_ref.as_deref(), Some("app-x1"));
    assert_eq!(adopted.start_time, Some(now()));
}

#[test]
fn a_run_controls_only_a_job_that_names_its_uid() {
    let mut run = run("app-x1", A, Some(Phase::Pending), "2026-10-22T01:00:00Z");
    run.metadata.uid = Some("uid-new".to_owned());
    let owned_by = |uid: &str, controller: bool| {
        let mut job = job(None);
        job.metadata.owner_references = Some(vec![OwnerReference {
            uid: uid.to_owned(),
            controller: Some(controller),
            ..OwnerReference::default()
        }]);
        job
    };
    assert!(controls(&run, &owned_by("uid-new", true)));
    assert!(!controls(&run, &owned_by("uid-old", true)));
    assert!(!controls(&run, &owned_by("uid-new", false)));
    assert!(!controls(&run, &job(None)));
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
    let mut edited = build.clone();
    edited.metadata.generation = Some(2);
    assert_ne!(name, run_name(&edited, A), "a spec edit");
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
            ConditionType::SourceReady,
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
        last_run_generation: Some(1),
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
    };
    let reason = |archive: &ArchiveSource| {
        let (status, reason, _, digest) = archive_source(archive);
        (status, reason.to_string(), digest)
    };
    assert_eq!(
        reason(&archive(None)),
        (ConditionStatus::False, "WaitingForUpload".to_owned(), None)
    );
    assert_eq!(
        reason(&archive(Some(DIGEST))),
        (
            ConditionStatus::True,
            "Archive".to_owned(),
            Some(DIGEST.to_owned())
        )
    );
}

const QUIET: Duration = Duration::from_mins(1);

fn holding(namespace: &str, name: &str, at: &str) -> BuildRun {
    let mut run = waiting(namespace, name, at);
    if let Some(status) = run.status.as_mut() {
        status.buildkitd_instance = Some(format!("buildkitd-{namespace}-0"));
    }
    run
}

fn unschedulable_pod(message: &str) -> Pod {
    Pod {
        status: Some(PodStatus {
            conditions: Some(vec![PodCondition {
                type_: "PodScheduled".to_owned(),
                status: "False".to_owned(),
                reason: Some("Unschedulable".to_owned()),
                message: Some(message.to_owned()),
                last_transition_time: Some(Time(now())),
                ..PodCondition::default()
            }]),
            ..PodStatus::default()
        }),
        ..Pod::default()
    }
}

#[test]
fn a_pod_is_unschedulable_when_the_scheduler_says_so() {
    let memory = "0/1 nodes are available: 1 Insufficient memory.";
    assert_eq!(
        unschedulable(&unschedulable_pod(memory)),
        Some((now(), memory.to_owned()))
    );
    assert_eq!(unschedulable(&Pod::default()), None);
}

#[test]
fn the_longest_idle_daemon_goes_first() {
    let up = ["team-a", "team-b", "team-c"].map(str::to_owned);
    let a = run("a", A, Some(Phase::Succeeded), "2026-10-22T07:00:00Z");
    let b = run("b", A, Some(Phase::Succeeded), "2026-10-22T06:00:00Z");
    let mut a = a;
    a.metadata.namespace = Some("team-a".to_owned());
    let mut b = b;
    b.metadata.namespace = Some("team-b".to_owned());
    assert_eq!(
        idlest(&up, &[&a, &b], "team-c", now(), QUIET),
        Some("team-b")
    );
    assert_eq!(
        idlest(&up, &[&a, &b], "team-b", now(), QUIET),
        Some("team-c"),
        "no runs at all"
    );
    assert_eq!(
        idlest(&up[..2], &[&a, &b], "team-b", now(), QUIET),
        Some("team-a")
    );
}

#[test]
fn ready_names_the_outcome_of_the_latest_finished_run() {
    let ready = |runs: &[BuildRun]| {
        let mut status = BuildStatus::default();
        summarise(&mut status, runs, now(), Some(1));
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

#[test]
fn ready_waits_for_a_run_of_the_current_generation() {
    let conditions = |runs: &[BuildRun], generation: i64, before: &[Condition]| {
        let mut status = BuildStatus {
            conditions: before.to_vec(),
            ..BuildStatus::default()
        };
        summarise(&mut status, runs, now(), Some(generation));
        status.conditions
    };
    let kinds = |c: &[Condition]| {
        c.iter()
            .map(|c| (c.kind, c.status))
            .collect::<Vec<(ConditionType, ConditionStatus)>>()
    };
    let old = run("api-1", A, Some(Phase::Succeeded), "2026-10-22T01:00:00Z");

    // The spec was edited: the old success is not the current spec's.
    let edited = conditions(std::slice::from_ref(&old), 2, &[]);
    assert_eq!(
        kinds(&edited),
        [(ConditionType::Ready, ConditionStatus::Unknown)]
    );

    // A run of the new generation is under way, then succeeds.
    let mut building = run("api-2", A, Some(Phase::Building), "2026-10-22T02:00:00Z");
    building.metadata.annotations = Some(BTreeMap::from([(
        annotations::BUILD_GENERATION.to_owned(),
        "2".to_owned(),
    )]));
    let under_way = conditions(&[old.clone(), building.clone()], 2, &edited);
    assert_eq!(
        kinds(&under_way),
        [
            (ConditionType::Ready, ConditionStatus::Unknown),
            (ConditionType::Reconciling, ConditionStatus::True)
        ]
    );
    building.status = run("api-2", A, Some(Phase::Succeeded), "2026-10-22T03:00:00Z").status;
    let done = conditions(&[old, building], 2, &under_way);
    assert_eq!(
        kinds(&done),
        [(ConditionType::Ready, ConditionStatus::True)]
    );
    assert_eq!(done[0].observed_generation, Some(2));
}

#[test]
fn a_build_is_stalled_while_its_source_or_builder_is_not_ready() {
    let mut status = BuildStatus::default();
    set_condition(
        &mut status.conditions,
        ConditionType::SourceReady,
        ConditionStatus::False,
        ConditionReason::RefNotFound,
        "no such ref",
        now(),
        Some(1),
    );
    summarise(&mut status, &[], now(), Some(1));
    let stalled = status
        .conditions
        .iter()
        .find(|c| c.kind == ConditionType::Stalled)
        .unwrap();
    assert_eq!(
        (
            stalled.status,
            stalled.reason.as_str(),
            stalled.message.as_str()
        ),
        (ConditionStatus::True, "RefNotFound", "no such ref")
    );

    set_condition(
        &mut status.conditions,
        ConditionType::SourceReady,
        ConditionStatus::True,
        ConditionReason::Resolved,
        "",
        now(),
        Some(1),
    );
    summarise(&mut status, &[], now(), Some(1));
    assert!(
        status
            .conditions
            .iter()
            .all(|c| c.kind != ConditionType::Stalled)
    );
}

#[test]
fn steps_reach_the_run_within_the_crd_bounds() {
    let long = "錯".repeat(300);
    let step = |i: usize| {
        serde_json::json!({
            "name": format!("{i} {long}"),
            "state": "succeeded",
            "startTime": "2026-10-22T00:00:00Z",
        })
    };
    let steps: Vec<_> = (0..25).map(step).collect();
    let message = serde_json::json!({
        "result": "failed",
        "reason": "BuildFailed",
        "message": "exit code: 1",
        "omittedSteps": 3,
        "steps": steps,
        "failedStep": {"name": format!("0 {long}"), "state": "failed", "message": long},
    })
    .to_string();
    let decoded = BuildResult::decode(&message);
    // Larger than a termination message holds; the controller must still bound what it copies.
    assert!(decoded.is_err());
    let Observed::Finished(status) = from_result(serde_json::from_str(&message).unwrap()) else {
        panic!("finished expected")
    };
    assert_eq!(status.steps.len(), run_api::MAX_STEPS);
    assert!(status.steps[0].name.starts_with("5 "));
    assert_eq!(status.omitted_steps, Some(8));
    for step in status.steps.iter().chain(status.failed_step.as_ref()) {
        assert!(step.name.chars().count() <= run_api::MAX_STEP_TEXT);
        assert!(step.message.chars().count() <= run_api::MAX_STEP_TEXT);
    }
    assert_eq!(
        status.failed_step.as_ref().map(|s| s.state),
        Some(StepState::Failed)
    );
}

fn condition(status: &BuildRunStatus, kind: ConditionType) -> Option<&Condition> {
    status.conditions.iter().find(|c| c.kind == kind)
}

/// `run` as the server stores it with `status`.
fn stored(run: &BuildRun, status: BuildRunStatus) -> BuildRun {
    BuildRun {
        status: Some(status),
        ..run.clone()
    }
}

#[test]
fn run_conditions_follow_the_phase() {
    let mut subject = run("api-1", A, None, "2026-10-22T01:00:00Z");
    subject.metadata.generation = Some(3);
    let t = |at: &str| -> Timestamp { at.parse().unwrap() };
    // Each write starts from a new status, as a finished status does; only the stored run keeps
    // the earlier conditions.
    let step = |before: BuildRunStatus, phase: Phase, reason: Option<Reason>, at: &str| {
        run_status(
            &stored(&subject, before),
            BuildRunStatus {
                phase: Some(phase),
                reason,
                ..BuildRunStatus::default()
            },
            t(at),
        )
    };
    let finished = [
        (
            Phase::Succeeded,
            None,
            None,
            ConditionStatus::True,
            "Succeeded",
        ),
        (
            Phase::Failed,
            Some(Reason::BuildFailed),
            Some("BuildFailed"),
            ConditionStatus::False,
            "BuildFailed",
        ),
        (
            Phase::Failed,
            Some(Reason::OutOfMemory),
            Some("OutOfMemory"),
            ConditionStatus::False,
            "OutOfMemory",
        ),
        (
            Phase::Cancelled,
            Some(Reason::Cancelled),
            Some("Cancelled"),
            ConditionStatus::False,
            "Cancelled",
        ),
    ];
    for (end, reason, stalled, ready, ready_reason) in finished {
        let case = format!("{end:?}/{reason:?}");
        let pending = step(
            BuildRunStatus::default(),
            Phase::Pending,
            Some(Reason::QuotaExceeded),
            "2026-10-22T01:00:00Z",
        );
        let reconciling = condition(&pending, ConditionType::Reconciling).unwrap();
        assert_eq!(reconciling.status, ConditionStatus::True, "{case}");
        assert_eq!(reconciling.reason, "QuotaExceeded", "{case}");
        assert_eq!(
            condition(&pending, ConditionType::Ready).unwrap().status,
            ConditionStatus::False
        );
        assert!(
            condition(&pending, ConditionType::Stalled).is_none(),
            "{case}"
        );

        let planning = step(pending, Phase::Planning, None, "2026-10-22T01:01:00Z");
        let building = step(planning, Phase::Building, None, "2026-10-22T01:02:00Z");
        let reconciling = condition(&building, ConditionType::Reconciling).unwrap();
        assert_eq!(reconciling.reason, "Building", "{case}");
        // Still True since Pending: the transition time stays.
        let since = ts("2026-10-22T01:00:00Z");
        assert_eq!(reconciling.last_transition_time, since, "{case}");
        let building_ready = condition(&building, ConditionType::Ready).unwrap();
        assert_eq!(building_ready.last_transition_time, since, "{case}");

        let done = step(building, end, reason, "2026-10-22T01:03:00Z");
        assert!(
            condition(&done, ConditionType::Reconciling).is_none(),
            "{case}"
        );
        assert_eq!(
            condition(&done, ConditionType::Stalled).map(|c| (c.status, c.reason.as_str())),
            stalled.map(|r| (ConditionStatus::True, r)),
            "{case}"
        );
        let ready_condition = condition(&done, ConditionType::Ready).unwrap();
        assert_eq!(ready_condition.status, ready, "{case}");
        assert_eq!(ready_condition.reason, ready_reason, "{case}");
        let moved = ready == ConditionStatus::True;
        let at_end = ready_condition.last_transition_time == ts("2026-10-22T01:03:00Z");
        assert_eq!(at_end, moved, "{case}");
        for c in &done.conditions {
            assert_eq!(c.observed_generation, Some(3), "{case}");
        }

        // Writing the finished status again changes nothing.
        let again = run_status(
            &stored(&subject, done.clone()),
            done.clone(),
            t("2026-10-22T02:00:00Z"),
        );
        assert_eq!(again, done, "{case}");
    }
}

#[test]
fn a_run_records_its_own_rebuild_request() {
    let mut subject = run("api-1", A, None, "2026-10-22T01:00:00Z");
    let status = BuildRunStatus {
        phase: Some(Phase::Pending),
        ..BuildRunStatus::default()
    };
    assert_eq!(
        run_status(&subject, status.clone(), now()).observed_rebuild,
        None
    );
    subject.metadata.annotations = Some(BTreeMap::from([(
        annotations::REBUILD.to_owned(),
        "2026-10-22T00:59:00Z".to_owned(),
    )]));
    assert_eq!(
        run_status(&subject, status, now())
            .observed_rebuild
            .as_deref(),
        Some("2026-10-22T00:59:00Z")
    );
}

#[test]
fn a_daemon_with_an_admitted_run_is_not_freed() {
    let up = ["team-a", "team-b"].map(str::to_owned);
    let busy = holding("team-a", "a", "2026-10-22T07:00:00Z");
    let queued = waiting("team-b", "b", "2026-10-22T07:30:00Z");
    assert_eq!(
        idlest(&up, &[&busy, &queued], "team-c", now(), QUIET),
        Some("team-b")
    );
    let mut done = run("c", A, Some(Phase::Succeeded), "2026-10-22T07:00:00Z");
    done.metadata.namespace = Some("team-b".to_owned());
    let hold = holding("team-b", "d", "2026-10-22T07:40:00Z");
    assert_eq!(
        idlest(&up, &[&busy, &done, &hold], "team-c", now(), QUIET),
        None
    );
    assert_eq!(idlest(&[], &[], "team-c", now(), QUIET), None);
}

#[test]
fn a_run_gives_its_slot_only_to_a_run_that_can_start() {
    let grace = Duration::from_mins(1);
    let since: Timestamp = "2026-10-22T07:58:30Z".parse().unwrap();
    let mine = holding("team-c", "c", "2026-10-22T07:50:00Z");
    let other = waiting("team-a", "a", "2026-10-22T07:55:00Z");
    let ready = BTreeMap::from([("team-a".to_owned(), 1)]);
    let cold = BTreeMap::new();
    let release = |runs: &[&BuildRun], ready: &BTreeMap<String, u32>, since: Timestamp| {
        release_slot(runs, &mine, ready, since, now(), grace)
    };
    assert!(release(&[&mine, &other], &ready, since));
    assert!(
        !release(&[&mine, &other], &cold, since),
        "its daemon is down"
    );
    assert!(!release(&[&mine], &ready, since), "nobody waits");
    let recent: Timestamp = "2026-10-22T07:59:30Z".parse().unwrap();
    assert!(
        !release(&[&mine, &other], &ready, recent),
        "within the grace"
    );
    let mut credentials = waiting("team-a", "a", "2026-10-22T07:55:00Z");
    if let Some(status) = credentials.status.as_mut() {
        status.reason = Some(Reason::RegistryCredentialsMissing);
    }
    assert!(!release(&[&mine, &credentials], &ready, since));
    let running = holding("team-a", "r", "2026-10-22T07:40:00Z");
    assert!(
        !release(&[&mine, &other, &running], &ready, since),
        "its namespace is at its limit"
    );
    let roomy = BTreeMap::from([("team-a".to_owned(), 2)]);
    assert!(release(&[&mine, &other, &running], &roomy, since));
}

#[test]
fn a_released_run_does_not_block_the_run_that_took_its_slot() {
    let mut released = waiting("team-c", "c", "2026-10-22T07:50:00Z");
    if let Some(status) = released.status.as_mut() {
        status.reason = Some(Reason::InsufficientResources);
    }
    let newer = waiting("team-a", "a", "2026-10-22T07:55:00Z");
    assert!(admission(std::slice::from_ref(&released), &newer, 1, 1).is_ok());
    let plain = waiting("team-c", "c", "2026-10-22T07:50:00Z");
    assert!(admission(&[plain], &newer, 1, 1).is_err());
    let taken = [started("team-a", "a", "2026-10-22T07:55:00Z")];
    assert!(admission(&taken, &released, 1, 1).is_err());
}

#[test]
fn a_daemon_active_a_moment_ago_is_not_freed() {
    let up = ["team-a", "team-b"].map(str::to_owned);
    let mut recent = run("a", A, Some(Phase::Succeeded), "2026-10-22T07:59:30Z");
    recent.metadata.namespace = Some("team-a".to_owned());
    let mut quiet = run("b", A, Some(Phase::Succeeded), "2026-10-22T07:58:00Z");
    quiet.metadata.namespace = Some("team-b".to_owned());
    assert_eq!(
        idlest(&up, &[&recent, &quiet], "team-c", now(), QUIET),
        Some("team-b")
    );
    assert_eq!(idlest(&up[..1], &[&recent], "team-c", now(), QUIET), None);
    let mut new = waiting("team-a", "n", "2026-10-22T07:59:50Z");
    new.status = None;
    assert_eq!(idlest(&up[..1], &[&new], "team-c", now(), QUIET), None);
}

fn given_back(namespace: &str, name: &str, at: &str) -> BuildRun {
    let mut run = waiting(namespace, name, at);
    if let Some(status) = run.status.as_mut() {
        status.reason = Some(Reason::InsufficientResources);
    }
    run
}

#[test]
fn a_run_that_gave_its_slot_back_does_not_take_it_again() {
    let released = given_back("team-c", "c", "2026-10-22T07:50:00Z");
    let newer = waiting("team-a", "a", "2026-10-22T07:55:00Z");
    assert!(admission(std::slice::from_ref(&newer), &released, 1, 1).is_err());
    assert!(admission(std::slice::from_ref(&released), &newer, 1, 1).is_ok());
    assert!(
        admission(&[newer], &released, 1, 2).is_ok(),
        "a second slot is free"
    );
}

#[test]
fn a_run_that_keeps_its_slot_while_it_waits_for_resources_keeps_its_place() {
    let mut waiting_for_memory = holding("team-c", "c", "2026-10-22T07:50:00Z");
    if let Some(status) = waiting_for_memory.status.as_mut() {
        status.reason = Some(Reason::InsufficientResources);
    }
    let newer = waiting("team-a", "a", "2026-10-22T07:55:00Z");
    assert!(
        admission(
            &[newer, waiting_for_memory.clone()],
            &waiting_for_memory,
            1,
            1
        )
        .is_ok(),
        "the newer run does not come before it"
    );
}

#[test]
fn a_run_gives_its_slot_to_a_run_that_comes_before_it_afterwards() {
    let grace = Duration::from_mins(1);
    let since: Timestamp = "2026-10-22T07:58:30Z".parse().unwrap();
    let mine = holding("team-c", "c", "2026-10-22T07:50:00Z");
    let ready = BTreeMap::from([("team-a".to_owned(), 1)]);
    let newer = given_back("team-a", "a", "2026-10-22T07:55:00Z");
    assert!(
        release_slot(&[&mine, &newer], &mine, &ready, since, now(), grace),
        "a run that gave its slot back earlier comes first, even if it is newer"
    );
}

#[test]
fn runs_that_gave_their_slots_back_take_turns() {
    let released_at = |mut run: BuildRun, at: &str| {
        if let Some(status) = run.status.as_mut() {
            status.slot_released_at = Some(ts(at));
        }
        run
    };
    let early = released_at(
        given_back("team-a", "a", "2026-10-22T07:55:00Z"),
        "2026-10-22T07:40:00Z",
    );
    let late = released_at(
        given_back("team-c", "c", "2026-10-22T07:50:00Z"),
        "2026-10-22T07:58:00Z",
    );
    assert!(
        admission(std::slice::from_ref(&late), &early, 1, 1).is_ok(),
        "the run that gave its slot back earlier goes first"
    );
    assert!(
        admission(std::slice::from_ref(&early), &late, 1, 1).is_err(),
        "the other run is older but gave its slot back later"
    );
}

#[test]
fn daemon_out_of_memory_says_when_a_new_limit_applies() {
    let idle = Duration::from_mins(10);
    for (running, wanted, still_old) in [
        (Some("2Gi"), Some("4Gi"), true),
        (Some("2Gi"), Some("2Gi"), false),
        (Some("4Gi"), Some("2Gi"), false),
        (None, Some("4Gi"), false),
    ] {
        let message = daemon_out_of_memory(running, wanted, idle);
        assert!(
            message.contains("idle for 10m and is replaced"),
            "{message}"
        );
        assert_eq!(
            message.contains("still has the old limit of 2Gi"),
            still_old,
            "{message}"
        );
        assert_eq!(
            message.contains("builder.zeabur.com/build-memory"),
            !still_old,
            "{message}"
        );
    }
}
