---
default: major
---

# `registry.tokenKey` is now `registry.tokenSecretRef`, with `name` for the Secret

The chart names a Secret the way the `Build` CRD and Kubernetes do: `registry.tokenSecretRef` (`namespace`, `name`) replaces `registry.tokenKey` (`namespace`, `secret`). Rename the key and its `secret` field in your values before you upgrade. The chart fails to render while `registry.tokenSecretRef` is unset.
