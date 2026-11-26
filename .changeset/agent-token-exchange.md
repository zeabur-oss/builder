---
default: patch
---

# The AI builder signs in through any OAuth 2.0 token endpoint

`spec.agent.model.auth.type: tokenExchange` sends a Kubernetes service account token to a token endpoint, such as Microsoft Entra ID, and uses the access token of the reply as a bearer token for the model API. The run gets a new access token before the old one expires. The reply must have `token_type` `Bearer` and `expires_in`.
