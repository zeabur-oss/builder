---
name: zeabur-builder-dev
description: Development loop for the Zeabur Builder repository — the checks CI runs and the engineering standards for comments, commits, tests, design and scale. Use before changing any Rust, chart, Nix or CI file in this repo.
---

# Zeabur Builder development loop

Follow the shared skills (install: `npx skills add zeabur-oss/skills --skill <name>`;
`skills-lock.json` pins them): `zeabur-oss-rust` (Nix toolchain, workspace, lints, common
checks), `zeabur-oss-standards` (with [`STANDARDS.md`](STANDARDS.md) here),
`zeabur-oss-dependencies`, `zeabur-oss-workflow`, `zeabur-oss-cluster-tests`. Crate levels,
generated files and the cluster job are in `CONTRIBUTING.md`.

## Checks (what CI runs, beyond `zeabur-oss-rust`'s)
```sh
nix develop -c ./scripts/check-levels.sh
nix develop -c ./scripts/check-chart.sh
```

## Comments
A comment says only what the code cannot: a quirk and its cause, a workaround, or the
specification or upstream source behind behaviour that must not change, cited in rustdoc.
Usually one line; no design rationale, history or measurements.
