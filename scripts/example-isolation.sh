#!/usr/bin/env bash
# Checks the isolation between two build namespaces of the example:
#   NAMESPACES="team-a team-b" scripts/deploy-example.sh <context>
#   scripts/example-isolation.sh <context>
# Both namespaces must have built once, so their buildkitd and client certificate exist. Starts
# a fake metadata endpoint on the node (169.254.169.254:8169 on its loopback; k3d's Traefik holds
# port 80) and an HTTP Service in isolation-target, and removes both at the end. A target counts as
# reached when it answers HTTP at all.
# Environment: IMAGE_PREFIX, TAG (as for deploy-example.sh), A and B (team-a, team-b).
set -euo pipefail

context="${1:?usage: example-isolation.sh <context>}"
prefix="${IMAGE_PREFIX:-zeabur-builder}"
tag="${TAG:-dev}"
a="${A:-team-a}"
b="${B:-team-b}"
registry=registry.builder-registry.svc.cluster.local:5000
addr() { echo "tcp://buildkitd-$1-0.buildkitd-$1.builder-system.svc:1234"; }

k() { kubectl --context "$context" "$@"; }
out=$(mktemp)
failures=0
check() {
  local want=$1 what=$2
  shift 2
  if "$@" >"$out" 2>&1; then got=pass; else got=fail; fi
  if [[ $got == "$want" ]]; then
    echo "ok    $what"
  else
    echo "WRONG $what (expected $want, got $got)"
    failures=$((failures + 1))
  fi
  sed -e 's/^/      /' "$out" | tail -n "${LINES_SHOWN:-4}"
}

cleanup() {
  rm -f "$out"
  k -n isolation-target exec metadata -- ip addr del 169.254.169.254/32 dev lo >/dev/null 2>&1 || true
  k delete namespace isolation-target --wait=false >/dev/null 2>&1 || true
  k -n "$a" delete pod probe --wait=false >/dev/null 2>&1 || true
  k -n "$b" delete pod probe --wait=false >/dev/null 2>&1 || true
  k -n "$b" delete secret "stolen-from-$a" >/dev/null 2>&1 || true
  k -n "$a" delete secret probe-push >/dev/null 2>&1 || true
}
trap cleanup EXIT

k create namespace isolation-target --dry-run=client -o yaml | k apply -f - >/dev/null
k apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: {name: metadata, namespace: isolation-target}
spec:
  hostNetwork: true
  containers:
    - name: metadata
      image: docker.io/library/busybox:1.38.0
      command: [sh, -c, "ip addr add 169.254.169.254/32 dev lo; mkdir -p /www; echo instance-metadata > /www/index.html; exec httpd -f -p 169.254.169.254:8169 -h /www"]
      securityContext: {privileged: true}
---
apiVersion: v1
kind: Pod
metadata: {name: target, namespace: isolation-target, labels: {app: target}}
spec:
  containers:
    - name: target
      image: docker.io/library/busybox:1.38.0
      command: [sh, -c, "mkdir -p /www; echo cluster-service > /www/index.html; exec httpd -f -p 8080 -h /www"]
---
apiVersion: v1
kind: Service
metadata: {name: target, namespace: isolation-target}
spec:
  selector: {app: target}
  ports: [{port: 8080}]
EOF

# A's client certificate, as any member of A can read it, and B's own, in a probe Pod in B.
k -n "$a" get secret zeabur-builder-buildkit-client -o json |
  jq --arg n "stolen-from-$a" '{apiVersion, kind, type, data, metadata: {name: $n}}' |
  k -n "$b" apply -f - >/dev/null
# A token for $a/probe only, as an admin gets one from the controller (docs/operations/registry-tokens.mdx).
token=$(k -n builder-system exec deploy/zeabur-builder -- \
  /zeabur-builder registry-token --repository "$a/probe" --push)
k -n "$a" create secret generic probe-push --type kubernetes.io/dockerconfigjson \
  --from-literal=.dockerconfigjson="{\"auths\":{\"$registry\":{\"registrytoken\":\"$token\"}}}" \
  --dry-run=client -o yaml | k apply -f - >/dev/null

k apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: {name: probe, namespace: $b}
spec:
  automountServiceAccountToken: false
  securityContext: {runAsNonRoot: true, runAsUser: 1000, seccompProfile: {type: RuntimeDefault}}
  containers:
    - name: probe
      image: $prefix/builder-job:$tag
      imagePullPolicy: IfNotPresent
      command: [sleep, "3600"]
      env: [{name: HOME, value: /tmp}]
      securityContext: {allowPrivilegeEscalation: false, capabilities: {drop: [ALL]}}
      volumeMounts:
        - {name: own, mountPath: /tls/own, readOnly: true}
        - {name: stolen, mountPath: /tls/stolen, readOnly: true}
  volumes:
    - name: own
      secret: {secretName: zeabur-builder-buildkit-client}
    - name: stolen
      secret: {secretName: stolen-from-$a}
