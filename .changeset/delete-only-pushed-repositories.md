---
default: patch
---

# The controller deletes only repositories it pushed to for a `Build`

Before it signs a token that pushes to a repository, it records the repository in a ConfigMap in its own Namespace, and cleanup reads those records instead of the registry catalog. Images pushed by hand to a `<namespace>/<name>` path are no longer deleted. The chart's Role in the controller's Namespace now allows ConfigMaps ([registry tokens](docs/operations/registry-tokens.mdx#clean-up-images-and-sources)).
