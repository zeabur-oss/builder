---
default: minor
---

# A ClusterBuilder's planner and frontend name their type

`spec.type` is `dockerfile`, `railpack` with `railpack: {image, timeout, resources}`, or `agent` with `agent: {image, timeout, resources, model}`, in place of `spec.planner`. `spec.frontend.type` is `dockerfile.v0`, or `gateway.v0` with `source`. A `dockerfile` or `agent` builder with a `gateway.v0` frontend now reads the Dockerfile it names, as a Dockerfile frontend image. Rewrite custom ClusterBuilders in the new shape after the upgrade; the chart's own are already.
