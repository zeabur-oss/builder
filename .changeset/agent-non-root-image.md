---
default: patch
---

# The AI builder writes images that run as a non-root user

A trial build of the `agent` builder fails when its image runs as root or as a user name, and the model gets the reason. The model writes `USER` with a numeric UID other than 0, so the image starts under the restricted Pod Security profile. A run whose builds all run as root ends with `BuildKeepsFailing` instead of `Succeeded`. The last plan of a service is used again only when its image runs as a non-root UID.
