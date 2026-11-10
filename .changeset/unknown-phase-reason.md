---
default: minor
---

# A `BuildRun` phase or reason that a reader does not know decodes as `Unknown`

`Phase` and `Reason` in `zeabur-builder-api` gain an `Unknown` value for readers built before a later value existed, and the `Build` condition reasons are typed as `ConditionReason`. The strings on the wire do not change, and the CRDs list `Unknown` for `phase` and `reason`. A reader released before this change still fails on a new value.
