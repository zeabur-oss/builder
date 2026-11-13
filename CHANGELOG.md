# Changelog

All notable changes to Zeabur Builder. Versions follow [Semantic Versioning](https://semver.org/); each section comes from the change files in `.changeset/` (CONTRIBUTING.md, "Releases").

## 0.2.2 (2026-11-13)

### Features

#### A cluster-wide ceiling for each build's CPU and memory

`--buildkitd-cpu-max` and `--buildkitd-memory-max`, or the chart values `buildkitd.resources.max.cpu` and `buildkitd.resources.max.memory`, cap what a Namespace's `builder.zeabur.com/build-cpu` and `build-memory` can ask for. If unset, there is no ceiling. A build that asks for more runs at the ceiling, and its BuildRun gets a `BuildLimited` Event that says so.

#### Builds can run on a build node set aside by a taint

The chart values `builds.nodeSelector` and `builds.tolerations` decide which nodes run every namespace's buildkitd and every build Job; see docs/builds/build-nodes.mdx. With both empty, builds go wherever the scheduler puts them, as before.

A change to the node selector or tolerations reaches a namespace's buildkitd the way every other buildkitd setting does: once it has been idle and scaled to zero, the next build replaces it and its cache. A daemon counts as ready only when its Pod is of the revision the StatefulSet rolls out.

#### BuildRuns record their steps, the failed step, kstatus conditions and the rebuild request they answer

A finished BuildRun has `status.steps` (the last 20 BuildKit steps, each with `name`, `state`, `cached`, `startTime`, `completionTime` and an error `message`, with names and messages cut to 120 characters), `omittedSteps` (how many earlier steps were left out) and `failedStep` (the step that failed, also when `steps` leaves it out; `fetch` or `plan` when the run stopped there). The Job's termination message stays within 4096 bytes: it drops the oldest steps first, then base images, exposed ports, extra tags, the message and the phase times, and always keeps the result, reason, digest and failed step.

Every BuildRun has `Reconciling` (True while Pending, Planning or Building), `Stalled` (True on Failed or Cancelled, with the run's reason) and `Ready` (True on Succeeded, otherwise False) conditions, so `kubectl wait --for=condition=Ready buildrun/<name>` works. `status.observedRebuild` shows the run's own `builder.zeabur.com/rebuild` value.

#### ClusterBuilders show the controller's build settings in their status

Each `ClusterBuilder` has a status with `buildDefaults` (`requests` and `limits` of one build, each with `cpu` and `memory`), `buildCeiling` (`cpu` and `memory` from `--buildkitd-cpu-max` and `--buildkitd-memory-max`; a missing value means no ceiling), `defaultConcurrentBuilds` and a `Ready` condition. The controller writes it when it takes the lease and when a `ClusterBuilder` is created or changed. The chart's ClusterRole can now patch `clusterbuilders/status`.

#### The controller frees idle buildkitd when a run lacks memory or CPU, and hands its slot to a run that can start

When a run's buildkitd does not fit for lack of memory or CPU, the controller scales idle buildkitd in other namespaces to zero, one per check and longest idle first, until it fits. While it waits, the run's reason is `InsufficientResources`. After `--buildkitd-pending-grace` seconds (default 60), the run gives its cluster slot back if another queued run's namespace already has a ready buildkitd, so that run starts first. It then waits behind the other queued runs.

#### builder.zeabur.com/build-memory sets the build Job's memory limit too

A Namespace's `builder.zeabur.com/build-memory` now also sets the memory limit of each container of its build Jobs, held to `--buildkitd-memory-max` like buildkitd's. A build Job container killed for its memory fails the run with `OutOfMemory` and a message that names the annotation, where it failed with `PlanFailed`, `BuildFailed` or `InfraError` before.

#### Build Jobs have a memory limit

Each container of a build Job requests 128 MiB of memory and may use up to 512 MiB, so a build Job can no longer use up a small node's memory. Change it with the chart value `jobResources` or the controller flags `--job-memory-request` and `--job-memory-limit`. `RUN` steps run in buildkitd and keep `buildkitd.resources`.

#### Build logs are NDJSON events with step durations

The build container writes one JSON event per line: `started`, `log` (with `stream` `stdout` or `stderr`), `done` and `error` (with `seconds`), `cached`, and `message` for buildctl's own output. Every event carries its step name, and step numbers continue across Railpack's two `buildctl` runs. `kubectl logs` shows JSON; `jq -r '.text // .message // .name'` reads it as text. The `fetch` and `plan` steps keep their text output.

#### A `BuildRun` phase or reason that a reader does not know decodes as `Unknown`

`Phase` and `Reason` in `zeabur-builder-api` gain an `Unknown` value for readers built before a later value existed, and the `Build` condition reasons are typed as `ConditionReason`. The strings on the wire do not change, and the CRDs list `Unknown` for `phase` and `reason`. A reader released before this change still fails on a new value.

### Fixes

#### A changed buildkitd setting takes effect once the daemon has been idle

buildkitd is a cache, so a running daemon keeps the settings it started with. When the cache size, storage class, build nodes, a namespace's build limits or the buildkitd spec of a new Builder version change, the first build after that namespace's buildkitd has scaled to zero (after `--buildkitd-idle`, 600 seconds by default) deletes it with its cache and creates it again with the new settings. A namespace that keeps building uses the old settings until then. The StatefulSet carries the SHA-256 of its spec in the annotation `builder.zeabur.com/spec-hash`. If Kubernetes rejects the new StatefulSet, the run fails with `InfraError` and the API server's message.

#### A Namespace's buildkitd goes when the Namespace is deleted

The buildkitd StatefulSet, Service, NetworkPolicy and TLS Secret of a Namespace name that Namespace as their owner, so Kubernetes garbage collection deletes them, and with the StatefulSet the cache, as soon as the Namespace is gone. The controller no longer lists Namespaces every 30 seconds to find them, and the chart's ClusterRole no longer grants `list` on Namespaces; the Role in the controller's Namespace no longer grants `delete` on Services and NetworkPolicies. When a Namespace is deleted and created again with the same name, its next build waits until garbage collection has deleted the old daemon, then gets a new one with a new CA.

A daemon made by an earlier version gets its owner the next time its Namespace builds. To clean up after a Namespace that was deleted before that, delete its objects by hand: `kubectl -n <controller namespace> delete statefulset,service,networkpolicy,secret -l builder.zeabur.com/namespace=<namespace>`.

## 0.2.1 (2026-11-07)

### Fixes

#### v0.2.0 was never released; v0.2.1 has its changes

The release run of v0.2.0 failed after building, before it published anything, so v0.2.0 has a tag but no images, chart or release. v0.2.1 is the first release with the changes listed under 0.2.0.

## 0.2.0 (2026-11-07)

### Breaking Changes

#### The controller always runs buildkitd in a user namespace, and `--buildkitd-host-users` is removed

buildkitd's Pod always sets `hostUsers: false`, which needs Kubernetes 1.33 or later. The controller refuses to start when it is given `--buildkitd-host-users`, so delete it from the chart's `extraArgs`, whether it was `true` or `false`. A cluster older than 1.33 can no longer run Builder; upgrade the cluster first.

### Fixes

#### Builds start again after the buildkitd cache size or storage class changes

Kubernetes refuses to change a StatefulSet's volume, so after `--buildkitd-cache-size` or `--buildkitd-storage-class` changed, builds in a namespace that already had a buildkitd never started. The controller now deletes that buildkitd and its cache and creates it again, so the next build starts with an empty cache of the new size.

## 0.1.2 (2026-11-04)

### Changed

- Builds have limits by default. Each build gets 1 CPU and 1 GiB of memory, and one build runs at a time in each namespace and across the cluster. A run over a limit waits with the reason `QuotaExceeded`, and the oldest run starts first. In 0.1.1 a namespace ran up to 3 builds at once, with no CPU or memory limit. To allow more, set the chart values `buildkitd.resources.limits` and `maxConcurrentBuilds`, or set the Namespace annotations `builder.zeabur.com/build-cpu`, `builder.zeabur.com/build-memory` and `builder.zeabur.com/max-concurrent-builds` for one namespace ([values.yaml](https://github.com/zeabur-oss/builder/blob/main/deploy/chart/values.yaml)).
- A namespace's buildkitd gets one build's limits times the builds the namespace may run at once. Changing a limit restarts that buildkitd and stops the builds running on it, so change limits when no builds are running.
- A new run of a Build waits until the Job and Pod of its previous run have stopped. With `concurrency: Replace`, the cancelled build no longer overlaps the one that replaced it.
- The controller deletes only the repositories it pushed to for a Build. Images you push by hand to a `<namespace>/<name>` path stay when a Build of that name is deleted. Before it issues a push token, the controller records the repository in a ConfigMap in its own Namespace, and the chart's Role there now allows ConfigMaps ([registry tokens](https://oss.zeabur.com/docs/builder/operations/registry-tokens/#clean-up-images-and-sources)).
- Repositories pushed by 0.1.1 have no record. If you delete a Build that hasn't run since the upgrade, its images stay in the registry. Delete them by hand.
- The release page now shows this changelog section and a link to the documentation. Each image, index and chart digest is in the signed `digests.txt` asset, and the install and verify commands are on [Release artifacts and verification](https://oss.zeabur.com/docs/builder/operations/releases/).
- Images are pushed to GHCR by digest, so GHCR no longer gets `candidate-*` tags.

### Added

- A Build records the ports its image exposes. After a successful build, Builder reads the pushed image's config from the registry and writes its `ExposedPorts`, such as `["3000/tcp"]`, to `status.exposedPorts` of the BuildRun and the Build. Zeabur suggests these ports on the service page. Railpack images declare no ports and record none. If the read fails, the list stays empty and the build still succeeds.
- A build fails with the reason `OutOfMemory` when buildkitd is killed for using more memory than its limit. The message names the annotation and the controller flag to raise.

## 0.1.2-beta.1 (2026-11-03)

### Fixes

- Builds are limited by default to 1 CPU and 1 GiB each, one at a time across the cluster, and one active run per Build
- Images are pushed by digest with no `candidate-*` tags left in GHCR

#### The controller deletes only repositories it pushed to for a `Build`

Before it signs a token that pushes to a repository, it records the repository in a ConfigMap in its own Namespace, and cleanup reads those records instead of the registry catalog. Images pushed by hand to a `<namespace>/<name>` path are no longer deleted. The chart's Role in the controller's Namespace now allows ConfigMaps ([registry tokens](docs/operations/registry-tokens.mdx#clean-up-images-and-sources)).

#### A Build records the ports its image exposes

After a successful build, Builder reads the pushed image's config back from the registry and records its `ExposedPorts` in `status.exposedPorts` of the BuildRun and the Build, such as `["3000/tcp"]`. Zeabur suggests these ports on the service page. An image that declares none, such as a Railpack build, records none, and a failed read leaves the list empty without failing the build.

#### Release notes are the changelog section and a link to the documentation

The digests of every image, index and the chart are in the signed `digests.txt` asset ([releases](docs/operations/releases.mdx)).

## 0.1.2-beta.0 (2026-11-01)

### Fixes

- Images are pushed by digest with no `candidate-*` tags left in GHCR

#### The controller deletes only repositories it pushed to for a `Build`

Before it signs a token that pushes to a repository, it records the repository in a ConfigMap in its own Namespace, and cleanup reads those records instead of the registry catalog. Images pushed by hand to a `<namespace>/<name>` path are no longer deleted. The chart's Role in the controller's Namespace now allows ConfigMaps ([registry tokens](docs/operations/registry-tokens.mdx#clean-up-images-and-sources)).

#### Release notes are the changelog section and a link to the documentation

The digests of every image, index and the chart are in the signed `digests.txt` asset ([releases](docs/operations/releases.mdx)).

## 0.1.1 (2026-10-28)

The first published release. 0.1.0 was tagged, but its release workflow failed before publishing anything; it is withdrawn and was never published. 0.1.1 holds everything listed under 0.1.0, and:

### Changed

- Releases are built, signed and attested by the shared release workflow of [zeabur-oss/.github](https://github.com/zeabur-oss/.github). Verify signatures with `--certificate-identity-regexp '^https://github\.com/zeabur-oss/\.github/\.github/workflows/release\.yml@'`, `--certificate-github-workflow-repository zeabur-oss/builder` and `--certificate-github-workflow-ref refs/tags/v0.1.1`, and attestations with `--signer-workflow zeabur-oss/.github/.github/workflows/release.yml` ([releases](docs/operations/releases.mdx)).

### Fixed

- `RUN` steps no longer hang or time out on HTTPS where the Pod's MTU is under 1500, as under flannel's VXLAN on k3s: buildkitd clamps the TCP MSS of their connections to the Pod's route MTU. Before, a step's bridge had an MTU of 1500, full-size replies were dropped wherever the ICMP "fragmentation needed" did not reach the server, and `apk add` or `mise install` stalled.

## 0.1.0 (2026-10-28)

Withdrawn: tagged, never published. See 0.1.1.

### Added

- The `zeabur-builder` Helm chart installs everything a cluster needs: the CRDs, the controller, the `dockerfile` and `railpack` ClusterBuilders, buildkitd's configuration and its egress policy. Registry settings are chart values.
- Images for linux/amd64 and linux/arm64, published by digest and signed with Sigstore; the chart pins them by digest.
- A ValidatingAdmissionPolicy in the chart limits the controller's ServiceAccount to build Jobs and buildkitd StatefulSets (docs/operations/controller-permissions.mdx). `buildkitdImage` sets buildkitd's image.
- `spec.output.pushSecretRef`: a push Secret in the `Build`'s namespace for an image on another registry.
- `registry.tokenKey.namespace` and `registry.tokenKey.secret` are required. The chart fails at install and render time without them, not after an upload waits for a token that never comes. The controller requires `ZEABUR_BUILDER_REGISTRY_TOKEN_KEY` and exits at start without it. The default registry takes pushes only with the controller's tokens; anonymous pushes to `registry.host` are refused.
