---
default: minor
---

# A Build's Ready follows its spec generation, and a run finds its Job by owner

A `BuildRun` records the `Build` generation it was created for, and editing a `Build`'s spec builds again. `Ready` is `True` only when the newest run of the current generation succeeded, `Reconciling` shows while one is unfinished, and `Stalled` shows while `SourceReady` or `BuilderReady` is `False`. After the upgrade, each `Build` builds once more.

A run adopts a Job at once when the Job names the run's uid as its controller, and reads its Pod by the Job's controller uid. Runs waiting for a buildkitd or a slot wake on changes of that buildkitd or of other runs, and a daemon Pod the scheduler cannot place counts as short of resources whatever its message.
