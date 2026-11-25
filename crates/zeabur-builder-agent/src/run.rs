//! One run: take the repository's Dockerfile, else build the last plan again, else ask the agent.

use std::path::{Path, PathBuf};

use rig_core::catalog::{Catalog, ModelSpec};
use rig_core::providers::registry::{ConnectError, ConnectOptions, ProviderId};
use zeabur_builder_core::refusal::Refusal;

use crate::agent::Agent;
use crate::budget::{Limits, Spent};
use crate::federation;
use crate::http::HyperClient;
use crate::output::{self, WriteError};
use crate::plan::{Details, Info, Outcome, Plan, Reason, Source};
use crate::repo::{Repo, dockerignore, rules_of};
use crate::trial::{Attempt, Buildctl, SECRET_ENV_PREFIX, Secret, TrialBuild};

/// Writes a Dockerfile for a repository and trial-builds it on a buildkitd.
///
/// Exits 0 with a plan, 2 when it stopped with a reason, 1 on an error of its own. The
/// Dockerfile is printed and written to `<out>/Dockerfile`; the info object is the last line on
/// stdout and `<out>/info.json`.
#[derive(Debug, clap::Parser)]
#[command(version)]
pub(crate) struct Args {
    /// The repository.
    #[arg(long)]
    context: PathBuf,
    /// The buildkitd address, such as `tcp://buildkitd:1234`.
    #[arg(long)]
    buildkit: String,
    /// The directory with `ca.pem`, `cert.pem` and `key.pem` for buildkitd.
    #[arg(long)]
    tlsdir: Option<PathBuf>,
    /// Where the plan is written.
    #[arg(long, default_value = "/workspace/plan")]
    out: PathBuf,
    /// A directory with the last plan's `Dockerfile` (and `Dockerfile.dockerignore`), tried
    /// before the agent.
    #[arg(long)]
    previous: Option<PathBuf>,
    /// The provider, by Rig's name: `anthropic`, `openai`, `gcp.gemini`, `openrouter` …. It sets
    /// the endpoint, the API format and the key variable, such as `ANTHROPIC_API_KEY`.
    #[arg(long)]
    provider: String,
    /// The model, as the provider names it: `claude-opus-5-5`, or `anthropic/claude-opus-5.5` on
    /// OpenRouter.
    #[arg(long)]
    model: String,
    /// The model API's base URL, for a proxy or a self-hosted endpoint.
    #[arg(long)]
    base_url: Option<String>,
    /// Registries images may come from.
    #[arg(long = "registry", default_values = ["docker.io", "ghcr.io", "quay.io", "gcr.io"])]
    registries: Vec<String>,
    /// A build variable the trial builds get as a secret, by name; its value is in
    /// `ZB_SECRET_<name>`.
    #[arg(long = "secret")]
    secrets: Vec<String>,
}

/// The variable with the model API key. The build Job sets it from the namespace's Secret.
const API_KEY_ENV: &str = "ZB_MODEL_API_KEY";

/// Why a run ended with an error of the agent's own rather than a plan or a reason.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RunError {
    #[error("cannot open {}: {source}", path.display())]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot create {}: {source}", path.display())]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot remove {}: {source}", path.display())]
    Remove {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Output(#[from] WriteError),
}

/// Runs, writes the plan and its info, and returns whether there is a plan.
pub(crate) async fn run(args: Args) -> Result<bool, RunError> {
    let repo = Repo::new(&args.context).map_err(|source| RunError::Open {
        path: args.context.clone(),
        source,
    })?;
    let limits = Limits::default();
    let builder = Buildctl {
        addr: args.buildkit.clone(),
        tlsdir: args.tlsdir.clone(),
        context: args.context.clone(),
        // Outside `out`, so the plan folder holds only the plan.
        work: std::env::temp_dir().join(format!("zeabur-builder-agent-{}", std::process::id())),
        registries: args.registries.clone(),
        timeout: limits.per_build,
        secrets: args
            .secrets
            .iter()
            .map(|name| Secret {
                name: name.clone(),
                value: std::env::var(format!("{SECRET_ENV_PREFIX}{name}")).unwrap_or_default(),
            })
            .collect(),
    };
    let previous = match &args.previous {
        Some(dir) => read_previous(dir).await,
        None => None,
    };
    let key = std::env::var(API_KEY_ENV)
        .ok()
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty());
    let (outcome, spent) = plan(&repo, &builder, previous, limits, async || {
        connect(
            &args.provider,
            &args.model,
            args.base_url.as_deref(),
            key.clone(),
        )
        .await
    })
    .await;
    report(&args.out, outcome, spent.as_ref()).await
}

