#!/usr/bin/env bash
# Writes the test registry's Secret zot-tls in builder-registry, as the Zeabur Catalog's zot-tls:
# a new CA in ca.crt and a certificate it signed for the registry's Service in tls.crt and tls.key.
# A rerun rotates both.
#   scripts/registry-tls.sh <context>
set -euo pipefail

context="${1:?usage: registry-tls.sh <context>}"
host=registry.builder-registry.svc.cluster.local
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT

ec=(-newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes)
openssl req -x509 "${ec[@]}" -keyout "$dir/ca.key" -out "$dir/ca.crt" -days 30 \
  -subj "/CN=builder-registry CA $(date +%s%N)" \
  -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign,cRLSign 2>/dev/null
openssl req "${ec[@]}" -keyout "$dir/tls.key" -out "$dir/tls.csr" -subj "/CN=$host" 2>/dev/null
printf 'subjectAltName=DNS:%s\nextendedKeyUsage=serverAuth\nbasicConstraints=CA:FALSE\n' "$host" \
  >"$dir/ext"
openssl x509 -req -in "$dir/tls.csr" -CA "$dir/ca.crt" -CAkey "$dir/ca.key" -CAcreateserial \
  -days 30 -extfile "$dir/ext" -out "$dir/tls.crt" 2>/dev/null
kubectl --context "$context" -n builder-registry create secret generic zot-tls \
  --from-file="$dir/ca.crt" --from-file="$dir/tls.crt" --from-file="$dir/tls.key" \
  --dry-run=client -o yaml | kubectl --context "$context" apply -f - >/dev/null
