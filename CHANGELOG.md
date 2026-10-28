# Changelog

All notable changes to Zeabur Builder. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

## [0.1.1] - 2026-10-28

The first published release. 0.1.0 was tagged, but its release workflow failed before publishing
anything; it is withdrawn and was never published. 0.1.1 holds everything listed under 0.1.0, and:

### Changed

- Releases are built, signed and attested by the shared release workflow of
  [zeabur-oss/.github](https://github.com/zeabur-oss/.github). Verify signatures with
  `--certificate-identity-regexp '^https://github\.com/zeabur-oss/\.github/\.github/workflows/release\.yml@'`,
  `--certificate-github-workflow-repository zeabur-oss/builder` and
  `--certificate-github-workflow-ref refs/tags/v0.1.1`, and attestations with
  `--signer-workflow zeabur-oss/.github/.github/workflows/release.yml`
  ([releases](docs/operations/releases.mdx)).

## [0.1.0] - 2026-10-28 [YANKED]

Withdrawn: tagged, never published. See 0.1.1.

### Added

- The `zeabur-builder` Helm chart installs everything a cluster needs: the CRDs, the controller,
  the `dockerfile` and `railpack` ClusterBuilders, buildkitd's configuration and its egress
  policy. Registry settings are chart values.
- Images for linux/amd64 and linux/arm64, published by digest and signed with Sigstore; the chart
  pins them by digest.
- A ValidatingAdmissionPolicy in the chart limits the controller's ServiceAccount to build Jobs
  and buildkitd StatefulSets (docs/operations/controller-permissions.mdx). `buildkitdImage` sets buildkitd's
  image.
- `spec.output.pushSecretRef`: a push Secret in the `Build`'s namespace for an image on another
  registry.
- `registry.tokenKey.namespace` and `registry.tokenKey.secret` are required. The chart fails at
  install and render time without them, not after an upload waits for a token that never comes.
  The controller requires `ZEABUR_BUILDER_REGISTRY_TOKEN_KEY` and exits at start without it.
  The default registry takes pushes only with the controller's tokens; anonymous pushes to
  `registry.host` are refused.
