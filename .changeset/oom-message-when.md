---
default: patch
---

# The OutOfMemory message says when a raised buildkitd limit applies

When buildkitd runs out of memory, the BuildRun message says that a new limit applies once buildkitd has been idle and is replaced. If the namespace already asks for more memory than the running buildkitd has, the message says that buildkitd still has the old limit.
