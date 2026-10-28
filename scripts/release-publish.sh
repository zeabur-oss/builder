#!/usr/bin/env bash
# Publishes the images that release-build.sh built and the kubernetes job smoked, compiling
# nothing: each image by digest, one index per image, and the chart pinned to those indexes, all
# signed keyless (Sigstore, with the workflow's identity token). The shared release workflow
# attests what subjects.json lists; `scripts/release.sh assets` makes the draft's assets.
# CI only, as `scripts/release.sh publish`, inside `nix develop`; Docker is the runner's.
#   scripts/release-publish.sh v0.1.1 artifacts published
#
# <artifacts> holds candidate-linux-amd64/ and candidate-linux-arm64/ from release-build.sh; <out>
# receives subjects.json (the three image indexes and the chart, by digest), digests.txt (every
# image, index and the chart) and the chart's package.
set -euo pipefail

version="${1:?usage: release-publish.sh <vVERSION> <artifacts> <out>}"
artifacts="$(realpath "${2:?artifact directory required}")"
mkdir -p "${3:?output directory required}"
out="$(realpath "$3")"
: "${GITHUB_REPOSITORY:?}" "${GITHUB_ACTOR:?}" "${GITHUB_RUN_ID:?}" "${GITHUB_RUN_ATTEMPT:?}" "${GH_TOKEN:?}"
root="$(cd "$(dirname "$0")/.." && pwd)"
plain="${version#v}"
owner="${GITHUB_REPOSITORY%%/*}"
owner="${owner,,}"
charts="ghcr.io/$owner/charts"
names=(builder builder-job builder-railpack)
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
# Registry logins live in this run's own files, gone when it ends. Helm reads the same file.
export DOCKER_CONFIG="$work/docker" HELM_REGISTRY_CONFIG="$work/docker/config.json"
mkdir -p "$DOCKER_CONFIG"
auth=(--authfile "$DOCKER_CONFIG/config.json")

# Everything that can refuse does so before the first write.
# A release, an image tag or a chart version once published is never replaced.
if gh release view "$version" --repo "$GITHUB_REPOSITORY" >/dev/null 2>&1; then
  echo "release $version already exists; inspect it before trying again" >&2
  exit 1
fi
printf '%s' "$GH_TOKEN" | docker login ghcr.io --username "$GITHUB_ACTOR" --password-stdin
printf '%s' "$GH_TOKEN" | helm registry login ghcr.io --username "$GITHUB_ACTOR" --password-stdin
for ref in "${names[@]/#/ghcr.io/$owner/}" "$charts/zeabur-builder"; do
  if skopeo inspect --raw "${auth[@]}" "docker://$ref:$plain" >/dev/null 2>&1; then
    echo "$ref:$plain already exists; inspect it before trying again" >&2
    exit 1
  fi
done
for arch in amd64 arm64; do
  for name in "${names[@]}"; do
    [[ -f "$artifacts/candidate-linux-$arch/$name.tar" ]] || { echo "missing $name for $arch" >&2; exit 1; }
  done
done

# Each image: both architectures by digest, then one index of exactly those two.
cp -R "$root/deploy/chart" "$work/chart"
rm -f "$out/digests.txt"
subjects=()
for name in "${names[@]}"; do
  image="ghcr.io/$owner/$name"
  refs=()
  for arch in amd64 arm64; do
    candidate="$image:candidate-$GITHUB_RUN_ID-$GITHUB_RUN_ATTEMPT-$arch"
    # The input is this run's own smoked build, not a registry image: no signature policy applies.
    skopeo --insecure-policy copy "${auth[@]}" "docker-archive:$artifacts/candidate-linux-$arch/$name.tar" "docker://$candidate"
    digest="$(skopeo inspect "${auth[@]}" --format '{{.Digest}}' "docker://$candidate")"
    refs+=("$image@$digest")
    cosign sign --yes "$image@$digest"
    printf '%s  %s\n' "$name linux/$arch" "$image@$digest" >>"$out/digests.txt"
  done
  docker manifest create "$image:$plain" "${refs[@]}"
  docker manifest push "$image:$plain"
  index="sha256:$(skopeo inspect --raw "${auth[@]}" "docker://$image:$plain" | sha256sum | cut -d ' ' -f 1)"
  cosign sign --yes "$image@$index"
  printf '%s  %s\n' "$name index" "$image@$index" >>"$out/digests.txt"
  subjects+=("$image" "$index")

  # The chart value of this image: image, jobImage or railpackImage.
  case "$name" in
    builder) key=image ;;
    builder-job) key=jobImage ;;
    builder-railpack) key=railpackImage ;;
  esac
  # sed rather than `yq -i`, which would drop the comments and blank lines of values.yaml.
  sed -i "/^$key:\$/,/^[a-z]/ s|^  digest: \"\"\$|  digest: \"$index\"|" "$work/chart/values.yaml"
  KEY="$key" INDEX="$index" yq -e '.[strenv(KEY)].digest == strenv(INDEX)' "$work/chart/values.yaml" >/dev/null ||
    { echo "could not pin $key.digest in values.yaml" >&2; exit 1; }
done

# The chart at the version of Chart.yaml (checked equal to the release's by `release.sh check`).
helm lint --strict "$work/chart" --set registry.host=registry.example:5000
helm package "$work/chart" --destination "$out"
helm push "$out/zeabur-builder-$plain.tgz" "oci://$charts"
chart_digest="sha256:$(skopeo inspect --raw "${auth[@]}" "docker://$charts/zeabur-builder:$plain" | sha256sum | cut -d ' ' -f 1)"
cosign sign --yes "$charts/zeabur-builder@$chart_digest"
printf 'chart  %s\n' "$charts/zeabur-builder@$chart_digest" >>"$out/digests.txt"
subjects+=("$charts/zeabur-builder" "$chart_digest")

# What the workflow attests: [{"name": ..., "digest": ...}], the three indexes and the chart.
printf '%s\n' "${subjects[@]}" |
  jq -R -n '[inputs] as $s | [range(0; $s | length; 2) | {name: $s[.], digest: $s[. + 1]}]' \
    >"$out/subjects.json"
cat "$out/subjects.json" "$out/digests.txt"
