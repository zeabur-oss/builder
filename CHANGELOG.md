# Changelog

All notable changes to Zeabur Builder. Versions follow [Semantic Versioning](https://semver.org/); each section comes from the change files in `.changeset/` (CONTRIBUTING.md, "Releases").

## 0.3.1 (2026-11-27)

### Features

#### Commit statuses link to the Build's page and say what happened in one line

Set the `builder.zeabur.com/details-url` annotation on a Build to an `http` or `https` URL, such as the dashboard page that shows it. Its commit statuses then link to that page as their details. The status descriptions are now one short line, such as `Built and pushed api` or `Build failed: [2/3] RUN npm ci`. The run's Events keep the tag, digest and full message.

### Fixes

#### The AI builder writes images that run as a non-root user

A trial build of the `agent` builder fails when its image runs as root or as a user name, and the model gets the reason. The model writes `USER` with a numeric UID other than 0, so the image starts under the restricted Pod Security profile. A run whose builds all run as root ends with `BuildKeepsFailing` instead of `Succeeded`. The last plan of a service is used again only when its image runs as a non-root UID.

#### The AI builder signs in through any OAuth 2.0 token endpoint

`spec.agent.model.auth.type: tokenExchange` sends a Kubernetes service account token to a token endpoint, such as Microsoft Entra ID, and uses the access token of the reply as a bearer token for the model API. The run gets a new access token before the old one expires. The reply must have `token_type` `Bearer` and `expires_in`.

#### Chart values for the controller's Pod

The chart takes `imagePullSecrets`, `podLabels`, `podAnnotations`, `podSecurityContext`, `nodeSelector`, `tolerations` and `affinity` for the controller's Pod. With the defaults, the Deployment is the same as before.

#### The plan container takes the planner's resources

If a ClusterBuilder's planner sets `resources`, the build Job's `plan` container gets those requests and limits. Before, it always got the Job memory. A planner whose `resources` set no memory keeps the Job memory.

#### The API server checks a planner's CPU and memory

`resources.requests` and `resources.limits` of a ClusterBuilder's planner take Kubernetes quantities. The API server refuses a value such as `memory: lots`, and takes a number such as `cpu: 1` without quotes.

## 0.3.0 (2026-11-24)

### Breaking Changes

#### The chart names the GitHub App, webhook TLS and image values like the registry values

Rename these chart values before you upgrade. The chart fails to render while an old key is set, and the error names the new key.

| Before | Now |
| --- | --- |
| `githubApp.secretName` | `githubApp.secretRef.name` |
| `githubApp.grantsName` | `githubApp.grantsConfigMapRef.name` |
| `webhooks.ingress.tlsSecretName` | `webhooks.ingress.tls.secretName` |
| `buildkitdImage: <image>` | `buildkitdImage: {repository, tag, digest}` |
| `railpackFrontend: <image>` | `railpackFrontend: {repository, tag, digest}` |

`buildkitdImage` and `railpackFrontend` take the same fields as `image`, `jobImage` and `railpackImage`. A reference such as `docker.io/moby/buildkit:v0.34.2@sha256:…` splits into `repository` (`docker.io/moby/buildkit`), `tag` (`v0.34.2`) and `digest` (`sha256:…`). Set a tag, a digest or both.

#### The CRDs refuse durations the controller cannot read

`triggers.pollInterval` and `planner.timeout` take hours, minutes, seconds and milliseconds, largest first and each once, such as `90s`, `1h30m` or `1.5h`. A fraction goes on the last unit only. The CRDs now refuse `1.5m30s`, `30s1m` and `1s2s` when you apply them. Before, the controller took such a `pollInterval` as unset and polled at the default interval. Write the value in the forms above.

#### The controller's time flags take durations

`--buildkitd-pending-grace`, `--buildkitd-idle`, `--job-deadline`, `--job-ttl` and the `--ttl` of `registry-token` take a duration such as `90s`, `10m` or `1h`, as the CRD fields do. A bare number is refused. If you pass these flags through `extraArgs`, change `600` to `10m`. The chart sets none of them.

#### The registry's CA is read from a Secret at run time

`registry.caSecretRef` (`namespace`, `name`, `key`) names the Secret key that holds the registry's CA, and replaces `registry.ca` and `registry.caFrom`. The controller follows that Secret and publishes the CA alone in the Secret `registry-ca` of its Namespace. Each buildkitd mounts it, and every signed-in user may read it. A new CA reaches the controller, buildkitd and new build Jobs without a reinstall. A running buildkitd restarts at its next build to trust the new CA and keeps its cache. Builds it runs at that moment fail.

Upload Secrets no longer hold `ca.crt`. Read the CA from `registry-ca` instead.

#### `registry.tokenKey` is now `registry.tokenSecretRef`, with `name` for the Secret

The chart names a Secret the way the `Build` CRD and Kubernetes do: `registry.tokenSecretRef` (`namespace`, `name`) replaces `registry.tokenKey` (`namespace`, `secret`). Rename the key and its `secret` field in your values before you upgrade. The chart fails to render while `registry.tokenSecretRef` is unset.

### Features

#### Builds fetch Git submodules

The `fetch` step fetches submodules at the commits the repository records. A submodule on the repository's `https://` host uses the same token. A submodule that cannot be fetched fails the step, and the message names its path.

### Fixes

#### The AI Dockerfile tool hides every file the build leaves out

The model's file tools and the build's `.dockerignore` now use one list of credential files. The tools also hide a `.git` file and files named `.pem` or `.key`.

#### `pollInterval` is read in jiff's friendly format

The controller reads `triggers.pollInterval` with jiff. Every value the CRD pattern admitted still means the same, except a fraction before another unit (`1.5m30s`), units out of order (`30s1m`) and a repeated unit (`1s2s`). The controller takes such a value as unset and uses the default interval.

#### The OutOfMemory message says when a raised buildkitd limit applies

When buildkitd runs out of memory, the BuildRun message says that a new limit applies once buildkitd has been idle and is replaced. If the namespace already asks for more memory than the running buildkitd has, the message says that buildkitd still has the old limit.

#### url 2.5.9 fixes RUSTSEC-2026-0592

`Url::origin` could overflow the stack on a crafted URL. The controller reaches `url` through its AI dependency.

## 0.2.5 (2026-11-22)

### Fixes

#### Upload Secrets carry the registry's CA

When the default registry has a CA of its own (`registry.ca` or `registry.caFrom`), each upload Secret holds it in `ca.crt`. An uploader that may read the Secret can now trust the registry without reading the registry's own Secret in another namespace.

## 0.2.4 (2026-11-21)

### Features

#### The agent's trial builds get the Build's variables

A variable in `spec.env` reaches the agent's trial builds as a BuildKit secret, as it reaches the final build, so a run that stopped with `NeedsBuildVariable` can finish after you set the variable. The model reads the variable names, not their values: the agent replaces a value in a trial build's output with the variable's name. `zeabur-builder-agent` takes `--secret NAME` with the value in `ZB_SECRET_NAME`.

#### The chart installs the agent ClusterBuilder when agent.enabled is true

Set `agent.provider`, `agent.model` and `agent.auth`, and create the Secret or ServiceAccount it names in each Build's namespace. The planner runs from the Job image, which now holds `zeabur-builder-agent`.

#### A ClusterBuilder of type agent writes the Dockerfile for a repository that has none

`agent.model` names the model provider, the model and how a run signs in: `auth.type: apiKey` with the Secret that holds the API key, or `anthropicFederation` or `openaiFederation` with the ServiceAccount whose token the run exchanges, in the Build's namespace. A run waits with `ModelKeyMissing` while the Secret, its key or the ServiceAccount is missing. `BuildRun.status.plan` shows where the Dockerfile came from, why the planner stopped, and the tokens it used.

#### A Build reuses its last plan instead of running the planner

After a run of a builder with a planner (`railpack` or `agent`) plans and builds, the build step pushes the plan folder (`/workspace/plan`) beside the Build's images, tagged `zeabur-plan-<Build uid>`, and the controller records it in `Build.status.plan` with the builder and repository of that run's Job, which the run reports in the new `BuildRun.status.builder` and `BuildRun.status.image`. Later runs build with that plan: when the controller creates a run's Job, it hands the Job the saved plan, and the Job unpacks it instead of running the planner, so the `agent` builder calls no model. Such a run reports `planSource: saved` and the plan's digest in `planDigest`. A Build with the annotation `builder.zeabur.com/plan: fresh`, or one that names another builder, plans every run, and so does a Build whose output repository changed. A run that reuses a plan does not need the `agent` builder's model key. If the saved plan no longer builds, the run fails; set the annotation to plan again. Railpack's non-root rewrite now goes to `/workspace/out/non-root`, and the agent's trial builds to a temporary folder, so the plan folder holds only the plan.

#### zeabur-builder-api builds without schemars when its schema feature is off

The default feature `schema` adds `JsonSchema` to every type and `definitions()`. A crate that only reads and writes the objects can set `default-features = false` and leaves out schemars, dyn-clone and ref-cast.

#### A Build's Ready follows its spec generation, and a run finds its Job by owner

A `BuildRun` records the `Build` generation it was created for, and editing a `Build`'s spec builds again. `Ready` is `True` only when the newest run of the current generation succeeded, `Reconciling` shows while one is unfinished, and `Stalled` shows while `SourceReady` or `BuilderReady` is `False`. After the upgrade, each `Build` builds once more.

A run adopts a Job at once when the Job names the run's uid as its controller, and reads its Pod by the Job's controller uid. Runs waiting for a buildkitd or a slot wake on changes of that buildkitd or of other runs, and a daemon Pod the scheduler cannot place counts as short of resources whatever its message.

#### One reason vocabulary and typed errors

A `BuildRun` waiting for the previous run to stop now reports the reason `PreviousRunRunning` instead of `QuotaExceeded`, and the `Ready` condition repeats the run's reason without a second vocabulary. Every error in the builder crates is a typed enum, and one `Refusal` carries a reason and a message.

#### A ClusterBuilder's planner and frontend name their type

`spec.type` is `dockerfile`, `railpack` with `railpack: {image, timeout, resources}`, or `agent` with `agent: {image, timeout, resources, model}`, in place of `spec.planner`. `spec.frontend.type` is `dockerfile.v0`, or `gateway.v0` with `source`. A `dockerfile` or `agent` builder with a `gateway.v0` frontend now reads the Dockerfile it names, as a Dockerfile frontend image. Rewrite custom ClusterBuilders in the new shape after the upgrade; the chart's own are already.

#### Iterator chains and shorter doc comments

Loops that only filter, map or fold are iterator chains, and long doc comments are shorter. Behaviour is unchanged.

#### A Build's source names its type

`spec.source` is a union: `type: git` with `git: {url, ref, credentialsRef, githubApp}`, or `type: archive` with `archive: {}`. `contextDir` and `dockerfile` stay beside `type`. Rewrite each Build in the new shape after the upgrade.

#### Conditions carry a typed type, and status times keep their precision

`zeabur-builder-api` is now published. A condition's `type` is the enum `ConditionType`, `ConditionReason` can be read back with an `Unknown` fallback, and every status time is a `jiff::Timestamp`. The CRD schema lists the allowed condition types.

#### The build container reads a typed plan

`zeabur-builder-job build` reads the builds as JSON from a file, instead of `buildctl` command lines it parsed again. The file is the Pod's `builder.zeabur.com/build-plan` annotation, mounted with a downward API volume, which the admission policy now allows for that annotation only.

A ClusterBuilder's `frontend.opts` now takes names of at most 128 characters and values of at most 512, so the largest plan fits in the 256 KiB a Pod's annotations hold. Shorten longer options before the upgrade.

### Fixes

#### The agent builder can sign in without a model API key

With Anthropic and OpenAI, a ClusterBuilder's `agent.model.auth` can be `anthropicFederation` or `openaiFederation`: the run exchanges a projected service account token for a short-lived access token (workload identity federation). The build Job runs as the ServiceAccount the builder names, and only the plan container mounts its token, for the provider's audience. The chart's Job admission policy allows such tokens and no API server token, and the controller can read ServiceAccounts. Run by hand, `zeabur-builder-agent` reads the same variables as the provider's SDKs, such as `ANTHROPIC_FEDERATION_RULE_ID` or `OPENAI_IDENTITY_PROVIDER_ID`; an API key still comes first.

#### A Build written with zeabur-builder-api leaves out default triggers and concurrency

A client no longer writes `triggers: {}` and `concurrency: Replace` when it sets neither, so it does not own those fields under server-side apply. The API server still fills in both defaults.

#### Typed JSON everywhere

Webhook bodies, Railpack plans, registry replies and the objects the daemon writes are read and built through typed structs. A webhook whose repository fields are not strings is refused with 400, like a body that is not JSON. clippy refuses `serde_json::Value`, `DynamicObject` and their accessors outside the few places that pass through JSON of any shape.

## 0.2.3 (2026-11-18)

### Features

#### Releases are signed by builder's own release workflow

Verify images, the chart and `SHA256SUMS` with `--certificate-identity-regexp '^https://github\.com/zeabur-oss/builder/\.github/workflows/release\.yml@'`, and attestations with `--signer-workflow zeabur-oss/builder/.github/workflows/release.yml` ([releases](docs/operations/releases.mdx)). Releases up to v0.2.2 keep the identity of `zeabur-oss/.github`.

### Fixes

#### The chart takes its version from Chart.yaml's version

The chart's default image tag is now its `version`, and `appVersion` is gone, so knope sets `Cargo.toml`, `Cargo.lock` and `Chart.yaml` itself.

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
