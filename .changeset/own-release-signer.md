---
default: minor
---

# Releases are signed by builder's own release workflow

Verify images, the chart and `SHA256SUMS` with `--certificate-identity-regexp '^https://github\.com/zeabur-oss/builder/\.github/workflows/release\.yml@'`, and attestations with `--signer-workflow zeabur-oss/builder/.github/workflows/release.yml` ([releases](docs/operations/releases.mdx)). Releases up to v0.2.2 keep the identity of `zeabur-oss/.github`.
