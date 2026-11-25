---
default: patch
---

# The agent planner signs in to Azure OpenAI with a service account token

With `auth.type: azureFederation`, an `agent` builder on the `azure.openai` provider exchanges the run's service account token for a Microsoft Entra access token, so no Namespace stores an Azure OpenAI key. `zeabur-builder-agent` does the same when `AZURE_TENANT_ID`, `AZURE_CLIENT_ID` and `AZURE_FEDERATED_TOKEN_FILE` are set and no Azure key is.
