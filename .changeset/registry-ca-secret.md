---
default: major
---

# The registry's CA is read from a Secret at run time

`registry.caSecretRef` (`namespace`, `name`, `key`) names the Secret key that holds the registry's CA, and replaces `registry.ca` and `registry.caFrom`. The controller follows that Secret and publishes the CA alone in the Secret `registry-ca` of its Namespace. Each buildkitd mounts it, and every signed-in user may read it. A new CA reaches the controller, buildkitd and new build Jobs without a reinstall. A running buildkitd restarts at its next build to trust the new CA and keeps its cache. Builds it runs at that moment fail.

Upload Secrets no longer hold `ca.crt`. Read the CA from `registry-ca` instead.
