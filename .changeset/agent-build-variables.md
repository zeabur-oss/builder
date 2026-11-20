---
default: minor
---

# The agent's trial builds get the Build's variables

A variable in `spec.env` reaches the agent's trial builds as a BuildKit secret, as it reaches the final build, so a run that stopped with `NeedsBuildVariable` can finish after you set the variable. The model reads the variable names, not their values: the agent replaces a value in a trial build's output with the variable's name. `zeabur-builder-agent` takes `--secret NAME` with the value in `ZB_SECRET_NAME`.
