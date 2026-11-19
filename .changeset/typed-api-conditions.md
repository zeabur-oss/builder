---
default: minor
---

# Conditions carry a typed type, and status times keep their precision

`zeabur-builder-api` is now published. A condition's `type` is the enum `ConditionType`, `ConditionReason` can be read back with an `Unknown` fallback, and every status time is a `jiff::Timestamp`. The CRD schema lists the allowed condition types.
