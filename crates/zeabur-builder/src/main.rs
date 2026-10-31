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
mod remote;
mod ssh;
mod tls;
mod webhook;

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use zeabur_builder_jobspec::buildkitd::Resources;

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
    /// How long the token lasts, in seconds.
    #[arg(long, default_value_t = 900)]
    ttl: u64,
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

async fn registry_token(args: RegistryTokenArgs) -> Result<String, String> {
    let client = kube::Client::try_default()
        .await
        .map_err(|e| format!("cannot reach Kubernetes: {e}"))?;
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
    Ok(signer.token(
        &grants,
        jiff::Timestamp::now(),
        Duration::from_secs(args.ttl),
    ))
}

#[derive(Debug, Args)]
struct ControllerArgs {
    /// The controller's namespace.
    #[arg(long, env = "POD_NAMESPACE")]
    namespace: String,
    /// The registry for `Build`s without `spec.output.image`, such as `registry.internal:5000`.
    #[arg(long, env = "ZEABUR_BUILDER_DEFAULT_REGISTRY")]
    default_registry: String,
    /// The default registry's CA, PEM, when a public CA did not sign its certificate. The `fetch`
    /// container of an archive source trusts it.
    #[arg(long, env = "ZEABUR_BUILDER_REGISTRY_CA")]
    registry_ca: Option<String>,
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
    /// Where the buildkitd of each namespace runs. Default: the controller's namespace.
    #[arg(long)]
    buildkitd_namespace: Option<String>,
    /// The buildkitd image, rootful.
    #[arg(
        long,
        default_value = "docker.io/moby/buildkit:v0.34.1@sha256:99368f3e46177c6405b9740e4461d91cee004128daeecca78d4ceac50ec1da5c"
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
    /// `true` shares the node's user namespace instead of a Pod user namespace; only for
    /// Kubernetes older than 1.33.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    buildkitd_host_users: bool,
    /// buildkitd's CPU request.
    #[arg(long, default_value = "100m")]
    buildkitd_cpu_request: String,
    /// buildkitd's memory request.
    #[arg(long, default_value = "256Mi")]
    buildkitd_memory_request: String,
    /// buildkitd's memory limit. Default: none.
    #[arg(long)]
    buildkitd_memory_limit: Option<String>,
    /// Seconds a namespace goes without builds before its buildkitd scales to zero.
    #[arg(long, default_value_t = 600)]
    buildkitd_idle: u64,
    /// How long a build may run, in seconds.
    #[arg(long, default_value_t = 3600)]
    job_deadline: i64,
    /// How long a finished Job is kept, in seconds, for log collectors.
    #[arg(long, default_value_t = 600)]
    job_ttl: i32,
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
            host_users: self.buildkitd_host_users,
            resources: Resources {
                cpu_request: Some(self.buildkitd_cpu_request),
                memory_request: Some(self.buildkitd_memory_request),
                memory_limit: self.buildkitd_memory_limit,
            },
            idle_after: Duration::from_secs(self.buildkitd_idle),
        };
        controller::Settings {
            namespace: self.namespace,
            daemon: Arc::new(daemon),
            max_concurrent: self.max_concurrent_builds,
            default_registry: self.default_registry,
            registry_ca: self.registry_ca.filter(|ca| !ca.trim().is_empty()),
            registry_http: self.registry_http,
            registry_token_key: self.registry_token_key,
            job_image: self.job_image,
            job_deadline_seconds: self.job_deadline,
            job_ttl_seconds: self.job_ttl,
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

async fn run_controller(args: ControllerArgs) -> Result<(), String> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_provider| "a TLS provider is already installed".to_owned())?;
    tracing_subscriber::fmt().json().with_target(false).init();
    let client = kube::Client::try_default()
        .await
        .map_err(|e| format!("cannot reach Kubernetes: {e}"))?;
    controller::run(client, args.settings()).await
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Crds => zeabur_builder_api::definitions_yaml()
            .map(Some)
            .map_err(|e| e.to_string()),
        Command::Controller(args) => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())
            .and_then(|runtime| runtime.block_on(run_controller(args)))
            .map(|()| None),
        Command::GithubApp(command) => rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .map_err(|_provider| "a TLS provider is already installed".to_owned())
            .and_then(|()| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())
            })
            .and_then(|runtime| runtime.block_on(github_app::run(command)))
            .map(Some),
        Command::RegistryToken(args) => rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .map_err(|_provider| "a TLS provider is already installed".to_owned())
            .and_then(|()| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())
            })
            .and_then(|runtime| runtime.block_on(registry_token(args)))
            .map(Some),
    };
    match result {
        Ok(Some(text)) => output::stdout_line(&text),
        Ok(None) => ExitCode::SUCCESS,
        Err(message) => {
            output::stderr_line(&format!("error: {message}"));
            ExitCode::FAILURE
        }
    }
}
