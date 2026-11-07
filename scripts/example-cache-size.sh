#!/usr/bin/env bash
# Changes the buildkitd cache size and builds again in a namespace whose daemon already exists:
#   scripts/deploy-example.sh <context>
#   scripts/example-cache-size.sh <context>
# Kubernetes refuses to change a StatefulSet's volume claim templates, so the controller recreates
# the daemon with an empty cache of the new size (zeabur-oss/builder#66).
# Environment: NAMESPACE (team-a), CACHE_SIZE (11Gi), TIMEOUT (600).
set -euo pipefail

context="${1:?usage: example-cache-size.sh <context>}"
ns="${NAMESPACE:-team-a}"
size="${CACHE_SIZE:-11Gi}"
timeout="${TIMEOUT:-600}"
root="$(cd "$(dirname "$0")/.." && pwd)"
pvc="cache-buildkitd-$ns-0"

k() { kubectl --context "$context" "$@"; }
cleanup() { k -n "$ns" delete build resized --ignore-not-found --wait=false >/dev/null 2>&1 || true; }
trap cleanup EXIT

before=$(k -n builder-system get pvc "$pvc" -o jsonpath='{.spec.resources.requests.storage}')
[[ -n $before && $before != "$size" ]] || {
  echo "$pvc is ${before:-missing}; run deploy-example.sh first" >&2
  exit 1
}

args=$(helm --kube-context "$context" -n builder-system get values builder -o json |
  jq -c --arg size "$size" '[.extraArgs[] | select(startswith("--buildkitd-cache-size=") | not)]
    + ["--buildkitd-cache-size=" + $size]')
helm --kube-context "$context" -n builder-system upgrade builder "$root/deploy/chart" \
  --reuse-values --set-json "extraArgs=$args" >/dev/null
k -n builder-system rollout status deployment/zeabur-builder --timeout 120s

k apply -f - >/dev/null <<EOF
apiVersion: builder.zeabur.com/v1alpha1
kind: Build
metadata: {name: resized, namespace: $ns}
spec:
  source: {git: {url: https://github.com/traefik/whoami}}
  builderRef: dockerfile
EOF
deadline=$(($(date +%s) + timeout))
until [[ -n $(k -n "$ns" get build resized -o jsonpath='{.status.latestImage}') ]]; do
  failed=$(k -n "$ns" get buildrun -l builder.zeabur.com/build=resized \
    -o jsonpath='{range .items[?(@.status.phase=="Failed")]}{.status.reason}{": "}{.status.message}{"\n"}{end}')
  if [[ -n $failed ]]; then
    echo "$ns/resized failed: $failed" >&2
    exit 1
  fi
  if (($(date +%s) > deadline)); then
    echo "$ns/resized: no image after ${timeout}s after the cache went from $before to $size" >&2
    k -n "$ns" get buildrun -l builder.zeabur.com/build=resized -o yaml >&2
    exit 1
  fi
  sleep 5
done

after=$(k -n builder-system get pvc "$pvc" -o jsonpath='{.spec.resources.requests.storage}')
if [[ $after != "$size" ]]; then
  echo "$pvc is $after after the build, not $size" >&2
  exit 1
fi
echo "ok    $ns built again after the cache went from $before to $size"
