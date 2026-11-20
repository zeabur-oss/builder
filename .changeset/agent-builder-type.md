---
default: minor
---

# A ClusterBuilder of type agent writes the Dockerfile for a repository that has none

`agent.model` names the model provider, the model and how a run signs in: `auth.type: apiKey` with the Secret that holds the API key, or `anthropicFederation` or `openaiFederation` with the ServiceAccount whose token the run exchanges, in the Build's namespace. A run waits with `ModelKeyMissing` while the Secret, its key or the ServiceAccount is missing. `BuildRun.status.plan` shows where the Dockerfile came from, why the planner stopped, and the tokens it used.