EOF
k apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: {name: probe, namespace: $a}
spec:
  automountServiceAccountToken: false
  securityContext: {runAsNonRoot: true, runAsUser: 1000, seccompProfile: {type: RuntimeDefault}}
  containers:
    - name: probe
      image: $prefix/builder-job:$tag
      imagePullPolicy: IfNotPresent
      command: [sleep, "3600"]
      env: [{name: HOME, value: /tmp}, {name: DOCKER_CONFIG, value: /tmp/docker}]
      securityContext: {allowPrivilegeEscalation: false, capabilities: {drop: [ALL]}}
      volumeMounts:
        - {name: own, mountPath: /tls/own, readOnly: true}
        - {name: push, mountPath: /tmp/docker, readOnly: true}
  volumes:
    - name: own
      secret: {secretName: zeabur-builder-buildkit-client}
    - name: push
      secret: {secretName: probe-push, items: [{key: .dockerconfigjson, path: config.json}]}
EOF
k -n isolation-target wait --for condition=Ready pod/metadata pod/target --timeout 120s >/dev/null
k -n "$a" wait --for condition=Ready pod/probe --timeout 120s >/dev/null
k -n "$b" wait --for condition=Ready pod/probe --timeout 120s >/dev/null

echo "== client certificates"
check pass "$b's certificate opens $b's buildkitd" \
  k -n "$b" exec probe -- timeout 20 buildctl --addr "$(addr "$b")" --tlsdir /tls/own debug workers
check fail "$a's certificate is refused by $b's buildkitd" \
  k -n "$b" exec probe -- timeout 20 buildctl --addr "$(addr "$b")" --tlsdir /tls/stolen debug workers
check fail "a Pod in $a cannot reach $b's buildkitd at all" \
  k -n "$a" exec probe -- timeout 10 buildctl --addr "$(addr "$b")" --tlsdir /tls/own debug workers

# Builds in a namespace's buildkitd with its certificate, as a member could: in A with the token
# for $a/probe, in B with no registry credentials.
build_in() {
  local ns=$1 name=$2 dockerfile=$3
  k -n "$ns" exec probe -- sh -c "mkdir -p /tmp/ctx && printf '%s\n' \"\$1\" > /tmp/ctx/Dockerfile &&
    buildctl --addr $(addr "$ns") --tlsdir /tls/own build --progress plain --no-cache \
      --frontend dockerfile.v0 --local context=/tmp/ctx --local dockerfile=/tmp/ctx $name" build "$dockerfile"
}
build() { build_in "$a" "$@"; }
echo "== registry"
check pass "a token for $a/probe pushes to $registry/$a/probe" \
  build "--output type=image,name=$registry/$a/probe:isolation,push=true" "FROM busybox:1.38.0"
check fail "a token for $a/probe cannot push to $registry/$b/probe" \
  build "--output type=image,name=$registry/$b/probe:isolation,push=true" "FROM busybox:1.38.0"
check fail "a push without a token is refused" \
  build_in "$b" "--output type=image,name=$registry/$b/probe:isolation,push=true" "FROM busybox:1.38.0"

echo "== RUN network"
reach() { echo "wget -S -T 10 -O /dev/null $1 2>&1 | grep -q HTTP/"; }
run_step() { build "" "FROM busybox:1.38.0
RUN ip -4 addr show eth0 | grep inet; $1"; }
LINES_SHOWN=2 check pass "RUN has its own network namespace and reaches the internet" \
  run_step "$(reach https://example.com)"
check pass "RUN reaches the registry" \
  run_step "$(reach "http://$registry/v2/")"
check fail "RUN cannot reach a Service in another namespace" \
  run_step "$(reach http://target.isolation-target.svc:8080/)"
check fail "RUN cannot reach the Kubernetes API" \
  run_step "$(reach https://kubernetes.default.svc/version)"
check fail "RUN cannot reach 169.254.169.254" \
  run_step "$(reach http://169.254.169.254:8169/)"
check fail "RUN --network=host is refused" \
  build "" "# syntax=docker/dockerfile:1
FROM busybox:1.38.0
RUN --network=host true"
check fail "a gateway frontend outside allowedRepositories is refused" \
  build "" "# syntax=docker.io/tonistiigi/dockerfile:latest
FROM busybox:1.38.0"

echo "== the same targets from an ordinary Pod in $a, for comparison"
check pass "a Pod reaches a Service in another namespace" \
  k -n "$a" exec probe -- sh -c "$(reach http://target.isolation-target.svc:8080/)"
check pass "a Pod reaches 169.254.169.254 (the fake endpoint on the node)" \
  k -n "$a" exec probe -- sh -c "$(reach http://169.254.169.254:8169/)"

if ((failures)); then
  echo "$failures checks failed" >&2
  exit 1
fi
echo "all checks passed"
