---
default: minor
---

# Builds can run on a build node set aside by a taint

The chart values `builds.nodeSelector` and `builds.tolerations` decide which nodes run every namespace's buildkitd and every build Job; see docs/builds/build-nodes.mdx. With both empty, builds go wherever the scheduler puts them, as before.

A buildkitd whose settings changed is now deleted with its cache once it is idle at zero replicas, and created again from the current settings on the next build, instead of being patched in place. After upgrading, each namespace's first build runs with an empty cache.
