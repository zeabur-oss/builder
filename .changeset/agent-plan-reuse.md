---
default: minor
---

# Builds record their plan, and the agent builder reuses its last good Dockerfile

After a run of a builder with a planner (`railpack` or `agent`) builds and pushes its image, the build step pushes the folder the planner wrote beside the Build's images, tagged `zeabur-plan-<Build uid>`, and the controller records it in `Build.status.plan` as `<repository>@<digest>`. The next run unpacks it into `/workspace/previous` before its planner starts. The `agent` builder builds that plan first, with the port, language and variables the agent reported, and calls the model only when it no longer builds. The planner no longer gets registry credentials.
