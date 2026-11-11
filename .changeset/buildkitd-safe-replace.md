---
default: patch
---

# buildkitd is replaced only when the new one is valid and no build uses the old one

When a change to the cache size or storage class cannot be applied to a Namespace's buildkitd StatefulSet, the controller first checks the new StatefulSet with a dry-run create. If Kubernetes rejects it, the old daemon and its cache stay and the run fails with `InfraError` and the API server's message. Otherwise the controller waits for the Namespace's running builds, deletes the old StatefulSet only if it is still the object it read (`uid` and `resourceVersion` preconditions), and creates the new one after the old Pod and cache claim are gone, so the new storage class takes effect. The chart's Role gains `get` on PersistentVolumeClaims in the controller's Namespace.
