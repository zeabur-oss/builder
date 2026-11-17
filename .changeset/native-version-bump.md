---
default: patch
---

# The chart takes its version from Chart.yaml's version

The chart's default image tag is now its `version`, and `appVersion` is gone, so knope sets `Cargo.toml`, `Cargo.lock` and `Chart.yaml` itself.
