---
default: patch
---

# The agent planner can run without a model API key

With Anthropic and OpenAI, `zeabur-builder-agent` can exchange a Kubernetes service account token for a short-lived access token (workload identity federation). It reads the same variables as the provider's SDKs, such as `ANTHROPIC_FEDERATION_RULE_ID` or `OPENAI_IDENTITY_PROVIDER_ID`. An API key still comes first.
