---
default: major
---

# The controller always runs buildkitd in a user namespace, and `--buildkitd-host-users` is removed

buildkitd's Pod always sets `hostUsers: false`, which needs Kubernetes 1.33 or later. The controller refuses to start when it is given `--buildkitd-host-users`, so delete it from the chart's `extraArgs`, whether it was `true` or `false`. A cluster older than 1.33 can no longer run Builder; upgrade the cluster first.
