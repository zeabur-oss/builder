---
default: minor
---

# Build Jobs have a memory limit

Each container of a build Job requests 128 MiB of memory and may use up to 512 MiB, so a build Job can no longer use up a small node's memory. Change it with the chart value `jobResources` or the controller flags `--job-memory-request` and `--job-memory-limit`. `RUN` steps run in buildkitd and keep `buildkitd.resources`.
