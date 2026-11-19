---
default: minor
---

# A ClusterBuilder of type agent writes the Dockerfile for a repository that has none

`planner.model` names the model provider, the model and the Secret that holds its API key, in the Build's namespace. A run waits with `ModelKeyMissing` while the Secret or its key is missing. `BuildRun.status.plan` shows where the Dockerfile came from, why the planner stopped, and the tokens it used.
