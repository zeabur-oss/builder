#!/usr/bin/env bash
# Installs the chart on a test cluster with a test registry, applies the sample Builds and waits
# for a Build's image:
#   scripts/deploy-example.sh <context> [[namespace/]build...]   # default: team-a/whoami
# Environment:
#   IMAGE_PREFIX, TAG     the images from scripts/build-images.sh (zeabur-builder, dev)
#   K3D_CLUSTER           when set, imports the images into this k3d cluster first
#   HOST_USERS            buildkitd's hostUsers (false: a user namespace per buildkitd)
#   IDLE                  seconds before an idle buildkitd scales to zero (600)
#   NAMESPACES            build namespaces to create (team-a team-b)
#   TIMEOUT               seconds to wait for the Builds (900); the first failed BuildRun stops
#                         the wait
set -euo pipefail

context="${1:?usage: deploy-example.sh <context> [[namespace/]build...]}"
shift
builds=("$@")
[[ ${#builds[@]} -gt 0 ]] || builds=(whoami)
prefix="${IMAGE_PREFIX:-zeabur-builder}"
tag="${TAG:-dev}"
host_users="${HOST_USERS:-false}"
idle="${IDLE:-600}"
read -r -a namespaces <<<"${NAMESPACES:-team-a team-b}"
timeout="${TIMEOUT:-900}"
root="$(cd "$(dirname "$0")/.." && pwd)"
manifests="$root/deploy/examples"
registry=registry.builder-registry.svc.cluster.local:5000

k() { kubectl --context "$context" "$@"; }
ensure_namespace() { k create namespace "$1" --dry-run=client -o yaml | k apply -f - >/dev/null; }

if [[ -n ${K3D_CLUSTER:-} ]]; then
  k3d image import --cluster "$K3D_CLUSTER" \
    "$prefix/builder:$tag" "$prefix/builder-job:$tag" "$prefix/builder-railpack:$tag"
fi

for ns in builder-system builder-registry "${namespaces[@]}"; do ensure_namespace "$ns"; done

# The key that signs registry tokens, as the Zeabur Catalog's zot-token-signer: the controller
# reads tls.key, the registry checks tokens with tls.crt. A rerun keeps it.
if ! k -n builder-registry get secret zot-token-signer >/dev/null 2>&1; then
  keys=$(mktemp -d)
  openssl genpkey -algorithm ed25519 -out "$keys/tls.key"
  openssl req -x509 -key "$keys/tls.key" -subj /CN=zot-token-signer -days 3650 -out "$keys/tls.crt"
  k -n builder-registry create secret tls zot-token-signer --cert "$keys/tls.crt" \
    --key "$keys/tls.key" >/dev/null
  rm -rf "$keys"
fi
k apply -f "$manifests/registry.yaml"
# A changed config needs a restart.
k -n builder-registry patch deployment registry --type merge -p \
  "{\"spec\":{\"template\":{\"metadata\":{\"annotations\":{\"zot-config\":\"$(sha256sum "$manifests/registry.yaml" | cut -c1-16)\"}}}}}"

# Helm installs the CRDs only on the first install; a rerun applies them itself.
if helm --kube-context "$context" -n builder-system status builder >/dev/null 2>&1; then
  k apply --server-side -f "$root/deploy/chart/crds/crds.yaml"
fi
helm --kube-context "$context" upgrade --install builder "$root/deploy/chart" -n builder-system \
  --set image.repository="$prefix/builder" --set image.tag="$tag" \
  --set jobImage.repository="$prefix/builder-job" --set jobImage.tag="$tag" \
  --set railpackImage.repository="$prefix/builder-railpack" --set railpackImage.tag="$tag" \
  --set registry.host="$registry" --set registry.http=true \
  --set registry.inCluster.namespace=builder-registry \
  --set registry.inCluster.podLabels.app\\.kubernetes\\.io/name=registry \
  --set registry.tokenKey.namespace=builder-registry --set registry.tokenKey.secret=zot-token-signer \
  --set-json "extraArgs=[\"--buildkitd-cache-size=10Gi\",\"--buildkitd-host-users=$host_users\",\"--buildkitd-idle=$idle\"]"
# The controller restarts so that a new image with the same tag is picked up.
k -n builder-system rollout restart deployment/zeabur-builder
k -n builder-system rollout status deployment/zeabur-builder --timeout 120s
k -n builder-registry rollout status deployment/registry --timeout 120s

start=$(date +%s)
deadline=$((start + timeout))
k apply -f "$manifests/builds.yaml" >/dev/null
pending=()
for build in "${builds[@]}"; do
  [[ $build == */* ]] || build="team-a/$build"
  pending+=("$build")
done
echo "waiting for ${pending[*]}"
# All at once, so that the first failure stops the wait, whichever Build it is.
while :; do
  waiting=()
  for build in "${pending[@]}"; do
    ns="${build%%/*}"
    name="${build#*/}"
    image=$(k -n "$ns" get build "$name" -o jsonpath='{.status.latestImage}')
    if [[ -n $image ]]; then
      echo "$build: $image after $(($(date +%s) - start))s"
      continue
    fi
    failed=$(k -n "$ns" get buildrun -l "builder.zeabur.com/build=$name" \
      -o jsonpath='{range .items[?(@.status.phase=="Failed")]}{.metadata.name}{" "}{.status.reason}{": "}{.status.message}{"\n"}{end}')
    if [[ -n $failed ]]; then
      echo "$build failed: $failed" >&2
      exit 1
    fi
    waiting+=("$build")
  done
  [[ ${#waiting[@]} -gt 0 ]] || break
  pending=("${waiting[@]}")
  if (($(date +%s) > deadline)); then
    echo "${pending[*]}: no image after ${timeout}s" >&2
    k get build,buildrun,job,pod -A >&2
    exit 1
  fi
  sleep 5
done
k get build,buildrun -A
