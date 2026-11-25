//! `zeabur-builder`: the controller binary.

mod controller;
mod credentials;
mod daemon;
mod decide;
mod github;
mod github_app;
mod leader;
mod notify;
mod output;
mod record;
mod registry;
mod registry_ca;
mod remote;
mod ssh;
mod tls;
mod webhook;
mod write;

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use k8s_openapi::api::core::v1::Toleration;
use thiserror::Error;
use zeabur_builder_jobspec::buildkitd::Resources;
use zeabur_builder_jobspec::{JobMemory, Placement};

/// Why a subcommand failed.
#[derive(Debug, Error)]
enum Error {
    /// A ceiling below the request it must hold.
    #[error("--buildkitd-{what}-max={max} is below --buildkitd-{what}-request={request}")]
    CeilingBelowRequest {
        what: &'static str,
        max: String,
        request: String,
    },
    #[error("a TLS provider is already installed")]
    TlsProvider,
    #[error("cannot reach Kubernetes: {0}")]
    Kube(#[source] kube::Error),
    #[error(transparent)]
    Crds(#[from] serde_json::Error),
    #[error(transparent)]
    Registry(#[from] registry::RegistryError),
    #[error(transparent)]
    GithubApp(#[from] github_app::GithubAppError),
    #[error(transparent)]
    Controller(#[from] controller::ControllerError),
}

/// Zeabur Builder: turns Git commits into container images with `BuildKit`.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[expect(clippy::large_enum_variant, reason = "parsed once at start")]
#[derive(Debug, Subcommand)]
enum Command {
    /// Print the custom resource definitions as a YAML stream, for `kubectl apply -f -`.
    Crds,
    /// Run the controller.
    Controller(ControllerArgs),
    /// Set up the cluster's GitHub App.
    #[command(subcommand)]
    GithubApp(github_app::Command),
    /// Print a token for the default registry, signed with the registry token key. Run it in
    /// the controller's Pod:
    /// `kubectl exec deploy/zeabur-builder -- /zeabur-builder registry-token`.
    RegistryToken(RegistryTokenArgs),
}

#[derive(Debug, Args)]
struct RegistryTokenArgs {
    /// A repository the token may pull, such as `team-a/tools`. Repeat for more.
    #[arg(long = "repository", required = true)]
    repositories: Vec<String>,
    /// The token may also push.
    #[arg(long)]
    push: bool,
    /// The token may also delete manifests.
    #[arg(long)]
    delete: bool,
    /// How long the token lasts, such as `15m`.
    #[arg(long, default_value = "15m", value_parser = duration)]
    ttl: Duration,
    /// The Secret whose `tls.key` signs tokens, `<namespace>/<name>`.
    #[arg(long, env = "ZEABUR_BUILDER_REGISTRY_TOKEN_KEY", value_parser = secret_ref)]
    registry_token_key: (String, String),
}

/// Parses `<namespace>/<name>`.
fn secret_ref(value: &str) -> Result<(String, String), String> {
    match value.split_once('/') {
        Some((namespace, name))
            if !namespace.is_empty() && !name.is_empty() && !name.contains('/') =>
        {
            Ok((namespace.to_owned(), name.to_owned()))
        }
        _ => Err(format!("`{value}` is not <namespace>/<name>")),
    }
}

/// Checks a positive Kubernetes quantity, such as `500m` or `4Gi`.
fn quantity(value: &str) -> Result<String, String> {
    zeabur_builder_jobspec::buildkitd::times(value, 1)
        .map(|_| value.to_owned())
        .ok_or_else(|| format!("`{value}` is not a positive quantity, such as 500m or 4Gi"))
}

/// Parses a duration such as `90s`, `10m` or `1h`; a bare number is refused.
fn duration(value: &str) -> Result<Duration, String> {
    decide::parse_duration(value).ok_or_else(|| {
        if value.parse::<u64>().is_ok() {
            format!("`{value}` has no unit; write {value}s for seconds, or 10m, 1h")
        } else {
            format!("`{value}` is not a duration, such as 90s, 10m or 1h")
        }
    })
}

/// A duration of whole seconds, as a Job counts them.
fn whole_seconds(value: &str) -> Result<Duration, String> {
    let d = duration(value)?;
    if d.subsec_nanos() == 0 {
        Ok(d)
    } else {
        Err(format!(
            "`{value}` is not a whole number of seconds, as a Job counts them"
        ))
    }
}

/// A Job's deadline: whole seconds, at least one, as `activeDeadlineSeconds` requires.
fn job_deadline(value: &str) -> Result<Duration, String> {
    let d = whole_seconds(value)?;
    if d.is_zero() {
        Err(format!("`{value}` is too short; a build needs at least 1s"))
    } else {
        Ok(d)
    }
}

/// Parses `key=value`.
fn node_label(value: &str) -> Result<(String, String), String> {
    match value.split_once('=') {
        Some((key, label)) if !key.is_empty() => Ok((key.to_owned(), label.to_owned())),
        _ => Err(format!("`{value}` is not <key>=<value>")),
    }
}

/// Parses a toleration from JSON.
fn toleration(value: &str) -> Result<Toleration, String> {
    serde_json::from_str(value).map_err(|e| format!("`{value}` is not a toleration: {e}"))
}

async fn registry_token(args: RegistryTokenArgs) -> Result<String, Error> {
    let client = kube::Client::try_default().await.map_err(Error::Kube)?;
    let (namespace, name) = &args.registry_token_key;
    let signer = registry::read_signer(&client, namespace, name).await?;
    let access = registry::Access {
        push: args.push,
        delete: args.delete,
    };
    let grants: Vec<_> = args
        .repositories
        .iter()
        .map(|r| (r.as_str(), access))
        .collect();
    Ok(signer.token(&grants, jiff::Timestamp::now(), args.ttl))
}

#[derive(Debug, Args)]
struct ControllerArgs {
    /// The controller's namespace.
    #[arg(long, env = "POD_NAMESPACE")]
    namespace: String,
    /// The registry for `Build`s without `spec.output.image`, such as `registry.internal:5000`.
    #[arg(long, env = "ZEABUR_BUILDER_DEFAULT_REGISTRY")]
    default_registry: String,
    /// The Secret, `<namespace>/<name>`, that holds the default registry's CA (PEM), when a
    /// public CA did not sign its certificate, such as `zot/zot-tls`. The controller follows it
    /// and publishes the CA alone in the Secret `registry-ca` of its namespace.
    #[arg(long, env = "ZEABUR_BUILDER_REGISTRY_CA", value_parser = secret_ref)]
    registry_ca: Option<(String, String)>,
    /// The key of the CA in `--registry-ca`.
    #[arg(long, env = "ZEABUR_BUILDER_REGISTRY_CA_KEY", default_value = registry_ca::KEY)]
    registry_ca_key: String,
    /// The default registry speaks plain HTTP. Only for a test cluster.
    #[arg(long, env = "ZEABUR_BUILDER_REGISTRY_HTTP", default_value_t = false, action = clap::ArgAction::Set)]
    registry_http: bool,
    /// The Secret, `<namespace>/<name>`, whose `tls.key` (PKCS#8 Ed25519) signs tokens the
    /// default registry trusts, such as `zot/zot-token-signer`. Required. Each run pushes with a
    /// token for its own repositories, and the repositories of deleted `Build`s are deleted.
    #[arg(long, env = "ZEABUR_BUILDER_REGISTRY_TOKEN_KEY", value_parser = secret_ref)]
    registry_token_key: (String, String),
    /// The image of the Job's `fetch` and `build` containers.
    #[arg(long, env = "ZEABUR_BUILDER_JOB_IMAGE")]
    job_image: String,
    /// Builds a namespace runs at once; the Namespace annotation
    /// `builder.zeabur.com/max-concurrent-builds` overrides it.
    #[arg(long, default_value_t = decide::DEFAULT_MAX_CONCURRENT)]
    max_concurrent_builds: u32,
    /// Builds all namespaces run at once. A run over the limit waits, oldest first.
    #[arg(long, default_value_t = decide::DEFAULT_MAX_CLUSTER_BUILDS)]
    max_cluster_builds: u32,
    /// How long a run waits for memory or CPU for its buildkitd before it gives its cluster slot
    /// to a run that can start now, such as `1m`.
    #[arg(long, default_value = "1m", value_parser = duration)]
    buildkitd_pending_grace: Duration,
    /// Where the buildkitd of each namespace runs. Default: the controller's namespace.
    #[arg(long)]
    buildkitd_namespace: Option<String>,
    /// The buildkitd image, rootful.
    #[arg(
        long,
        default_value = "docker.io/moby/buildkit:v0.34.2@sha256:c5f8862f52499773af85bc07695b2379ef08e8002352b0df2c4864c91d78dabe"
    )]
    buildkitd_image: String,
    /// The `ConfigMap` with `buildkitd.toml`, in buildkitd's namespace.
    #[arg(long, default_value = "buildkitd")]
    buildkitd_config: String,
    /// Each namespace's cache volume.
    #[arg(long, default_value = "20Gi")]
    buildkitd_cache_size: String,
    /// The cache volumes' storage class. Default: the cluster's.
    #[arg(long)]
    buildkitd_storage_class: Option<String>,
    /// A `RuntimeClass` to run buildkitd in, such as a Kata or gVisor sandbox. Default: none.
    #[arg(long)]
    buildkitd_runtime_class: Option<String>,
    /// buildkitd's CPU request per build. Its container asks for this times the namespace's
    /// concurrent builds.
    #[arg(long, default_value = "100m")]
    buildkitd_cpu_request: String,
    /// buildkitd's memory request per build.
    #[arg(long, default_value = "256Mi")]
    buildkitd_memory_request: String,
    /// buildkitd's CPU limit per build; the Namespace annotation `builder.zeabur.com/build-cpu`
    /// overrides it.
    #[arg(long, default_value = "1")]
    buildkitd_cpu_limit: String,
    /// buildkitd's memory limit per build; the Namespace annotation
    /// `builder.zeabur.com/build-memory` overrides it.
    #[arg(long, default_value = "1Gi")]
    buildkitd_memory_limit: String,
    /// The most CPU one build may get, whatever `builder.zeabur.com/build-cpu` asks. Default: no
    /// ceiling.
    #[arg(long, value_parser = quantity)]
    buildkitd_cpu_max: Option<String>,
    /// The most memory one build may get, whatever `builder.zeabur.com/build-memory` asks.
    /// Default: no ceiling.
    #[arg(long, value_parser = quantity)]
    buildkitd_memory_max: Option<String>,
    /// A node label, `key=value`, that build Jobs and buildkitd Pods require. Repeat for more.
    #[arg(long = "build-node-selector", value_parser = node_label)]
    build_node_selector: Vec<(String, String)>,
    /// A toleration of build Jobs and buildkitd Pods, as JSON, such as
    /// `{"key":"builder.zeabur.com/builds","operator":"Equal","value":"true","effect":"NoSchedule"
    /// }`. Repeat for more.
    #[arg(long = "build-toleration", value_parser = toleration)]
    build_tolerations: Vec<Toleration>,
    /// How long a namespace goes without builds before its buildkitd scales to zero, such as
    /// `10m`.
    #[arg(long, default_value = "10m", value_parser = duration)]
    buildkitd_idle: Duration,
    /// The memory request of each container of a build Job.
    #[arg(long, default_value = "128Mi", value_parser = quantity)]
    job_memory_request: String,
    /// The memory limit of each container of a build Job; the Namespace annotation
    /// `builder.zeabur.com/build-memory` overrides it.
    #[arg(long, default_value = "512Mi", value_parser = quantity)]
    job_memory_limit: String,
    /// How long a build may run, such as `1h`.
    #[arg(long, default_value = "1h", value_parser = job_deadline)]
    job_deadline: Duration,
    /// How long a finished Job is kept for log collectors, such as `10m`.
    #[arg(long, default_value = "10m", value_parser = whole_seconds)]
    job_ttl: Duration,
    /// The GitHub App's Secret in the controller's namespace: `app-id`, `client-id`,
    /// `private-key`, `webhook-secret` and, for GitHub Enterprise Server, `api-url`.
    #[arg(
        long,
        env = "ZEABUR_BUILDER_GITHUB_APP_SECRET",
        default_value = "github-app"
    )]
    github_app_secret: String,
    /// The `ConfigMap` in the controller's namespace that maps an installation id to the
    /// namespaces allowed to use it, such as `"12345": "team-a, team-b"`.
    #[arg(long, default_value = "github-app-installations")]
    github_app_grants: String,
    /// Where the webhook receiver listens, such as `0.0.0.0:8080`. Unset: no receiver.
    #[arg(long, env = "ZEABUR_BUILDER_WEBHOOK_ADDR")]
    webhook_addr: Option<SocketAddr>,
    /// The path deliveries arrive under: `<path>/github`, `<path>/gitlab`, `<path>/gitea`,
    /// `<path>/bitbucket`, `<path>/generic` and `<path>/github-app`.
    #[arg(long, default_value = "/hooks")]
    webhook_path: String,
    /// The largest delivery read, in bytes.
    #[arg(long, default_value_t = 1024 * 1024)]
    webhook_max_body: usize,
    /// Deliveries per second, on average, across all senders.
    #[arg(long, default_value_t = 10)]
    webhook_rate: u32,
    /// Deliveries at once above that average.
    #[arg(long, default_value_t = 50)]
    webhook_burst: u32,
}

impl ControllerArgs {
    fn settings(self) -> controller::Settings {
        let daemon = daemon::Settings {
            namespace: self
                .buildkitd_namespace
                .unwrap_or_else(|| self.namespace.clone()),
            image: self.buildkitd_image,
            config_map: self.buildkitd_config,
            cache_size: self.buildkitd_cache_size,
            storage_class: self.buildkitd_storage_class,
            runtime_class: self.buildkitd_runtime_class,
            resources: Resources {
                cpu_request: Some(self.buildkitd_cpu_request),
                memory_request: Some(self.buildkitd_memory_request),
                cpu_limit: Some(self.buildkitd_cpu_limit),
                memory_limit: Some(self.buildkitd_memory_limit),
            },
            idle_after: self.buildkitd_idle,
            placement: Placement {
                node_selector: self.build_node_selector.into_iter().collect(),
                tolerations: self.build_tolerations,
            },
        };
        controller::Settings {
            namespace: self.namespace,
            daemon: Arc::new(daemon),
            max_concurrent: self.max_concurrent_builds,
            max_cluster_builds: self.max_cluster_builds,
            ceiling: decide::Ceiling {
                cpu: self.buildkitd_cpu_max,
                memory: self.buildkitd_memory_max,
            },
            pending_grace: self.buildkitd_pending_grace,
            default_registry: self.default_registry,
            registry_ca: self
                .registry_ca
                .map(|(namespace, name)| registry_ca::Source {
                    namespace,
                    name,
                    key: self.registry_ca_key,
                }),
            registry_http: self.registry_http,
            registry_token_key: self.registry_token_key,
            job_image: self.job_image,
            job_memory: JobMemory {
                request: self.job_memory_request,
                limit: self.job_memory_limit,
            },
            job_deadline_seconds: i64::try_from(self.job_deadline.as_secs()).unwrap_or(i64::MAX),
            job_ttl_seconds: i32::try_from(self.job_ttl.as_secs()).unwrap_or(i32::MAX),
            github_app_secret: self.github_app_secret,
            github_app_grants: self.github_app_grants,
            webhook: self.webhook_addr.map(|addr| webhook::Settings {
                addr,
                prefix: self.webhook_path,
                max_body: self.webhook_max_body,
                rate: self.webhook_rate,
                burst: self.webhook_burst,
            }),
        }
    }
}

/// Refuses a ceiling below the request it must hold: the limit would be under the request.
fn check_ceiling(what: &'static str, request: &str, max: Option<&str>) -> Result<(), Error> {
    match max {
        Some(max) if zeabur_builder_jobspec::buildkitd::exceeds(request, max) => {
            Err(Error::CeilingBelowRequest {
                what,
                max: max.to_owned(),
                request: request.to_owned(),
            })
        }
        _ => Ok(()),
    }
}

async fn run_controller(args: ControllerArgs) -> Result<(), Error> {
    check_ceiling(
        "cpu",
        &args.buildkitd_cpu_request,
        args.buildkitd_cpu_max.as_deref(),
    )?;
    check_ceiling(
        "memory",
        &args.buildkitd_memory_request,
        args.buildkitd_memory_max.as_deref(),
    )?;
    install_tls()?;
    tracing_subscriber::fmt().json().with_target(false).init();
    let client = kube::Client::try_default().await.map_err(Error::Kube)?;
    Box::pin(controller::run(client, args.settings())).await?;
    Ok(())
}

fn install_tls() -> Result<(), Error> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_provider| Error::TlsProvider)
}

