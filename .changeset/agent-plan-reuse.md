---
default: minor
---

# A Build reuses its last plan instead of running the planner

After a run of a builder with a planner (`railpack` or `agent`) plans and builds, the build step pushes the plan folder (`/workspace/plan`) beside the Build's images, tagged `zeabur-plan-<Build uid>`, and the controller records it in `Build.status.plan` with the builder that wrote it. Later runs build with that plan: the controller sets `BuildRun.spec.plan` to its digest, and the run unpacks the plan instead of running the planner, so the `agent` builder calls no model. A Build with the annotation `builder.zeabur.com/plan: fresh`, or one that names another builder, plans every run. If the saved plan no longer builds, the run fails; set the annotation to plan again. Railpack's non-root rewrite now goes to `/workspace/out/non-root`, and the agent's trial builds to a temporary folder, so the plan folder holds only the plan.
