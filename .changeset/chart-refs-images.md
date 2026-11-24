---
default: major
---

# The chart names the GitHub App, webhook TLS and image values like the registry values

Rename these chart values before you upgrade. The chart fails to render while an old key is set, and the error names the new key.

| Before | Now |
| --- | --- |
| `githubApp.secretName` | `githubApp.secretRef.name` |
| `githubApp.grantsName` | `githubApp.grantsConfigMapRef.name` |
| `webhooks.ingress.tlsSecretName` | `webhooks.ingress.tls.secretName` |
| `buildkitdImage: <image>` | `buildkitdImage: {repository, tag, digest}` |
| `railpackFrontend: <image>` | `railpackFrontend: {repository, tag, digest}` |

`buildkitdImage` and `railpackFrontend` take the same fields as `image`, `jobImage` and `railpackImage`. A reference such as `docker.io/moby/buildkit:v0.34.2@sha256:…` splits into `repository` (`docker.io/moby/buildkit`), `tag` (`v0.34.2`) and `digest` (`sha256:…`). Set a tag, a digest or both.