/// Runs a subcommand; the text for stdout, if it prints any.
async fn execute(command: Command) -> Result<Option<String>, Error> {
    match command {
        Command::Crds => Ok(Some(zeabur_builder_api::definitions_yaml()?)),
        Command::Controller(args) => {
            run_controller(args).await?;
            Ok(None)
        }
        Command::GithubApp(command) => {
            install_tls()?;
            let text = github_app::run(command).await?;
            Ok(Some(text))
        }
        Command::RegistryToken(args) => {
            install_tls()?;
            let text = registry_token(args).await?;
            Ok(Some(text))
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match execute(Cli::parse().command).await {
        Ok(Some(text)) => output::stdout_line(&text),
        Ok(None) => ExitCode::SUCCESS,
        Err(error) => {
            output::stderr_line(&format!("error: {error}"));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn the_time_flag_defaults_are_the_old_seconds() {
        let cli = Cli::try_parse_from([
            "zeabur-builder",
            "controller",
            "--namespace=builder-system",
            "--default-registry=registry.internal:5000",
            "--registry-token-key=zot/zot-token-signer",
            "--job-image=ghcr.io/zeabur-oss/builder-job:dev",
        ])
        .unwrap();
        let Command::Controller(args) = cli.command else {
            panic!("not the controller");
        };
        assert_eq!(args.buildkitd_pending_grace, Duration::from_secs(60));
        assert_eq!(args.buildkitd_idle, Duration::from_secs(600));
        assert_eq!(args.job_deadline, Duration::from_secs(3600));
        assert_eq!(args.job_ttl, Duration::from_secs(600));
    }

    #[test]
    fn a_duration_flag_takes_a_unit() {
        assert_eq!(duration("10m"), Ok(Duration::from_secs(600)));
        assert_eq!(duration("1h"), Ok(Duration::from_secs(3600)));
    }

    #[test]
    fn a_bare_number_is_refused_with_the_new_form() {
        let message = duration("600").unwrap_err();
        assert!(message.contains("write 600s"), "{message}");
        assert!(duration("soon").is_err());
        assert!(duration("-1m").is_err());
    }

    #[test]
    fn a_job_deadline_is_whole_seconds_and_not_zero() {
        assert_eq!(job_deadline("90s"), Ok(Duration::from_secs(90)));
        assert!(job_deadline("500ms").is_err());
        assert!(job_deadline("1.5s").is_err());
        assert!(job_deadline("0s").is_err());
        assert_eq!(whole_seconds("0s"), Ok(Duration::ZERO));
        assert!(whole_seconds("1500ms").is_err());
    }

    #[test]
    fn a_ceiling_below_the_request_is_refused() {
        assert_eq!(
            check_ceiling("memory", "256Mi", Some("128Mi"))
                .unwrap_err()
                .to_string(),
            "--buildkitd-memory-max=128Mi is below --buildkitd-memory-request=256Mi"
        );
        assert!(check_ceiling("memory", "256Mi", Some("1Gi")).is_ok());
        assert!(check_ceiling("cpu", "100m", None).is_ok());
    }

    #[test]
    fn the_charts_build_node_flags_parse() {
        let cli = Cli::try_parse_from([
            "zeabur-builder",
            "controller",
            "--namespace=builder-system",
            "--default-registry=registry.internal:5000",
            "--registry-token-key=zot/zot-token-signer",
            "--job-image=ghcr.io/zeabur-oss/builder-job:dev",
            "--build-node-selector=builder.zeabur.com/builds=true",
            r#"--build-toleration={"effect":"NoSchedule","key":"builder.zeabur.com/builds","operator":"Equal","value":"true"}"#,
        ])
        .unwrap();
        let Command::Controller(args) = cli.command else {
            panic!("not the controller");
        };
        let placement = args.settings().daemon.placement.clone();
        assert_eq!(
            placement.node_selector,
            BTreeMap::from([("builder.zeabur.com/builds".to_owned(), "true".to_owned())])
        );
        assert_eq!(
            placement.tolerations,
            vec![Toleration {
                key: Some("builder.zeabur.com/builds".to_owned()),
                operator: Some("Equal".to_owned()),
                value: Some("true".to_owned()),
                effect: Some("NoSchedule".to_owned()),
                toleration_seconds: None,
            }]
        );
        assert!(node_label("=true").is_err());
        assert!(toleration("key=x").is_err());
    }

    #[tokio::test]
    #[ignore = "needs a disposable cluster in KUBECONFIG (scripts/ci/kubernetes.sh)"]
    async fn the_api_server_refuses_a_planner_memory_that_is_not_a_quantity() {
        use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
        use kube::api::{Patch, PatchParams};
        use kube::runtime::wait::{await_condition, conditions};
        use kube::{Api, Client, CustomResourceExt as _};
        use zeabur_builder_api::cluster_builder::ClusterBuilder;

        let client = Client::try_default().await.unwrap();
        let definitions: Api<CustomResourceDefinition> = Api::all(client.clone());
        let definition = write::apply(&definitions, &ClusterBuilder::crd())
            .await
            .unwrap();
        let name = definition.metadata.name.unwrap();
        tokio::time::timeout(
            Duration::from_secs(30),
            await_condition(definitions, &name, conditions::is_crd_established()),
        )
        .await
        .unwrap()
        .unwrap();
        let builders: Api<ClusterBuilder> = Api::all(client);
        let dry_run = PatchParams::apply(write::MANAGER).dry_run();
        let with_memory = |memory: &str| -> ClusterBuilder {
            serde_json::from_value(serde_json::json!({
                "apiVersion": "builder.zeabur.com/v1alpha1",
                "kind": "ClusterBuilder",
                "metadata": {"name": "quantity-probe"},
                "spec": {
                    "type": "railpack",
                    "railpack": {
                        "image": "ghcr.io/zeabur-oss/builder-job:dev",
                        "resources": {"limits": {"memory": memory}},
                    },
                    "frontend": {"type": "dockerfile.v0"},
                },
            }))
            .unwrap()
        };
        let apply = |memory| Patch::Apply(with_memory(memory));
        builders
            .patch("quantity-probe", &dry_run, &apply("2Gi"))
            .await
            .unwrap();
        let refused = builders
            .patch("quantity-probe", &dry_run, &apply("lots"))
            .await;
        assert!(
            matches!(&refused, Err(kube::Error::Api(status)) if status.is_invalid()),
            "{refused:?}"
        );
    }
}
