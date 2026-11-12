---
default: minor
---

# Builds can run on a build node set aside by a taint

The chart values `builds.nodeSelector` and `builds.tolerations` decide which nodes run every namespace's buildkitd and every build Job; see docs/builds/build-nodes.mdx. With both empty, builds go wherever the scheduler puts them, as before.

A change to the node selector or tolerations reaches a namespace's buildkitd the way every other buildkitd setting does: once it has been idle and scaled to zero, the next build replaces it and its cache. A daemon counts as ready only when its Pod is of the revision the StatefulSet rolls out.
