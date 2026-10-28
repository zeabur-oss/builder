#!/usr/bin/env bash
# One CI job, as the shared CI workflow of zeabur-oss/.github runs it (from .github/workflows/ci.yml)
# and as anyone runs it locally: the workflow only checks out, installs Nix and restores caches,
# then calls this script once per job of --list. A job behaves the same on GitHub and on any Linux
# machine with Nix (and Docker for the cluster job).
#
#   scripts/ci.sh <job>      # run one job; it enters the Nix dev shell itself
#   scripts/ci.sh --list     # the job names
#
# A job leaves what is worth keeping in CI_ARTIFACTS (default target/ci/artifacts), which the
# workflow uploads as the artifact ci-<job>: the kubernetes job keeps there the log of every Pod.
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

artifacts="${CI_ARTIFACTS:-$root/target/ci/artifacts}"
mkdir -p "$artifacts"
artifacts="$(cd "$artifacts" && pwd)"

# The disposable cluster, in a kubeconfig of its own so that the machine's context never changes.
start_cluster() {
  export KUBECONFIG="$root/target/ci/kubeconfig"
  mkdir -p "$(dirname "$KUBECONFIG")"
  k3d cluster delete zeabur-builder >/dev/null 2>&1 || true
  if [[ -z "${ZEABUR_CI_KEEP:-}" ]]; then
    trap 'stop_collecting; k3d cluster delete zeabur-builder >/dev/null 2>&1 || true' EXIT
  else
    trap stop_collecting EXIT
  fi
  local registry=()
  [[ -z "${K3D_REGISTRY_CONFIG:-}" ]] || registry=(--registry-config "$K3D_REGISTRY_CONFIG")
  k3d cluster create zeabur-builder --image "$k3s_image" --no-lb --wait --timeout 300s "${registry[@]}"
}

# Keeps the log of every Pod outside kube-system that ends, in $artifacts/pods/, while the job
# runs. A build Job is deleted ten minutes after it ends (the controller's --job-ttl), often before
# the job fails and cluster_failure_context looks.
collector=""
collect_pod_logs() {
  local dir="$artifacts/pods" ns pod phase
  mkdir -p "$dir"
  while :; do
    kubectl get pod -A --no-headers \
      -o custom-columns=NS:.metadata.namespace,NAME:.metadata.name,PHASE:.status.phase 2>/dev/null |
      while read -r ns pod phase; do
        [[ $ns != kube-system && ($phase == Succeeded || $phase == Failed) ]] || continue
        [[ ! -e "$dir/$ns.$pod.log" ]] || continue
        kubectl -n "$ns" logs "$pod" --all-containers --prefix --timestamps >"$dir/$ns.$pod.log" 2>&1 || true
        [[ $phase != Failed ]] || echo "pod $ns/$pod failed; its log is in $dir/$ns.$pod.log"
      done
    sleep 3
  done
}
start_collecting() {
  collect_pod_logs &
  collector=$!
}
stop_collecting() {
  [[ -z "$collector" ]] || kill "$collector" 2>/dev/null || true
  collector=""
}

# Whether a RUN step reaches the network, as builds see it: each buildkitd's own view, then a
# one-step build on it.
network_context() {
  # Interfaces and their MTUs, with what BusyBox has too. Expanded by the shells that run it.
  # shellcheck disable=SC2016
  local links='for i in /sys/class/net/*; do echo "${i##*/} mtu $(cat "$i/mtu")"; done'
  echo "--- runner"
  uname -a
  sh -c "$links"
  echo "--- k3s node"
  docker exec k3d-zeabur-builder-server-0 sh -c "$links; ip route; iptables -V" || true
  local pod
  for pod in $(kubectl -n builder-system get pod -l app.kubernetes.io/name=buildkitd -o name); do
    echo "--- $pod: network"
    timeout 300 kubectl -n builder-system exec -i "$pod" -- sh -s <<'EOF' || true
for i in /sys/class/net/*; do echo "${i##*/} mtu $(cat "$i/mtu")"; done
cat /etc/resolv.conf; ip addr; ip route; iptables -V; iptables-save 2>&1 | head -n 80
wget -T 10 -q -O /dev/null https://dl-cdn.alpinelinux.org/alpine/ && echo "pod: https ok" || echo "pod: https failed"
probe=$(mktemp -d)
cat >"$probe/Dockerfile" <<'DOCKERFILE'
FROM docker.io/library/alpine:3
RUN cat /etc/resolv.conf; ip addr; ip route; \
  nslookup dl-cdn.alpinelinux.org || echo "RUN: dns failed"; \
  wget -T 10 -q -O /dev/null http://1.1.1.1/ && echo "RUN: ip ok" || echo "RUN: ip failed"; \
  wget -T 10 -q -O /dev/null https://dl-cdn.alpinelinux.org/alpine/ && echo "RUN: https ok" || echo "RUN: https failed"
DOCKERFILE
buildctl --addr unix:///run/buildkit/buildkitd.sock build --progress plain --no-cache \
  --frontend dockerfile.v0 --local context="$probe" --local dockerfile="$probe"
EOF
  done
}

# The logs of the Pods outside kube-system, in $artifacts/pods; with --print, their ends too.
save_pod_logs() {
  local ns pod
  mkdir -p "$artifacts/pods"
  kubectl get pod -A --no-headers 2>/dev/null | awk '$1 != "kube-system" { print $1, $2 }' |
    while read -r ns pod; do
      kubectl -n "$ns" logs "$pod" --all-containers --prefix --timestamps >"$artifacts/pods/$ns.$pod.log" 2>&1 || true
      if [[ "${1:-}" == --print ]]; then
        echo "--- $ns/$pod"
        tail -n 80 "$artifacts/pods/$ns.$pod.log"
      fi
    done
  kubectl get build,buildrun -A -o yaml >"$artifacts/builds.yaml" 2>&1 || true
}

# What the cluster holds when the job fails: on the console, and in full in $artifacts.
cluster_failure_context() {
  stop_collecting
  kubectl get build,buildrun,job,pod -A -o wide || true
  kubectl -n builder-system logs deployment/zeabur-builder --tail=100 || true
  save_pod_logs --print
  local log
  # The build Pods that failed earlier and are gone by now.
  for log in "$artifacts"/pods/*.log; do
    [[ -e "$log" ]] || continue
    if ! kubectl -n "$(basename "$log" | cut -d . -f 1)" get pod "$(basename "$log" .log | cut -d . -f 2-)" >/dev/null 2>&1; then
      echo "--- $(basename "$log" .log) (gone)"
      tail -n 150 "$log"
    fi
  done
  kubectl get events -A --sort-by=.lastTimestamp >"$artifacts/events.txt" 2>&1 || true
  tail -n 50 "$artifacts/events.txt"
  network_context 2>&1 | tee "$artifacts/network.txt"
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
    start_collecting
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
    stop_collecting
    save_pod_logs
    ;;
esac
