---
default: minor
---

# A Build's source names its type

`spec.source` is a union: `type: git` with `git: {url, ref, credentialsRef, githubApp}`, or `type: archive` with `archive: {}`. `contextDir` and `dockerfile` stay beside `type`. Rewrite each Build in the new shape after the upgrade.
