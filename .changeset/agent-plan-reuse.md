---
default: minor
---

# The agent builder reuses its last good Dockerfile

After a run whose agent wrote the Dockerfile builds and pushes its image, the build step pushes the plan beside the Build's images, tagged `zeabur-plan-<Build uid>`, and the controller records it in `Build.status.plan` as `<repository>@<digest>`. The next run builds that plan first, with the port, language and variables the agent reported, and calls the model only when it no longer builds.
