---
default: patch
---

# The agent builder can sign in without a model API key

With Anthropic and OpenAI, a ClusterBuilder's `agent.model.auth` can be `anthropicFederation` or `openaiFederation`: the run exchanges a projected service account token for a short-lived access token (workload identity federation). The build Job runs as the ServiceAccount the builder names, and only the plan container mounts its token, for the provider's audience. The chart's Job admission policy allows such tokens and no API server token, and the controller can read ServiceAccounts. Run by hand, `zeabur-builder-agent` reads the same variables as the provider's SDKs, such as `ANTHROPIC_FEDERATION_RULE_ID` or `OPENAI_IDENTITY_PROVIDER_ID`; an API key still comes first.
