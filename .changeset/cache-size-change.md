---
default: patch
---

# Builds start again after the buildkitd cache size or storage class changes

Kubernetes refuses to change a StatefulSet's volume, so after `--buildkitd-cache-size` or `--buildkitd-storage-class` changed, builds in a namespace that already had a buildkitd never started. The controller now deletes that buildkitd and its cache and creates it again, so the next build starts with an empty cache of the new size.
