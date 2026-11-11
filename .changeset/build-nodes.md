---
default: minor
---

# Builds can run on a build node set aside by a taint

The chart values `builds.nodeSelector` and `builds.tolerations` decide which nodes run every namespace's buildkitd and every build Job; see docs/builds/build-nodes.mdx. With both empty, builds go wherever the scheduler puts them, as before.

When the node selector changes, the controller waits for the running builds of a namespace to finish. It then replaces that namespace's buildkitd and its cache, through the same path as other changes that an update cannot make. The namespace's next build starts with an empty cache.
