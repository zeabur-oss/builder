---
default: minor
---

# A cluster-wide ceiling for each build's CPU and memory

`--buildkitd-cpu-max` and `--buildkitd-memory-max`, or the chart values `buildkitd.resources.max.cpu` and `buildkitd.resources.max.memory`, cap what a Namespace's `builder.zeabur.com/build-cpu` and `build-memory` can ask for. If unset, there is no ceiling. A build that asks for more runs at the ceiling, and its BuildRun gets a `BuildLimited` Event that says so.
