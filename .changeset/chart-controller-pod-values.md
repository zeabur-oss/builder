---
default: patch
---

# Chart values for the controller's Pod

The chart takes `imagePullSecrets`, `podLabels`, `podAnnotations`, `podSecurityContext`, `nodeSelector`, `tolerations` and `affinity` for the controller's Pod. With the defaults, the Deployment is the same as before.
