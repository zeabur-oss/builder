---
default: minor
---

# Builds can run on a build node set aside by a taint

The chart values `builds.nodeSelector` and `builds.tolerations` decide which nodes run every namespace's buildkitd and every build Job; see docs/builds/build-nodes.mdx. With both empty, builds go wherever the scheduler puts them, as before.

When the node selector changes, the controller waits for the running builds of a namespace to finish. It then replaces that namespace's buildkitd and its cache, through the same path as other changes that an update cannot make. The namespace's next build starts with an empty cache.

A change to the tolerations waits for the running builds of a namespace in the same way, and then restarts its buildkitd without deleting the cache. A daemon counts as ready only when its Pod is of the revision the StatefulSet rolls out, so a build does not start on a Pod that the update is about to replace.
