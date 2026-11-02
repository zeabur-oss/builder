#!/usr/bin/env bash
# Checks the crate levels in Cargo.toml [workspace.metadata]: a workspace crate may depend only on
# crates of a lower level, and a pure crate only on pure crates and the allowed third-party list.
set -euo pipefail

metadata=$(cargo metadata --format-version 1 --no-deps --locked)
levels=$(jq -c '.metadata.levels' <<<"$metadata")
pure=$(jq -c '.metadata.pure.crates' <<<"$metadata")
# Third-party crates a pure crate may use: data types and hashing, no I/O.
allowed='["k8s-openapi","kube","kube_quantity","schemars","serde","serde_json","sha2","thiserror","pretty_assertions"]'

failed=0
while IFS=$'\t' read -r crate dep kind; do
  crate_level=$(jq -r --arg c "$crate" '.[$c] // empty' <<<"$levels")
  if [[ -z $crate_level ]]; then
    echo "$crate has no level in [workspace.metadata.levels]" >&2
    failed=1
    continue
  fi
  dep_level=$(jq -r --arg c "$dep" '.[$c] // empty' <<<"$levels")
  if [[ -n $dep_level ]] && ((dep_level >= crate_level)); then
    echo "$crate (level $crate_level) depends on $dep (level $dep_level)" >&2
    failed=1
  fi
  if jq -e --arg c "$crate" 'index($c)' <<<"$pure" >/dev/null &&
    [[ -z $dep_level && $kind != dev ]] &&
    ! jq -e --arg d "$dep" 'index($d)' <<<"$allowed" >/dev/null; then
    echo "$crate is pure but depends on $dep" >&2
    failed=1
  fi
done < <(jq -r '.packages[] | .name as $n | .dependencies[] | [$n, .name, (.kind // "normal")] | @tsv' <<<"$metadata")

if ((failed)); then
  exit 1
fi
echo "levels ok"
