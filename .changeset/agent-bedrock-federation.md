---
default: patch
---

# The agent planner calls Amazon Bedrock with an IAM role

With `auth.type: awsFederation`, an `agent` builder on the `aws_bedrock` provider assumes an IAM role with the run's service account token and signs each Bedrock request with the role's temporary credentials, so no Namespace stores an AWS key. `zeabur-builder-agent` does the same with `AWS_ROLE_ARN`, `AWS_WEB_IDENTITY_TOKEN_FILE` and `AWS_REGION`.
