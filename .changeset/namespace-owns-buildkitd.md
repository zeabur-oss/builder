---
default: patch
---

# A Namespace's buildkitd goes when the Namespace is deleted

The buildkitd StatefulSet, Service, NetworkPolicy and TLS Secret of a Namespace name that Namespace as their owner, so Kubernetes garbage collection deletes them, and with the StatefulSet the cache, as soon as the Namespace is gone. The controller no longer lists Namespaces every 30 seconds to find them, and the chart's ClusterRole no longer grants `list` on Namespaces; the Role in the controller's Namespace no longer grants `delete` on Services and NetworkPolicies. When a Namespace is deleted and created again with the same name, its next build waits until garbage collection has deleted the old daemon, then gets a new one with a new CA.

A daemon made by an earlier version gets its owner the next time its Namespace builds. To clean up after a Namespace that was deleted before that, delete its objects by hand: `kubectl -n <controller namespace> delete statefulset,service,networkpolicy,secret -l builder.zeabur.com/namespace=<namespace>`.