/// A Dockerfile and its `.dockerignore`.
type Files = (String, String);

async fn read_previous(dir: &Path) -> Option<Files> {
    let dockerfile = tokio::fs::read_to_string(dir.join("Dockerfile"))
        .await
        .ok()?;
    let ignore = tokio::fs::read_to_string(dir.join("Dockerfile.dockerignore"))
        .await
        .unwrap_or_default();
    Some((dockerfile, rules_of(&ignore).to_owned()))
}

/// `model` of `provider`, sending through [`HyperClient`], with `key`. Without it, Rig reads the
/// key from the provider's own variable, such as `ANTHROPIC_API_KEY`; without that, an Anthropic
/// or OpenAI run can exchange a service account token instead ([`federation`]). Bedrock always
/// assumes an IAM role ([`federation::aws`]). A model its catalogue does not list still connects.
async fn connect(
    provider: &str,
    model: &str,
    base_url: Option<&str>,
    key: Option<String>,
) -> Result<rig_core::DynModel<rig_core::operation::Completion>, Refusal<Reason>> {
    let catalog = Catalog::builtin();
    let provider = ProviderId::catalog(provider).ok_or_else(|| {
        let mut known: Vec<_> = ProviderId::all().map(|p| p.vendor()).collect();
        known.sort_unstable();
        known.dedup();
        Refusal::new(
            Reason::ModelFailed,
            format!(
                "unknown provider `{provider}`; use one of {}",
                known.join(", ")
            ),
        )
    })?;
    let spec = catalog.get(provider, model).map_or_else(
        || ModelSpec::new(provider, model),
        |found| found.spec.clone(),
    );
    if provider.vendor() == federation::aws::VENDOR {
        if key.is_some() {
            return Err(Refusal::new(
                Reason::ModelFailed,
                "aws_bedrock signs in with an IAM role, not an API key: set AWS_ROLE_ARN, \
                 AWS_WEB_IDENTITY_TOKEN_FILE and AWS_REGION",
            ));
        }
        return federation::aws::connect(model, base_url).await;
    }
    let http = HyperClient::new().map_err(|e| Refusal::new(Reason::ModelFailed, e.to_string()))?;
    if key.is_none()
        && let Some(source) = federation::from_env(provider.vendor())?
    {
        return federation::connect(source, catalog, &spec, http, base_url).await;
    }
    let mut options = ConnectOptions::new().http(http);
    if let Some(key) = key {
        options = options.api_key(key);
    }
    if let Some(base_url) = base_url {
        options = options.base_url(base_url);
    }
    catalog.connect_with(&spec, options).map_err(|e| {
        if let ConnectError::MissingKey { tried, .. } = &e {
            Refusal::new(
                Reason::ModelKeyMissing,
                format!("set {}", tried.join(" or ")),
            )
        } else {
            Refusal::new(Reason::ModelFailed, e.to_string())
        }
    })
}

/// The order: the repository's Dockerfile, the last plan if it still builds, the agent.
pub(crate) async fn plan<B, C>(
    repo: &Repo,
    builder: &B,
    previous: Option<Files>,
    limits: Limits,
    connect: C,
) -> (Outcome, Option<Spent>)
where
    B: TrialBuild + Clone,
    C: AsyncFnOnce()
        -> Result<rig_core::DynModel<rig_core::operation::Completion>, Refusal<Reason>>,
{
    if let Some(dockerfile) = repo.file("Dockerfile") {
        let plan = Plan {
            source: Source::Repository,
            dockerfile,
            dockerignore: repo.file("Dockerfile.dockerignore").unwrap_or_default(),
            details: Details::default(),
        };
        return (Outcome::Ready(plan), None);
    }
    let mut hint = None;
    if let Some((dockerfile, dockerignore)) = previous {
        match builder.build(&dockerfile, &dockerignore).await {
            Attempt::Built => {
                let plan = Plan {
                    source: Source::Previous,
                    dockerfile,
                    dockerignore,
                    details: Details::default(),
                };
                return (Outcome::Ready(plan), None);
            }
            Attempt::Failed { summary, .. } => {
                hint = Some(format!("```dockerfile\n{dockerfile}\n```\n\n{summary}"));
            }
        }
    }
    let model = match connect().await {
        Ok(model) => model,
        Err(Refusal { reason, message }) => {
            return (
                Outcome::NeedsUser {
                    reason,
                    detail: message,
                },
                None,
            );
        }
    };
    let agent = Agent {
        model,
        repo: repo.clone(),
        builder: builder.clone(),
        limits,
    };
    let run = agent.run(hint.as_deref()).await;
    (run.outcome, Some(run.spent))
}

