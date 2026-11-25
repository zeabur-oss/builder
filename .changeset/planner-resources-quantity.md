---
default: patch
---

# The API server checks a planner's CPU and memory

`resources.requests` and `resources.limits` of a ClusterBuilder's planner take Kubernetes quantities. The API server refuses a value such as `memory: lots`, and takes a number such as `cpu: 1` without quotes.
