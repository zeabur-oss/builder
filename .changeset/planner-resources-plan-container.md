---
default: patch
---

# The plan container takes the planner's resources

If a ClusterBuilder's planner sets `resources`, the build Job's `plan` container gets those requests and limits. Before, it always got the Job memory. A planner whose `resources` set no memory keeps the Job memory.
