#!/usr/bin/env bash
# Lints the chart and checks what an install needs: the ClusterBuilders, the buildkitd config and
# egress policy, the admission policy, images pinned by digest when a digest is set, and webhooks
# off unless enabled.
set -euo pipefail

chart="$(dirname "$0")/../deploy/chart"
version=$(helm show chart "$chart" | yq .version)
version=${version//./\\.}
base=(--set registry.host=registry.example:5000 --set registry.tokenSecretRef.namespace=zot
  --set registry.tokenSecretRef.name=zot-token-signer)

helm lint --strict "$chart" "${base[@]}"
helm lint --strict "$chart" "${base[@]}" --set webhooks.enabled=true \
  --set webhooks.ingress.enabled=true --set webhooks.ingress.host=hooks.example.com
helm lint --strict "$chart" "${base[@]}" --set webhooks.enabled=true \
  --set webhooks.httpRoute.enabled=true --set 'webhooks.httpRoute.parentRefs[0].name=gateway'

off=$(helm template builder "$chart" "${base[@]}" --set webhooks.ingress.enabled=true)
if rg -q 'kind: (Service|Ingress|HTTPRoute)$|--webhook-addr' <<<"$off"; then
  echo "webhooks are exposed although webhooks.enabled is false" >&2
  exit 1
fi

on=$(helm template builder "$chart" "${base[@]}" --set webhooks.enabled=true \
  --set webhooks.path=/git/ --set webhooks.ingress.enabled=true \
  --set webhooks.ingress.host=hooks.example.com)
for want in 'kind: Ingress' '--webhook-path=/git$' 'path: /git/$' 'host: "hooks.example.com"'; do
  if ! rg -q -- "$want" <<<"$on"; then
    echo "the enabled chart lacks $want" >&2
    exit 1
  fi
done
require() { # $1 = rendered chart, $2 = what is checked, then patterns it must hold
  local out=$1 what=$2
  shift 2
  for want in "$@"; do
    if ! rg -q -- "$want" <<<"$out"; then
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
  '--buildkitd-image=docker.io/moby/buildkit:' '--buildkitd-cpu-limit=1$' '--buildkitd-memory-limit=1Gi$' \
  '--job-memory-request=128Mi$' '--job-memory-limit=512Mi$' \
  '--max-concurrent-builds=1$' '--max-cluster-builds=1$' "c.image == 'docker.io/moby/buildkit:" \
  "request.namespace in \['default', 'zot'\]"
if rg -q '^\s+\[registry\.|ZEABUR_BUILDER_REGISTRY_(CA|HTTP)|registry-ca' <<<"$plain"; then
  echo "default install: the chart sets registry http or a CA that nobody asked for" >&2
  exit 1
fi

require "$plain" "default image refs" "--github-app-secret=github-app\$" \
  '--github-app-grants=github-app-installations$' \
  '--buildkitd-image=docker.io/moby/buildkit:v[0-9.]+@sha256:[0-9a-f]{64}$' \
  "c.image == 'docker.io/moby/buildkit:v[0-9.]+@sha256:[0-9a-f]{64}'" \
  'source: "ghcr.io/railwayapp/railpack-frontend:v[0-9.]+"$'
renamed=$(helm template builder "$chart" "${base[@]}" --set githubApp.secretRef.name=app \
  --set githubApp.grantsConfigMapRef.name=grants --set webhooks.enabled=true \
  --set webhooks.ingress.enabled=true --set webhooks.ingress.host=hooks.example.com \
  --set webhooks.ingress.tls.secretName=hooks-tls --set buildkitdImage.tag=v9.9.9 \
  --set buildkitdImage.digest= --set railpackFrontend.digest="sha256:$(printf '1%.0s' {1..64})")
require "$renamed" "renamed values" '--github-app-secret=app$' '--github-app-grants=grants$' \
  'secretName: hooks-tls$' '--buildkitd-image=docker.io/moby/buildkit:v9.9.9$' \
  "c.image == 'docker.io/moby/buildkit:v9.9.9'" \
  'source: "ghcr.io/railwayapp/railpack-frontend:v[0-9.]+@sha256:1{64}"$'
for old in githubApp.secretName=x:githubApp.secretRef.name \
  githubApp.grantsName=x:githubApp.grantsConfigMapRef.name \
  webhooks.ingress.tlsSecretName=x:webhooks.ingress.tls.secretName \
  buildkitdImage=x:buildkitdImage railpackFrontend=x:railpackFrontend; do
  if err=$(helm template builder "$chart" "${base[@]}" --set "${old%%:*}" 2>&1); then
    echo "renamed values: the chart renders with ${old%%:*}" >&2
    exit 1
  fi
  if ! rg -q -F -- "${old##*:}" <<<"$err"; then
    echo "renamed values: the error for ${old%%:*} does not name ${old##*:}" >&2
    exit 1
  fi
done

if rg -q 'name: agent$' <<<"$plain"; then
  echo "default install: the chart installs the agent builder that nobody asked for" >&2
  exit 1
fi
agent=$(helm template builder "$chart" "${base[@]}" --set agent.enabled=true \
  --set agent.provider=anthropic --set agent.model=claude-opus-5-5 \
  --set agent.auth.type=apiKey --set agent.auth.apiKey.name=model \
  --set agent.auth.apiKey.key=anthropic)
require "$agent" "agent builder" 'name: agent$' 'type: agent$' 'timeout: 25m$' \
  "image: \"ghcr.io/zeabur-oss/builder-job:$version\"\$" 'provider: "anthropic"$' \
  'model: "claude-opus-5-5"$' 'type: apiKey$' 'name: model$' 'key: anthropic$'
if helm template builder "$chart" "${base[@]}" --set agent.enabled=true >/dev/null 2>&1; then
  echo "agent builder: the chart installs it without a model" >&2
  exit 1
fi
if helm template builder "$chart" "${base[@]}" --set agent.enabled=true \
  --set agent.provider=anthropic --set agent.model=claude-opus-5-5 >/dev/null 2>&1; then
  echo "agent builder: the chart installs it without auth" >&2
  exit 1
fi

digest=sha256:$(printf '0%.0s' {1..64})
pinned=$(helm template builder "$chart" "${base[@]}" --set image.digest="$digest" \
  --set jobImage.digest="$digest" --set railpackImage.digest="$digest" \
  --set registry.caSecretRef.namespace=zot --set registry.caSecretRef.name=zot-tls \
  --set 'admissionPolicy.extraImages[0]=example.com/planner:1' \
  --set registry.inCluster.namespace=zot --set registry.inCluster.podLabels.app=zot)
require "$pinned" "pinned install" "image: \"ghcr.io/zeabur-oss/builder@$digest\"" \
  "value: \"ghcr.io/zeabur-oss/builder-job@$digest\"" "image: \"ghcr.io/zeabur-oss/builder-railpack@$digest\"" \
  'ca = \["/etc/buildkit-registry-ca/ca.crt"\]' 'kubernetes.io/metadata.name: zot' \
  'name: ZEABUR_BUILDER_REGISTRY_CA$' 'value: "zot/zot-tls"$' 'value: "ca.crt"$' \
  'resourceNames: \[zot-tls\]$' 'verbs: \[get, list, watch\]$' 'resourceNames: \[registry-ca\]$' \
  'name: "system:authenticated"' \
  "c.image in \['ghcr.io/zeabur-oss/builder-job@$digest', 'ghcr.io/zeabur-oss/builder-railpack@$digest', 'example.com/planner:1'\]" \
  "request.namespace in \['default', 'zot'\]"
if rg -q -- '--buildkitd-.*-max' <<<"$plain"; then
  echo "default install: the chart sets a build ceiling that nobody asked for" >&2
  exit 1
fi
ceiling=$(helm template builder "$chart" "${base[@]}" --set buildkitd.resources.max.cpu=4 \
  --set buildkitd.resources.max.memory=8Gi)
require "$ceiling" "build ceiling" '--buildkitd-cpu-max=4$' '--buildkitd-memory-max=8Gi$'
http=$(helm template builder "$chart" "${base[@]}" --set registry.http=true)
require "$http" "plain HTTP registry" 'http = true$' 'name: ZEABUR_BUILDER_REGISTRY_HTTP, value: "true"'
if rg -q -- '--build-(node-selector|toleration)' <<<"$plain"; then
  echo "default install: the chart places builds although builds is empty" >&2
  exit 1
fi
placed=$(helm template builder "$chart" "${base[@]}" \
  --set-json 'builds.nodeSelector={"builder.zeabur.com/builds":"true"}' \
  --set-json 'builds.tolerations=[{"key":"builder.zeabur.com/builds","operator":"Equal","value":"true","effect":"NoSchedule"}]')
require "$placed" "build node" '- "--build-node-selector=builder.zeabur.com/builds=true"$' \
  '- "--build-toleration=\{\\"effect\\":\\"NoSchedule\\",\\"key\\":\\"builder.zeabur.com/builds\\",\\"operator\\":\\"Equal\\",\\"value\\":\\"true\\"\}"$'
controller() { # $1 = rendered chart: the controller's Pod spec and metadata
  yq 'select(.kind == "Deployment" and .metadata.name == "zeabur-builder") | .spec.template' <<<"$1"
}
if controller "$plain" | rg -q '^\s+(annotations|imagePullSecrets|nodeSelector|tolerations|affinity):'; then
  echo "default install: the controller's Pod has settings that nobody asked for" >&2
  exit 1
fi
require "$(controller "$plain")" "controller Pod defaults" 'runAsNonRoot: true$' 'type: RuntimeDefault$'
pod=$(helm template builder "$chart" "${base[@]}" \
  --set-json 'imagePullSecrets=[{"name":"mirror"}]' \
  --set-json 'podLabels={"team":"builds","app.kubernetes.io/name":"other"}' \
  --set podAnnotations.note=kept --set-json 'podSecurityContext={"runAsUser":2000}' \
  --set-json 'nodeSelector={"pool":"system"}' \
  --set-json 'tolerations=[{"key":"system","operator":"Exists"}]' \
  --set-json 'affinity={"nodeAffinity":{"requiredDuringSchedulingIgnoredDuringExecution":{"nodeSelectorTerms":[{"matchExpressions":[{"key":"zone","operator":"In","values":["a"]}]}]}}}')
pod=$(controller "$pod")
require "$pod" "controller Pod values" '^\s+- name: mirror$' 'team: builds$' \
  'app.kubernetes.io/name: zeabur-builder$' 'note: kept$' 'runAsUser: 2000$' 'pool: system$' \
  'key: system$' 'key: zone$'
if rg -q 'app.kubernetes.io/name: other$' <<<"$pod"; then
  echo "controller Pod values: podLabels replaced a selector label" >&2
  exit 1
fi
tokens=$plain
require "$tokens" "registry tokens" 'value: "zot/zot-token-signer"$' 'namespace: zot$' \
  'resourceNames: \[zot-token-signer\]$' "request.namespace in \['default', 'zot'\]"
for half in namespace name; do
  if helm template builder "$chart" "${base[@]}" --set "registry.caSecretRef.$half=x" >/dev/null 2>&1; then
    echo "registry CA: the chart renders with registry.caSecretRef.$half alone" >&2
    exit 1
  fi
done
for missing in "registry.tokenSecretRef.namespace=" "registry.tokenSecretRef.name="; do
  if err=$(helm template builder "$chart" --set registry.host=registry.example:5000 \
    --set registry.tokenSecretRef.namespace=zot --set registry.tokenSecretRef.name=zot-token-signer \
    --set "$missing" 2>&1); then
    echo "missing token key: the chart renders without ${missing%=}" >&2
    exit 1
  fi
  if ! rg -qF -- "${missing%=} is required" <<<"$err"; then
    echo "missing token key: the error does not name ${missing%=}" >&2
    exit 1
  fi
done
echo "chart ok"
