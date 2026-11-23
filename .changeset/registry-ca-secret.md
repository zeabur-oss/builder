---
default: major
---

# The registry's CA is read from a Secret at run time

`registry.caSecret` (`namespace`, `secret`, `key`) names the Secret key that holds the registry's CA, and replaces `registry.ca` and `registry.caFrom`. The controller follows that Secret and publishes the CA alone in the Secret `registry-ca` of its Namespace. Each buildkitd mounts it, and every signed-in user may read it. A new CA reaches the controller, buildkitd and new build Jobs without a reinstall.

Upload Secrets no longer hold `ca.crt`. Read the CA from `registry-ca` instead.
