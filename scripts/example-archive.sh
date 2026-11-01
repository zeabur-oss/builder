#!/usr/bin/env bash
# Builds an uploaded archive and checks that only its own Build can read it:
#   scripts/deploy-example.sh <context>
#   scripts/example-archive.sh <context>
# Asks A/app for an upload token, pushes a small Dockerfile app with it to
# <registry>/A/app/source, as docs/builds/archive-source.mdx describes, and builds it in A. A Build of the same name in B and another
# Build in A name the same digest and must fail at the source. Deleting A/app removes its
# repositories, while A/handmade, pushed by hand, stays through a Build of that name being created
# and deleted. Removes its Builds at the end.
# Environment: A and B (team-a, team-b), TIMEOUT (600).
set -euo pipefail

context="${1:?usage: example-archive.sh <context>}"
a="${A:-team-a}"
b="${B:-team-b}"
timeout="${TIMEOUT:-600}"
registry=registry.builder-registry.svc.cluster.local:5000

k() { kubectl --context "$context" "$@"; }
failures=0
ok() { echo "ok    $1"; }
wrong() {
  echo "WRONG $1"
  failures=$((failures + 1))
}

cleanup() {
  k -n "$a" delete build app other handmade --ignore-not-found --wait=false >/dev/null 2>&1 || true
  k -n "$a" delete pod registry-check --ignore-not-found --wait=false >/dev/null 2>&1 || true
  k -n "$b" delete build app --ignore-not-found --wait=false >/dev/null 2>&1 || true
}
trap cleanup EXIT

build() { # $1 = namespace, $2 = name, $3 = source
  cat <<EOF
apiVersion: builder.zeabur.com/v1alpha1
kind: Build
metadata: {name: $2, namespace: $1}
spec:
  source: $3
  builderRef: dockerfile
EOF
}

# A Build without a digest waits for its upload. Anyone who may edit it asks for a token.
build "$a" app "{archive: {}}" | k apply -f - >/dev/null
k -n "$a" annotate build/app --overwrite builder.zeabur.com/upload-token-request=archive-test >/dev/null
k -n "$a" wait build/app --timeout 60s --for=jsonpath='{.status.uploadToken.request}'=archive-test >/dev/null
token=$(k -n "$a" get secret app-upload -o jsonpath='{.data.token}' | base64 -d)

