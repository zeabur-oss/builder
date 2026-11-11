---
default: minor
---

# The controller frees idle buildkitd when a run lacks memory or CPU, and hands its slot to a run that can start

When a run's buildkitd does not fit for lack of memory or CPU, the controller scales idle buildkitd in other namespaces to zero, one per check and longest idle first, until it fits. While it waits, the run's reason is `InsufficientResources`. After `--buildkitd-pending-grace` seconds (default 60), the run gives its cluster slot back if another queued run's namespace already has a ready buildkitd, so that run starts first. It then waits behind the other queued runs.
