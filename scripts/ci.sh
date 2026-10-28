#!/usr/bin/env bash
# One CI job, as the shared CI workflow of zeabur-oss/.github runs it (from .github/workflows/ci.yml)
# and as anyone runs it locally: the workflow only checks out, installs Nix and restores caches,
# then calls this script once per job of --list. A job behaves the same on GitHub and on any Linux
# machine with Nix (and Docker for the cluster job).
#
#   scripts/ci.sh <job>      # run one job; it enters the Nix dev shell itself
#   scripts/ci.sh --list     # the job names
#
# The kubernetes job creates the k3d cluster `zeabur-builder` and deletes it when it ends; set
# ZEABUR_CI_KEEP=1 to keep it. K3S_IMAGE picks the k3s release. K3D_REGISTRY_CONFIG passes a
# registries.yaml to k3d, such as one with pull-through mirrors. ZEABUR_CI_IMAGE_TAG uses the
# images zeabur-builder/<name>:<tag> already in Docker instead of building them (the release smoke).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

check_jobs=(format clippy docs test cargo-deny levels chart)
cluster_jobs=(kubernetes)

if [[ "${1:-}" == --list ]]; then
  printf '%s\n' "${check_jobs[@]}" "${cluster_jobs[@]}"
  exit 0
fi
job="${1:?usage: scripts/ci.sh <job> | --list}"
printf '%s\n' "${check_jobs[@]}" "${cluster_jobs[@]}" | grep -qx -- "$job" ||
  { echo "error: no job named $job (scripts/ci.sh --list)" >&2; exit 1; }

# The marker tells a re-run inside the dev shell apart from a call outside it.
if [[ -z "${ZEABUR_CI_SHELL:-}" ]]; then
  export ZEABUR_CI_SHELL=1
  exec nix develop "$root" --command "$0" "$@"
fi

export RUSTDOCFLAGS="-D warnings"
k3s_image="${K3S_IMAGE:-rancher/k3s:v1.36.5-k3s1}"

# The disposable cluster, in a kubeconfig of its own so that the machine's context never changes.
start_cluster() {
  export KUBECONFIG="$root/target/ci/kubeconfig"
  mkdir -p "$(dirname "$KUBECONFIG")"
  k3d cluster delete zeabur-builder >/dev/null 2>&1 || true
  if [[ -z "${ZEABUR_CI_KEEP:-}" ]]; then
    trap 'k3d cluster delete zeabur-builder >/dev/null 2>&1 || true' EXIT
  fi
  local registry=()
  [[ -z "${K3D_REGISTRY_CONFIG:-}" ]] || registry=(--registry-config "$K3D_REGISTRY_CONFIG")
  k3d cluster create zeabur-builder --image "$k3s_image" --no-lb --wait --timeout 300s "${registry[@]}"
}

# What the cluster holds when the job fails.
cluster_failure_context() {
  kubectl get build,buildrun,job,pod -A -o wide || true
  kubectl -n builder-system logs deployment/zeabur-builder --tail=100 || true
  local ns pod
  kubectl get pod -A --no-headers 2>/dev/null | awk '$1 != "kube-system" { print $1, $2 }' |
    while read -r ns pod; do
      echo "--- $ns/$pod"
      kubectl -n "$ns" logs "$pod" --all-containers --tail=80 || true
    done
  kubectl get events -A --sort-by=.lastTimestamp | tail -n 50 || true
}

case "$job" in
  format)
    cargo fmt --all --check
    nixfmt --check flake.nix
    shellcheck scripts/*.sh
    actionlint
    ;;
  clippy) cargo lint ;;
  docs) cargo doc --workspace --no-deps --document-private-items --locked ;;
  test) cargo nextest run --workspace --all-features --locked --no-tests=warn ;;
  cargo-deny) cargo deny --locked check ;;
  levels) ./scripts/check-levels.sh ;;
  chart) ./scripts/check-chart.sh ;;
  # The chart from this checkout on k3s, with the images built here: a Dockerfile build in two
  # namespaces and a Railpack build, the two failures the samples make on purpose, the isolation
  # between the namespaces and the admission policy.
  kubernetes)
    tag="${ZEABUR_CI_IMAGE_TAG:-ci}"
    start_cluster
    [[ -n "${ZEABUR_CI_IMAGE_TAG:-}" ]] || ./scripts/build-images.sh zeabur-builder ci
    if ! {
      TAG="$tag" K3D_CLUSTER=zeabur-builder ./scripts/deploy-example.sh k3d-zeabur-builder \
        team-a/whoami team-a/hello-python team-b/whoami &&
        kubectl -n team-a wait build/no-such-ref --timeout 120s \
          --for 'jsonpath={.status.conditions[?(@.type=="SourceReady")].reason}=RefNotFound' &&
        kubectl -n team-a wait buildrun -l builder.zeabur.com/build=no-such-dockerfile --timeout 600s \
          --for 'jsonpath={.status.phase}=Failed' &&
        TAG="$tag" ./scripts/example-isolation.sh k3d-zeabur-builder &&
        TAG="$tag" ./scripts/example-admission.sh k3d-zeabur-builder &&
        ./scripts/example-archive.sh k3d-zeabur-builder
    }; then
      cluster_failure_context
      exit 1
    fi
    ;;
esac
