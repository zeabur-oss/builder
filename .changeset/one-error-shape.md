---
default: minor
---

# One reason vocabulary and typed errors

A `BuildRun` waiting for the previous run to stop now reports the reason `PreviousRunRunning` instead of `QuotaExceeded`, and the `Ready` condition repeats the run's reason without a second vocabulary. Every error in the builder crates is a typed enum, and one `Refusal` carries a reason and a message.
