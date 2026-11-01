#!/usr/bin/env bash
# Lints the chart and checks what an install needs: the ClusterBuilders, the buildkitd config and
# egress policy, the admission policy, images pinned by digest when a digest is set, and webhooks
# off unless enabled.
set -euo pipefail

chart="$(dirname "$0")/../deploy/chart"
version=$(helm show chart "$chart" | sed -n 's/^appVersion: "*\([^"]*\)"*$/\1/p')
version=${version//./\\.}
base=(--set registry.host=registry.example:5000 --set registry.tokenKey.namespace=zot
  --set registry.tokenKey.secret=zot-token-signer)

helm lint --strict "$chart" "${base[@]}"
helm lint --strict "$chart" "${base[@]}" --set webhooks.enabled=true \
  --set webhooks.ingress.enabled=true --set webhooks.ingress.host=hooks.example.com
helm lint --strict "$chart" "${base[@]}" --set webhooks.enabled=true \
  --set webhooks.httpRoute.enabled=true --set 'webhooks.httpRoute.parentRefs[0].name=gateway'

off=$(helm template builder "$chart" "${base[@]}" --set webhooks.ingress.enabled=true)
if grep -qE 'kind: (Service|Ingress|HTTPRoute)$|--webhook-addr' <<<"$off"; then
  echo "webhooks are exposed although webhooks.enabled is false" >&2
  exit 1
fi

on=$(helm template builder "$chart" "${base[@]}" --set webhooks.enabled=true \
  --set webhooks.path=/git/ --set webhooks.ingress.enabled=true \
  --set webhooks.ingress.host=hooks.example.com)
for want in 'kind: Ingress' '--webhook-path=/git$' 'path: /git/$' 'host: "hooks.example.com"'; do
  if ! grep -qE -- "$want" <<<"$on"; then
    echo "the enabled chart lacks $want" >&2
    exit 1
  fi
done
require() { # $1 = rendered chart, $2 = what is checked, then patterns it must hold
  local out=$1 what=$2
  shift 2
  for want in "$@"; do
    if ! grep -qE -- "$want" <<<"$out"; then
      echo "$what: the chart lacks $want" >&2
      exit 1
    fi
  done
}

plain=$(helm template builder "$chart" "${base[@]}")
require "$plain" "default install" 'name: dockerfile$' 'name: railpack$' 'name: buildkitd$' \
  'name: buildkitd-egress$' '- 169.254.0.0/16$' 'ghcr.io/railwayapp/railpack-frontend",$' \
  "image: \"ghcr.io/zeabur-oss/builder:$version\"\$" 'kind: ValidatingAdmissionPolicy$' \
  "request.userInfo.username == 'system:serviceaccount:default:zeabur-builder'" \
  "c.image in \\['ghcr.io/zeabur-oss/builder-job:$version', 'ghcr.io/zeabur-oss/builder-railpack:$version'\\]" \
  '--buildkitd-image=docker.io/moby/buildkit:' "c.image == 'docker.io/moby/buildkit:" \
  "request.namespace in \['default', 'zot'\]"
if grep -qE '^\s+\[registry\.|ZEABUR_BUILDER_REGISTRY_(CA|HTTP)' <<<"$plain"; then
  echo "default install: the chart sets registry http or a CA that nobody asked for" >&2
  exit 1
fi

digest=sha256:$(printf '0%.0s' {1..64})
pinned=$(helm template builder "$chart" "${base[@]}" --set image.digest="$digest" \
  --set jobImage.digest="$digest" --set railpackImage.digest="$digest" \
  --set registry.ca=CA --set 'admissionPolicy.extraImages[0]=example.com/planner:1' \
  --set registry.inCluster.namespace=zot --set registry.inCluster.podLabels.app=zot)
require "$pinned" "pinned install" "image: \"ghcr.io/zeabur-oss/builder@$digest\"" \
  "value: \"ghcr.io/zeabur-oss/builder-job@$digest\"" "image: \"ghcr.io/zeabur-oss/builder-railpack@$digest\"" \
  'ca = \["/etc/buildkit/registry-ca.crt"\]' 'registry-ca.crt: \|' 'kubernetes.io/metadata.name: zot' \
  'name: ZEABUR_BUILDER_REGISTRY_CA$' 'value: "CA"$' \
  "c.image in \['ghcr.io/zeabur-oss/builder-job@$digest', 'ghcr.io/zeabur-oss/builder-railpack@$digest', 'example.com/planner:1'\]" \
  "request.namespace in \['default', 'zot'\]"
http=$(helm template builder "$chart" "${base[@]}" --set registry.http=true)
require "$http" "plain HTTP registry" 'http = true$' 'name: ZEABUR_BUILDER_REGISTRY_HTTP, value: "true"'
tokens=$plain
require "$tokens" "registry tokens" 'value: "zot/zot-token-signer"$' 'namespace: zot$' \
  'resourceNames: \[zot-token-signer\]$' "request.namespace in \['default', 'zot'\]"
for missing in "registry.tokenKey.namespace=" "registry.tokenKey.secret="; do
  if err=$(helm template builder "$chart" --set registry.host=registry.example:5000 \
    --set registry.tokenKey.namespace=zot --set registry.tokenKey.secret=zot-token-signer \
    --set "$missing" 2>&1); then
    echo "missing token key: the chart renders without ${missing%=}" >&2
    exit 1
  fi
  if ! grep -qF "${missing%=} is required" <<<"$err"; then
    echo "missing token key: the error does not name ${missing%=}" >&2
    exit 1
  fi
done
echo "chart ok"