/// Prints and writes the outcome; returns whether there is a plan.
async fn report(out: &Path, outcome: Outcome, spent: Option<&Spent>) -> Result<bool, RunError> {
    let write = |name: &str, text: String| {
        let path = out.join(name);
        async move {
            tokio::fs::write(&path, text)
                .await
                .map_err(|source| RunError::Write { path, source })
        }
    };
    tokio::fs::create_dir_all(out)
        .await
        .map_err(|source| RunError::CreateDir {
            path: out.to_path_buf(),
            source,
        })?;
    let mut info = Info {
        source: None,
        details: None,
        reason: None,
        detail: None,
        model_called: spent.is_some(),
        input_tokens: spent.map_or(0, |s| s.input_tokens),
        output_tokens: spent.map_or(0, |s| s.output_tokens),
    };
    let mut printed = None;
    let ready = match outcome {
        Outcome::Ready(plan) => {
            printed = Some(plan.dockerfile.clone());
            write("Dockerfile", plan.dockerfile).await?;
            // The repository's own Dockerfile keeps the repository's ignore rules as they are,
            // so a file left from an earlier run must not override them.
            if plan.source == Source::Repository && !plan.dockerignore.is_empty() {
                write("Dockerfile.dockerignore", plan.dockerignore).await?;
            } else if plan.source == Source::Repository {
                let stale = out.join("Dockerfile.dockerignore");
                if let Err(e) = tokio::fs::remove_file(&stale).await
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(RunError::Remove {
                        path: stale,
                        source: e,
                    });
                }
            } else {
                write("Dockerfile.dockerignore", dockerignore(&plan.dockerignore)).await?;
            }
            info.source = Some(plan.source);
            info.details = (plan.source == Source::Agent).then_some(plan.details);
            true
        }
        Outcome::NeedsUser { reason, detail } => {
            info.reason = Some(reason);
            info.detail = Some(detail);
            false
        }
    };
    let info = info.encode();
    write("info.json", info.clone()).await?;
    output::plan(printed.as_deref(), &info)?;
    Ok(ready)
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::disallowed_types,
        reason = "test fixtures are JSON of any shape"
    )]

    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;
    use crate::testing::{Dir, Replay, Script, failed, tool_use};

    async fn no_model()
    -> Result<rig_core::DynModel<rig_core::operation::Completion>, Refusal<Reason>> {
        panic!("the model was called");
    }

    #[tokio::test]
    async fn the_repository_s_dockerfile_comes_first() {
        let dir = Dir::new();
        dir.write("Dockerfile", "FROM scratch\n");
        let repo = Repo::new(dir.path()).unwrap();
        let previous = Some(("FROM busybox\n".to_owned(), String::new()));

        let (outcome, spent) = plan(
            &repo,
            &Script::default(),
            previous,
            Limits::default(),
            no_model,
        )
        .await;

        let Outcome::Ready(plan) = outcome else {
            panic!()
        };
        assert_eq!(
            (plan.source, plan.dockerfile.as_str()),
            (Source::Repository, "FROM scratch\n")
        );
        assert!(spent.is_none());
    }

    #[tokio::test]
    async fn the_last_plan_is_reused_while_it_builds() {
        let dir = Dir::new();
        let repo = Repo::new(dir.path()).unwrap();
        let previous = Some(("FROM busybox\n".to_owned(), "dist\n".to_owned()));

        let (outcome, spent) = plan(
            &repo,
            &Script::new([Attempt::Built]),
            previous,
            Limits::default(),
            no_model,
        )
        .await;

        let Outcome::Ready(plan) = outcome else {
            panic!()
        };
        assert_eq!(plan.source, Source::Previous);
        assert_eq!(plan.dockerignore, "dist\n");
        assert!(spent.is_none());
    }

    #[tokio::test]
    async fn a_last_plan_that_fails_is_the_agent_s_hint() {
        let dir = Dir::new();
        let repo = Repo::new(dir.path()).unwrap();
        let previous = Some(("FROM node:20.1-alpine\n".to_owned(), String::new()));
        let replay = Replay::new([tool_use(
            "1",
            "stop",
            &json!({"reason": "NoAppFound", "detail": "empty"}),
        )]);
        let script = Script::new([failed("manifest unknown")]);

        let (outcome, spent) = plan(&repo, &script, previous, Limits::default(), async || {
            Ok(replay.model())
        })
        .await;

        assert!(matches!(
            outcome,
            Outcome::NeedsUser {
                reason: Reason::NoAppFound,
                ..
            }
        ));
        assert_eq!(spent.unwrap().turns, 1);
        let first = &replay.requests.lock().unwrap()[0];
        assert!(first.contains("no longer builds"));
        assert!(first.contains("FROM node:20.1-alpine"));
        assert!(first.contains("manifest unknown"));
    }

    #[tokio::test]
    async fn no_key_names_the_vendor_s_variable() {
        // Assumes `HYPERBOLIC_API_KEY` is unset: removing a variable needs `unsafe`.
        let Err(error) = connect("hyperbolic", "some-model", None, None).await else {
            panic!()
        };
        assert_eq!(
            error,
            Refusal::new(Reason::ModelKeyMissing, "set HYPERBOLIC_API_KEY")
        );
    }

    #[tokio::test]
    async fn an_unknown_provider_lists_the_known_ones() {
        let Err(Refusal {
            reason,
            message: detail,
        }) = connect("gemini", "gemini-2.5-pro", None, None).await
        else {
            panic!()
        };
        assert_eq!(reason, Reason::ModelFailed);
        assert!(detail.contains("gcp.gemini"), "{detail}");
    }

    #[tokio::test]
    async fn a_model_the_catalogue_does_not_list_is_not_refused() {
        let Err(error) = connect("hyperbolic", "a-model-from-next-year", None, None).await else {
            panic!()
        };
        assert_eq!(error.reason, Reason::ModelKeyMissing);
    }

    #[tokio::test]
    async fn a_key_stands_in_for_the_vendor_s_variable() {
        let key = Some("sk-test".to_owned());

        assert!(connect("hyperbolic", "some-model", None, key).await.is_ok());
    }

    #[tokio::test]
    async fn the_repository_s_dockerfile_drops_an_earlier_ignore_file() {
        let out = Dir::new();
        out.write("Dockerfile.dockerignore", "dist\n");
        let plan = Plan {
            source: Source::Repository,
            dockerfile: "FROM busybox\n".to_owned(),
            dockerignore: String::new(),
            details: Details::default(),
        };

        assert!(
            report(out.path(), Outcome::Ready(plan), None)
                .await
                .unwrap()
        );

        assert!(!out.path().join("Dockerfile.dockerignore").exists());
    }

    #[tokio::test]
    async fn the_repository_s_own_ignore_file_goes_with_its_dockerfile() {
        let context = Dir::new();
        context.write("Dockerfile", "FROM busybox\n");
        context.write("Dockerfile.dockerignore", "secrets/\n");
        let repo = Repo::new(context.path()).unwrap();
        let out = Dir::new();

        let (outcome, _) = plan(&repo, &Script::default(), None, Limits::default(), no_model).await;
        report(out.path(), outcome, None).await.unwrap();

        let read = |name: &str| std::fs::read_to_string(out.path().join(name)).unwrap();
        assert_eq!(read("Dockerfile.dockerignore"), "secrets/\n");
    }

    #[tokio::test]
    async fn writes_the_plan_and_its_info() {
        let out = Dir::new();
        let plan = Plan {
            source: Source::Previous,
            dockerfile: "FROM busybox\n".to_owned(),
            dockerignore: "dist".to_owned(),
            details: Details::default(),
        };

        assert!(
            report(out.path(), Outcome::Ready(plan), None)
                .await
                .unwrap()
        );

        let read = |name: &str| std::fs::read_to_string(out.path().join(name)).unwrap();
        assert_eq!(read("Dockerfile"), "FROM busybox\n");
        assert_eq!(read("Dockerfile.dockerignore"), dockerignore("dist"));
        let info: serde_json::Value = serde_json::from_str(&read("info.json")).unwrap();
        assert_eq!(
            info,
            json!({"source": "previous", "modelCalled": false, "inputTokens": 0, "outputTokens": 0})
        );
    }
}
