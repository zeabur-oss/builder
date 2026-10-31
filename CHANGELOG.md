# Changelog

All notable changes to Zeabur Builder. Versions follow [Semantic Versioning](https://semver.org/).

## Unreleased

### Changed

- Release notes are the changelog section and a link to the documentation. The digests of every image, index and the chart are in the signed `digests.txt` asset ([releases](docs/operations/releases.mdx)).
- Images are pushed by digest with no `candidate-*` tags left in GHCR.

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
