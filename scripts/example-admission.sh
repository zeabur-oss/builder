#!/usr/bin/env bash
# Checks the chart's admission policy: the controller's ServiceAccount may create a build Job and
# a buildkitd StatefulSet, and no other Job or StatefulSet:
#   scripts/deploy-example.sh <context>
#   scripts/example-admission.sh <context>
# A must have built once, so its buildkitd StatefulSet exists. Every request is a server-side dry
# run as the ServiceAccount, so nothing is created.
# Environment: IMAGE_PREFIX, TAG (as for deploy-example.sh), A (team-a).
set -euo pipefail
# check runs at the end of a pipe and counts failures in this shell.
shopt -s lastpipe

context="${1:?usage: example-admission.sh <context>}"
prefix="${IMAGE_PREFIX:-zeabur-builder}"
tag="${TAG:-dev}"
a="${A:-team-a}"
controller=system:serviceaccount:builder-system:zeabur-builder

k() { kubectl --context "$context" "$@"; }
failures=0
check() { # $1 = pass or fail, $2 = what, $3 = "admin" for the context's own user; the object on stdin
  local want=$1 what=$2 as=(--as "$controller") out got
  [[ ${3:-} == admin ]] && as=()
  if out=$(k "${as[@]}" create --dry-run=server -f - 2>&1); then got=pass; else got=fail; fi
  # A refusal counts only when the policy made it, not schema validation or RBAC.
  if [[ $got == fail ]] && ! grep -q 'denied request' <<<"$out"; then got="fail outside the policy"; fi
  if [[ $got == "$want" ]]; then
    echo "ok    $what"
  else
    echo "WRONG $what (expected $want, got $got)"
    failures=$((failures + 1))
  fi
  tail -n 2 <<<"$out" | sed -e 's/^/      /'
}

job() { # $1 = namespace: a Job in the shape of a build Job
  jq -n --arg ns "$1" --arg image "$prefix/builder-job:$tag" '{
    apiVersion: "batch/v1", kind: "Job",
    metadata: {name: "admission-probe", namespace: $ns},
    spec: {backoffLimit: 0, template: {spec: {
      restartPolicy: "Never", automountServiceAccountToken: false,
      securityContext: {runAsNonRoot: true, runAsUser: 1000, seccompProfile: {type: "RuntimeDefault"}},
      containers: [{name: "build", image: $image, command: ["true"],
        securityContext: {allowPrivilegeEscalation: false, readOnlyRootFilesystem: true,
          capabilities: {drop: ["ALL"]}}}],
      volumes: [{name: "tmp", emptyDir: {}}]}}}}'
}
privileged='.spec.template.spec.containers[0].securityContext |=
  (del(.allowPrivilegeEscalation) | .privileged = true)'

echo "== Jobs"
job "$a" | check pass "a build Job in $a"
job kube-system | check fail "a build Job in kube-system"
job builder-system | check fail "a build Job in the controller's namespace"
job builder-registry | check fail "a build Job in the registry's namespace"
job "$a" | jq "$privileged" | check fail "a privileged Job in $a"
job kube-system | jq "$privileged" | check fail "a privileged Job in kube-system"
job "$a" | jq '.spec.template.spec.containers[0].image = "docker.io/library/busybox:1.38.0"' |
  check fail "a Job with another image"
job "$a" | jq '.spec.template.spec.volumes += [{name: "host", hostPath: {path: "/"}}]' |
  check fail "a Job with a hostPath volume"
job "$a" | jq '.spec.template.spec.hostNetwork = true' | check fail "a Job on the host network"
job "$a" | jq '.spec.template.spec.hostPID = true' | check fail "a Job in the host PID namespace"
job "$a" | jq '.spec.template.spec.serviceAccountName = "admin"' |
  check fail "a Job with another ServiceAccount"
job "$a" | jq '.spec.template.spec.automountServiceAccountToken = true' |
  check fail "a Job with a ServiceAccount token"
job "$a" | jq "$privileged" | check pass "an admin's privileged Job is not the policy's concern" admin

echo "== StatefulSets"
buildkitd() { # $A's buildkitd StatefulSet under another name
  k -n builder-system get statefulset "buildkitd-$a" -o json |
    jq 'del(.status) | .metadata = {name: "admission-probe", namespace: .metadata.namespace,
      labels: .metadata.labels}'
}
buildkitd | check pass "a buildkitd StatefulSet"
buildkitd | jq '.spec.template.spec.containers[0].image = "docker.io/library/busybox:1.38.0"' |
  check fail "a privileged StatefulSet with another image"
buildkitd | jq '.spec.template.spec.volumes += [{name: "host", hostPath: {path: "/"}}]' |
  check fail "a StatefulSet with a hostPath volume"
# The API refuses hostNetwork with hostUsers false before the policy sees it.
buildkitd | jq '.spec.template.spec |= (del(.hostUsers) | .hostNetwork = true)' |
  check fail "a StatefulSet on the host network"

if ((failures)); then
  echo "$failures checks failed" >&2
  exit 1
fi
echo "all checks passed"
