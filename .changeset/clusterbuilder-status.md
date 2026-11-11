---
default: minor
---

# ClusterBuilders show the controller's build settings in their status

Each `ClusterBuilder` has a status with `buildDefaults` (`requests` and `limits` of one build, each with `cpu` and `memory`), `buildCeiling` (`cpu` and `memory` from `--buildkitd-cpu-max` and `--buildkitd-memory-max`; a missing value means no ceiling), `defaultConcurrentBuilds` and a `Ready` condition. The controller writes it when it takes the lease and when a `ClusterBuilder` is created or changed. The chart's ClusterRole can now patch `clusterbuilders/status`.
