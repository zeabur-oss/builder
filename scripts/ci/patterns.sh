#!/usr/bin/env bash
# Fails on patterns this repository replaced. Each rule has a violating example, which must match
# its regex, and a reason that names the right way. Use this table only for what types and clippy
# (clippy.toml) cannot catch.
set -euo pipefail
cd "$(dirname "$0")/../.."

failures=0

# $1 = id, $2 = rg globs (space-separated), $3 = regex, $4 = a violating line, $5 = the right way
rule() {
  local id=$1 regex=$3 example=$4 reason=$5 globs=() glob list
  read -r -a list <<<"$2"
  for glob in "${list[@]}"; do globs+=(-g "$glob"); done
  if ! rg -q -e "$regex" <<<"$example"; then
    echo "patterns.sh: rule $id does not match its own example" >&2
    failures=$((failures + 1))
  fi
  if rg -n -e "$regex" "${globs[@]}" -g '!scripts/ci/patterns.sh' .; then
    echo "^ $id: $reason" >&2
    failures=$((failures + 1))
  fi
}

rule registry-ca-from 'deploy/** crates/** scripts/** docs/**' '\bcaFrom\b' \
  '  caFrom:' \
  'name the Secret key of the CA in registry.caSecret; the controller publishes it in registry-ca'
rule registry-ca-bundle 'deploy/** crates/** scripts/** docs/**' '\bcaBundle\b' \
  '  caBundle: LS0t' \
  'name the Secret key of the CA in registry.caSecret; the controller publishes it in registry-ca'
rule upload-secret-ca 'crates/**/*.rs !crates/zeabur-builder/src/registry_ca.rs' '"ca\.crt"' \
  '        data.insert("ca.crt".to_owned(), ByteString(ca.as_bytes().to_vec()));' \
  'an upload Secret holds the credentials only; clients read the CA from builder-system/registry-ca'
rule k3d-image-import 'scripts/** .github/**' '^[^#]*\bk3d image import\b' \
  'k3d image import ghcr.io/zeabur-oss/builder:dev -c zeabur-builder' \
  'k3d 5.9.0 can report success for an import that never ran; pipe docker image save to ctr image import on each node (scripts/deploy-example.sh)'
rule text-scraping 'scripts/**' '\b(grep|sed|awk|cut)\b' \
  "version=\$(helm show chart deploy/chart | sed -n 's/^version: //p')" \
  'read structured output with jq, yq or kubectl -o jsonpath, search text with rg, and move a long script with branches to a Rust test or Bun'

if ((failures > 0)); then
  exit 1
fi
