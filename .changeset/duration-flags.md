---
default: major
---

# The controller's time flags take durations

`--buildkitd-pending-grace`, `--buildkitd-idle`, `--job-deadline`, `--job-ttl` and the `--ttl` of `registry-token` take a duration such as `90s`, `10m` or `1h`, as the CRD fields do. A bare number is refused. If you pass these flags through `extraArgs`, change `600` to `10m`. The chart sets none of them.
