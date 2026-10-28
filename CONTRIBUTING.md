# Contributing

## Developer Certificate of Origin

This project uses the [Developer Certificate of Origin 1.1](DCO). Sign off every commit to
certify that you wrote it or otherwise have the right to submit it under the project licence:

```sh
git commit -s
```

CI rejects pull requests whose commits lack a `Signed-off-by` line matching the author. Commit
messages follow [Conventional Commits](https://www.conventionalcommits.org/).

## Before you open a pull request

CI is the shared workflow of [zeabur-oss/.github](https://github.com/zeabur-oss/.github): it runs
every job of `scripts/ci.sh --list` as `scripts/ci.sh <job>`, which enters the Nix dev shell
itself, so `scripts/ci.sh clippy` runs a job exactly as CI does. Or run what CI runs, inside
`nix develop`:

```sh
cargo fmt --all --check && nixfmt --check flake.nix && shellcheck scripts/*.sh && actionlint
cargo lint                                   # clippy, all targets and features, -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items
cargo nextest run --workspace
cargo deny --locked check
./scripts/check-levels.sh
./scripts/check-chart.sh
```

Generated files are committed and checked by tests. Regenerate them with:

```sh
cargo run -q -p zeabur-builder -- crds > deploy/chart/crds/crds.yaml
ZEABUR_BUILDER_BLESS=1 cargo nextest run -p zeabur-builder-jobspec   # build Job golden files
```

Review a golden-file diff like code: it is exactly what will run in every build.

Design rationale lives next to the code, in rustdoc: state the decision and its reason, and
cite the specification, upstream source or prior art it rests on. Never cite something you have
not read; write "not verified" instead. Documentation is written in English.

## Crate levels

A crate may depend only on crates of a lower level (`[workspace.metadata.levels]` in
`Cargo.toml`, checked by `scripts/check-levels.sh`):

| Level | Crate | Holds |
|---|---|---|
| 0 | `zeabur-builder-core` | Repository keys, tags, routing, build results |
| 1 | `zeabur-builder-api` | The custom resources |
| 2 | `zeabur-builder-jobspec` | The build Job |
| 3 | `zeabur-builder` | The binary |

Levels 0 to 2 are pure: no I/O, no clock, no randomness. The controller's decisions are
functions of their inputs, so they are tested without a cluster.

## Toolchain, lints and dependencies

Shared with [Zeabur Identity](https://github.com/zeabur-oss/identity):

- `flake.lock` is the only place the compiler version is set: the newest nightly in the locked
  rust-overlay that has every component and target listed in `flake.nix`. Renovate's lock file
  maintenance (`renovate.json`) moves it every Monday.
- Lints are set once in `Cargo.toml`. All are `warn`, and `cargo lint` turns them into errors.
  Suppress with `#[expect(lint, reason = "…")]`. Only the binary's `output` module writes to
  stdout or stderr.
- `cargo deny check` enforces `deny.toml`: OSI-approved licences, crates.io only, aws-lc-rs as
  the only cryptographic backend, no advisories.
- Cargo does not pick a release younger than seven days (`.cargo/config.toml`).

## Compiler cache (optional)

The repository sets up no compiler cache. To share compiled crates between checkouts and
worktrees on your machine, install [kache](https://ninja.kunobi.com/docs/kache/getting-started/c-cpp)
and set `RUSTC_WRAPPER=kache` in your shell profile; `nix develop` keeps it. The C code that build
scripts compile (aws-lc-sys) goes through kache too, with no further setup.
`scripts/build-images.sh` unsets `RUSTC_WRAPPER`, so image binaries are always compiled.

## Tests against a cluster

`scripts/ci.sh kubernetes` is the cluster job of CI. It creates the k3d cluster `zeabur-builder`
with its own kubeconfig (`target/ci/kubeconfig`), builds the images, installs the chart from
`deploy/chart`, builds the sample Builds from Git (a Dockerfile in `team-a` and `team-b`, Railpack
in `team-a`), waits for the two samples that fail on purpose, runs `scripts/example-isolation.sh`,
`scripts/example-admission.sh` and `scripts/example-archive.sh`, and deletes the cluster. Run it on a Linux machine with Docker; `ZEABUR_CI_KEEP=1` keeps the
cluster. It keeps the log of every Pod, the build Pods included, in `$CI_ARTIFACTS/pods`
(`target/ci/artifacts` by default; the artifact `ci-kubernetes` in CI). When it fails, it also
keeps the Builds, BuildRuns and events there, and prints each buildkitd's network and the result of
a one-step build that fetches from a `RUN` step.

To work against a cluster by hand, use a disposable one with its own kubeconfig, never your normal
context:

```sh
export KUBECONFIG=$(mktemp -d)/kubeconfig
k3d cluster create zeabur-builder
scripts/build-images.sh zeabur-builder dev
K3D_CLUSTER=zeabur-builder scripts/deploy-example.sh k3d-zeabur-builder team-a/whoami team-a/hello-python
```

`scripts/deploy-example.sh` starts the test registry in `deploy/examples/registry.yaml` with a
new token key, creates the namespaces in `NAMESPACES` (default `team-a team-b`), installs the
chart, applies `deploy/examples/builds.yaml` and waits for each named Build's
`status.latestImage`. It stops at the first failed `BuildRun`.

```sh
kubectl -n team-a get build -o wide        # Ready, Revision, Image
kubectl -n team-a get buildrun             # Phase, Reason, Started, Finished
kubectl -n team-a logs job/<buildrun>      # buildctl progress
k3d cluster delete zeabur-builder
```

## Releases

The release workflow is the shared one of [zeabur-oss/.github](https://github.com/zeabur-oss/.github);
its stages are `scripts/release.sh check|build|publish|assets|verify`. To release `vX.Y.Z`:

1. Set the version in `Cargo.toml` (`[workspace.package]`, then `cargo update -w` for
   `Cargo.lock`) and in `deploy/chart/Chart.yaml` (`version` and `appVersion`), update the images
   that `scripts/check-chart.sh` expects, add the `## [X.Y.Z]` section to `CHANGELOG.md`, and merge
   to `main`.
2. Optionally rehearse: run the Release workflow by hand with the version. It builds and smokes
   the images and publishes nothing.
3. Once CI has passed on `main` for that commit, push the tag `vX.Y.Z` on it; the release checks
   CI and stops otherwise. The workflow leaves a draft release; check it and publish it.
