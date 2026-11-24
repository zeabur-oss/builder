---
default: patch
---

# `pollInterval` is read in jiff's friendly format

The controller reads `triggers.pollInterval` with jiff. Every value the CRD pattern admitted still means the same, except a fraction before another unit (`1.5m30s`), units out of order (`30s1m`) and a repeated unit (`1s2s`). The controller takes such a value as unset and uses the default interval.