# The client side: pack the folder, push the empty config and the layer, then the manifest.
push=$(k -n "$a" run archive-push --rm -i --quiet --restart Never \
  --image docker.io/curlimages/curl:8.11.1 --env "AUTH=Authorization: Bearer $token" \
  --env "BASE=http://$registry" \
  --env "REPO=$a/app/source" --command -- sh -eu -s <<'EOF'
cd /tmp && mkdir -p app/static
printf 'hello from an archive\n' > app/static/hello.txt
ln -s static/hello.txt app/hello.txt
printf 'FROM busybox:1.37.0\nCOPY hello.txt /hello.txt\nCMD ["cat", "/hello.txt"]\n' > app/Dockerfile
tar -czf app.tar.gz -C app .
printf '{}' > config.json
blob() { # $1 = file; prints its digest
  digest="sha256:$(sha256sum "$1" | cut -d' ' -f1)"
  location=$(curl -fsS -H "$AUTH" -X POST -o /dev/null -D - "$BASE/v2/$REPO/blobs/uploads/" |
    tr -d '\r' | sed -n 's/^[Ll]ocation: //p')
  case $location in /*) location="$BASE$location" ;; esac
  case $location in *\?*) sep='&' ;; *) sep='?' ;; esac
  curl -fsS -H "$AUTH" -X PUT -H 'Content-Type: application/octet-stream' \
    --data-binary "@$1" "$location${sep}digest=$digest" -o /dev/null
  echo "$digest"
}
config=$(blob config.json)
layer=$(blob app.tar.gz)
size=$(wc -c < app.tar.gz | tr -d ' ')
printf '{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","artifactType":"application/vnd.zeabur.source.v1+tar","config":{"mediaType":"application/vnd.oci.empty.v1+json","digest":"%s","size":2},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"%s","size":%s}]}' \
  "$config" "$layer" "$size" > manifest.json
curl -fsS -H "$AUTH" -X PUT -H 'Content-Type: application/vnd.oci.image.manifest.v1+json' \
  --data-binary @manifest.json "$BASE/v2/$REPO/manifests/upload-$(date +%s)" -o /dev/null
echo "digest=sha256:$(sha256sum manifest.json | cut -d' ' -f1)"
EOF
)
digest=$(sed -n 's/^digest=//p' <<<"$push")
[[ $digest =~ ^sha256:[0-9a-f]{64}$ ]] || {
  echo "the push printed no digest: $push" >&2
  exit 1
}
echo "pushed $registry/$a/app/source@$digest"

archive="{archive: {digest: $digest}}"

echo "== admission"
refused() { # $1 = what, then the manifest on stdin
  if out=$(k apply --dry-run=server -f - 2>&1); then
    wrong "$1 is admitted"
  else
    ok "$1 is refused: $(grep -oE 'set exactly one[^"]*|an archive source has no webhook trigger|should match [^"]*' <<<"$out" | head -n1)"
  fi
}
build "$a" app "{git: {url: https://github.com/traefik/whoami}, archive: {digest: $digest}}" |
  refused "a source with git and archive"
build "$a" app "{}" | refused "a source with neither git nor archive"
{ build "$a" app "$archive"; echo "  triggers: {webhook: {secretRef: {name: hook, key: secret}}}"; } |
  refused "an archive source with a webhook"
build "$a" app "{archive: {digest: sha256:abc}}" | refused "a short digest"

build "$a" app "$archive" | k apply -f - >/dev/null
build "$a" other "$archive" | k apply -f - >/dev/null
build "$b" app "$archive" | k apply -f - >/dev/null

echo "== builds"
wait_for() { # $1 = namespace, $2 = name; prints the image, or the failed run's reason and message
  local deadline=$(($(date +%s) + timeout)) image failed
  while (($(date +%s) < deadline)); do
    image=$(k -n "$1" get build "$2" -o jsonpath='{.status.latestImage}')
    if [[ -n $image ]]; then
      echo "image $image"
      return
    fi
    failed=$(k -n "$1" get buildrun -l "builder.zeabur.com/build=$2" \
      -o jsonpath='{range .items[?(@.status.phase=="Failed")]}{.status.reason}{": "}{.status.message}{"\n"}{end}')
    if [[ -n $failed ]]; then
      echo "failed $failed"
      return
    fi
    sleep 5
  done
  echo "timeout"
}

result=$(wait_for "$a" app)
if [[ $result == image* ]]; then
  ok "$a/app builds its own archive: ${result#image }"
  revision=$(k -n "$a" get buildrun -l builder.zeabur.com/build=app -o jsonpath='{.items[0].spec.revision}')
  if [[ $revision == "$digest" ]]; then ok "the BuildRun records the digest"; else wrong "the BuildRun records $revision"; fi
else
  wrong "$a/app: $result"
  k -n "$a" logs -l builder.zeabur.com/build=app --all-containers --tail 20 || true
fi
for target in "$b/app" "$a/other"; do
  result=$(wait_for "${target%%/*}" "${target#*/}")
  if [[ $result == "failed SourceFailed"* ]]; then
    ok "$target cannot read $a/app's archive: ${result#failed }"
  else
    wrong "$target: $result"
  fi
done

echo "== cleanup"
admin=$(k -n builder-system exec deploy/zeabur-builder -- /zeabur-builder registry-token \
  --repository "$a/handmade" --repository "$a/app" --repository "$a/app/source" --push --ttl 1800)
k -n "$a" run registry-check --restart Never --image docker.io/curlimages/curl:8.11.1 \
  --command -- sleep 1800 >/dev/null
k -n "$a" wait pod/registry-check --for=condition=Ready --timeout 120s >/dev/null
check() {
  k -n "$a" exec -i registry-check -- \
    env "AUTH=Authorization: Bearer $admin" "BASE=http://$registry" "$@"
}
status() { # $1 = path below /v2/
  # shellcheck disable=SC2016 # expanded in the Pod
  check sh -c 'curl -s -o /dev/null -w "%{http_code}" -H "$AUTH" "$BASE/v2/$0"' "$1"
}
tagged() { # $1 = repository; succeeds when it has a tag. Zot answers [] or 404 for none.
  # shellcheck disable=SC2016 # expanded in the Pod
  check sh -c 'curl -s -H "$AUTH" "$BASE/v2/$0/tags/list"' "$1" | grep -q '"tags":\["'
}

# An image someone pushed to a path of the Build layout, before any Build of that name.
check env "REPO=$a/handmade" sh -eu -s <<'EOF'
cd /tmp && printf '{}' > empty
digest="sha256:$(sha256sum empty | cut -d' ' -f1)"
location=$(curl -fsS -H "$AUTH" -X POST -o /dev/null -D - "$BASE/v2/$REPO/blobs/uploads/" |
  tr -d '\r' | sed -n 's/^[Ll]ocation: //p')
case $location in /*) location="$BASE$location" ;; esac
case $location in *\?*) sep='&' ;; *) sep='?' ;; esac
curl -fsS -H "$AUTH" -X PUT -H 'Content-Type: application/octet-stream' \
  --data-binary @empty "$location${sep}digest=$digest" -o /dev/null
printf '{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","artifactType":"application/vnd.example.handmade","config":{"mediaType":"application/vnd.oci.empty.v1+json","digest":"%s","size":2},"layers":[{"mediaType":"application/vnd.oci.empty.v1+json","digest":"%s","size":2}]}' \
  "$digest" "$digest" > manifest.json
curl -fsS -H "$AUTH" -X PUT -H 'Content-Type: application/vnd.oci.image.manifest.v1+json' \
  --data-binary @manifest.json "$BASE/v2/$REPO/manifests/1" -o /dev/null
EOF

recorded=$(k -n builder-system get configmap -l builder.zeabur.com/repositories=true -o json |
  jq -r --arg ns "$a" '.items[].data | select(.namespace == $ns and .name == "app") | .repositories')
if [[ $recorded == "$a/app"$'\n'"$a/app/source" ]]; then
  ok "the controller records $a/app's repositories"
else
  wrong "the record of $a/app: $recorded"
fi

# A Build of that name claims its repositories when it starts and clears them when it goes. The
# controller handles delete events in order, so A/handmade is done once A/app's repositories are.
build "$a" handmade "{archive: {}}" | k apply -f - >/dev/null
k -n "$a" wait build/handmade --timeout 60s --for=jsonpath='{.status.observedGeneration}'=1 >/dev/null
k -n "$a" delete build handmade >/dev/null
k -n "$a" delete build app >/dev/null
deadline=$(($(date +%s) + 120))
while (($(date +%s) < deadline)); do
  tagged "$a/app" || tagged "$a/app/source" || break
  sleep 3
done
for repository in "$a/app" "$a/app/source"; do
  if tagged "$repository"; then wrong "$repository has tags after deleting $a/app"; else ok "deleting $a/app deletes $repository"; fi
done
code=$(status "$a/handmade/manifests/1")
if [[ $code == 200 ]]; then ok "$a/handmade, pushed by hand, stays"; else wrong "$a/handmade after its Build came and went: $code"; fi

if ((failures)); then
  echo "$failures checks failed" >&2
  exit 1
fi
echo "all checks passed"
