---
name: zeabur-builder-dev
description: What is special about Zeabur Builder — its extra checks, how the images are compiled, and its Kubernetes scale rules. Use before changing any Rust, chart, Nix or CI file in this repo.
---

# Zeabur Builder

`CONTRIBUTING.md` has the crate levels, the generated files and the cluster job.

## Checks CI runs beyond fmt, clippy, tests and `cargo deny`

```sh
nix develop -c bash -c "bun install --frozen-lockfile && node_modules/.bin/zeabur-oss-levels"
nix develop -c ./scripts/check-chart.sh
```

## Images

`scripts/build-images.sh` compiles the image binaries itself, never through kache. CI builds them
with `ZEABUR_BUILDER_PROFILE=ci`; a release uses `release`. The controller image ships the CA
bundle of `ZEABUR_BUILDER_CA_BUNDLE`, so `webpki-roots` stays banned in `deny.toml`.

## Scale

- Watch with kube-rs `watcher`, `reflector` or `Controller`.
- One LIST of the namespace, selected by label, replaces a GET per object.
