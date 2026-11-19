---
default: minor
---

# The chart installs the agent ClusterBuilder when agent.enabled is true

Set `agent.provider`, `agent.model` and `agent.apiKeySecret`, and create that Secret in each Build's namespace. The planner runs from the Job image, which now holds `zeabur-builder-agent`.
