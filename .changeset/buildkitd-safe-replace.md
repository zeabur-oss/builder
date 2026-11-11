---
default: patch
---

# A changed buildkitd setting takes effect once the daemon has been idle

buildkitd is a cache, so a running daemon keeps the settings it started with. When the cache size, storage class, build nodes, a namespace's build limits or the buildkitd spec of a new Builder version change, the first build after that namespace's buildkitd has scaled to zero (after `--buildkitd-idle`, 600 seconds by default) deletes it with its cache and creates it again with the new settings. A namespace that keeps building uses the old settings until then. The StatefulSet carries the SHA-256 of its spec in the annotation `builder.zeabur.com/spec-hash`. If Kubernetes rejects the new StatefulSet, the run fails with `InfraError` and the API server's message.
