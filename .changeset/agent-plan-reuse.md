---
default: minor
---

# A Build reuses its last plan instead of running the planner

After a run of a builder with a planner (`railpack` or `agent`) plans and builds, the build step pushes the plan folder (`/workspace/plan`) beside the Build's images, tagged `zeabur-plan-<Build uid>`, and the controller records it in `Build.status.plan` with the builder and repository of that run's Job, which the run reports in the new `BuildRun.status.builder` and `BuildRun.status.image`. Later runs build with that plan: when the controller creates a run's Job, it hands the Job the saved plan, and the Job unpacks it instead of running the planner, so the `agent` builder calls no model. Such a run reports `planSource: saved` and the plan's digest in `planDigest`. A Build with the annotation `builder.zeabur.com/plan: fresh`, or one that names another builder, plans every run, and so does a Build whose output repository changed. A run that reuses a plan does not need the `agent` builder's model key. If the saved plan no longer builds, the run fails; set the annotation to plan again. Railpack's non-root rewrite now goes to `/workspace/out/non-root`, and the agent's trial builds to a temporary folder, so the plan folder holds only the plan.
