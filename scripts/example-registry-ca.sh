#!/usr/bin/env bash
# Rotates the registry's CA and checks that Zeabur Builder follows it without a reinstall:
#   scripts/deploy-example.sh <context>
#   scripts/example-registry-ca.sh <context>
# The controller publishes the CA alone in builder-system/registry-ca, which any signed-in user may
# read; the registry's own Secret, with its key, stays closed. A/hello-python (Railpack) rebuilds
# with the first CA: it pushes to the registry and pulls its first image back from it, so A's
# buildkitd keeps a pull client of the registry. Then zot-tls gets a new CA and certificate and the
# registry restarts: registry-ca follows, the next rebuild restarts A's buildkitd with the new CA,
# keeping its cache, and pushes and pulls with it. The chart keeps its revision and the controller
# its Pods.
# Environment: A (team-a), TIMEOUT (600).
set -euo pipefail

context="${1:?usage: example-registry-ca.sh <context>}"
a="${A:-team-a}"
timeout="${TIMEOUT:-600}"
root="$(cd "$(dirname "$0")/.." && pwd)"

k() { kubectl --context "$context" "$@"; }
failures=0
ok() { echo "ok    $1"; }
wrong() {
  echo "WRONG $1"
  failures=$((failures + 1))
}

published() { k -n builder-system get secret registry-ca -o jsonpath='{.data.ca\.crt}' | base64 -d; }
source_ca() { k -n builder-registry get secret zot-tls -o jsonpath='{.data.ca\.crt}' | base64 -d; }
daemon_pod() {
  k -n builder-system get pod -l "app.kubernetes.io/name=buildkitd,builder.zeabur.com/namespace=$a" \
    -o jsonpath='{.items[0].metadata.name} {.items[0].metadata.uid}'
}
cache_claim() { k -n builder-system get pvc "cache-$1" -o jsonpath='{.metadata.uid}'; }
# $1 = seconds, then a command that succeeds once the wait is over.
wait_until() {
  local deadline=$(($(date +%s) + $1))
  shift
  until "$@"; do
    (($(date +%s) < deadline)) || return 1
    sleep 2
  done
}
# Asks for a rebuild of A/hello-python and prints the phase its run ends in.
rebuild() { # $1 = the annotation value
  k -n "$a" annotate build/hello-python --overwrite "builder.zeabur.com/rebuild=$1" >/dev/null
  local deadline=$(($(date +%s) + timeout)) phase
  while (($(date +%s) < deadline)); do
    phase=$(k -n "$a" get buildrun -l builder.zeabur.com/build=hello-python -o json |
      jq -r --arg v "$1" '.items[]
        | select(.metadata.annotations["builder.zeabur.com/rebuild"] == $v) | .status.phase // empty')
    case $phase in Succeeded | Failed)
      echo "$phase"
      return
      ;;
    esac
    sleep 5
  done
  echo "no end after ${timeout}s"
}

echo "== who may read the CA"
user=(--as someone@example.com --as-group system:authenticated)
if [[ $(k -n builder-system auth can-i get secret/registry-ca "${user[@]}" || true) == yes ]]; then
  ok "a signed-in user may read registry-ca"
else
  wrong "a signed-in user may not read registry-ca"
fi
if [[ $(k -n builder-registry auth can-i get secret/zot-tls "${user[@]}" || true) == no ]]; then
  ok "a signed-in user may not read zot-tls"
else
  wrong "a signed-in user may read zot-tls, which holds the registry's key"
fi
if [[ $(published) == "$(source_ca)" && -n $(published) ]]; then
  ok "registry-ca holds the CA of zot-tls"
else
  wrong "registry-ca does not hold the CA of zot-tls"
fi

echo "== rotation"
result=$(rebuild "ca-$(date +%s)-before")
if [[ $result == Succeeded ]]; then
  ok "A/hello-python builds with the first CA"
else
  wrong "A/hello-python with the first CA: $result"
fi
revision=$(helm --kube-context "$context" -n builder-system status builder -o json | jq .version)
controller=$(k -n builder-system get pod -l app.kubernetes.io/name=zeabur-builder \
  -o jsonpath='{.items[*].metadata.uid}')
read -r pod daemon <<<"$(daemon_pod)"
claim=$(cache_claim "$pod")
old=$(published)

"$root/scripts/registry-tls.sh" "$context"
k -n builder-registry rollout restart deployment/registry >/dev/null
k -n builder-registry rollout status deployment/registry --timeout 120s >/dev/null
new=$(source_ca)
[[ $new != "$old" ]] || wrong "registry-tls.sh wrote the same CA"

follows() { [[ $(published) == "$new" ]]; }
if wait_until 60 follows; then
  ok "registry-ca follows the new CA"
else
  wrong "registry-ca still holds the first CA after 60s"
fi
result=$(rebuild "ca-$(date +%s)-after")
if [[ $result == Succeeded ]]; then
  ok "A/hello-python builds with the new CA"
else
  wrong "A/hello-python with the new CA: $result"
fi
read -r _ restarted <<<"$(daemon_pod)"
if [[ $restarted != "$daemon" ]]; then
  ok "A's buildkitd restarted for the new CA"
else
  wrong "A's buildkitd kept its Pod, and its pull clients with the first CA"
fi
if [[ $(k -n builder-system exec "$pod" -- cat /etc/buildkit-registry-ca/ca.crt) == "$new" ]]; then
  ok "A's buildkitd mounts the new CA"
else
  wrong "A's buildkitd mounts another CA"
fi
if [[ $(cache_claim "$pod") == "$claim" ]]; then
  ok "A's buildkitd keeps its cache"
else
  wrong "A's buildkitd lost its cache"
fi

if [[ $(helm --kube-context "$context" -n builder-system status builder -o json | jq .version) == "$revision" ]]; then
  ok "the chart keeps revision $revision"
else
  wrong "the chart was upgraded"
fi
if [[ $(k -n builder-system get pod -l app.kubernetes.io/name=zeabur-builder \
  -o jsonpath='{.items[*].metadata.uid}') == "$controller" ]]; then
  ok "the controller keeps its Pods"
else
  wrong "the controller restarted"
fi

if ((failures > 0)); then
  echo "$failures check(s) failed" >&2
  exit 1
fi
