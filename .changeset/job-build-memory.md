---
default: minor
---

# builder.zeabur.com/build-memory sets the build Job's memory limit too

A Namespace's `builder.zeabur.com/build-memory` now also sets the memory limit of each container of its build Jobs, held to `--buildkitd-memory-max` like buildkitd's. A build Job container killed for its memory fails the run with `OutOfMemory` and a message that names the annotation, where it failed with `PlanFailed`, `BuildFailed` or `InfraError` before.
