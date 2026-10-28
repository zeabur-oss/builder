#!/usr/bin/env bash
# One stage of a release, as the shared release workflow of zeabur-oss/.github runs it (from
# .github/workflows/release.yml). Each stage enters the Nix dev shell of the checkout in the
# current directory: with verify-only, the workflow runs a newer commit's scripts in the shell of
# the tag being verified. zeabur-oss-release (zeabur-oss/.github, release/) pushes, signs and
# verifies; this script keeps what is Zeabur Builder's own.
#
#   scripts/release.sh check   <version>                                the version is the crate's
#                                                                       and the chart's, with a
#                                                                       CHANGELOG.md section
#   scripts/release.sh build   <version> <os> <arch> <dist>             build the three images into
#                                                                       <dist>, smoke them on k3s
#   scripts/release.sh publish <version> <artifacts> <published>        push and sign the images
#                                                                       and the chart by digest
#   scripts/release.sh assets  <version> <artifacts> <release>          the draft's assets, signed
#                                                                       checksums and notes
#   scripts/release.sh verify  <version> <os> <arch> <downloaded>       check the draft as an
#                                                                       installer would
#
# <artifacts> holds the workflow's earlier artifacts: candidate-linux-<arch>/ from build and
# published/ from publish. Linux only; build is native (amd64 on x86_64, arm64 on aarch64).
set -euo pipefail

stage="${1:-}"
case "$stage:$#" in
  check:2 | publish:4 | assets:4 | build:5 | verify:5) ;;
  *)
    echo "usage: release.sh check|build|publish|assets|verify <version> ... (see the script's header)" >&2
    exit 2
    ;;
esac

if [[ -z "${ZEABUR_RELEASE_SHELL:-}" ]]; then
  export ZEABUR_RELEASE_SHELL=1
  exec nix develop "$PWD" --command "$0" "$@"
fi

scripts="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$scripts/.." && pwd)"
version="$2"
if [[ ! "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "expected a version like v0.1.1, not $version" >&2
  exit 2
fi
plain="${version#v}"
names=(builder builder-job builder-railpack)
chart_file="zeabur-builder-$plain.tgz"

# The chart value that pins each image: image, jobImage or railpackImage.
chart_key() {
  case "$1" in
    builder) echo image ;;
    builder-job) echo jobImage ;;
    builder-railpack) echo railpackImage ;;
  esac
}

# The one reference digests.txt records for a kind, such as `builder index` or `chart`.
recorded() { # <digests.txt> <kind>
  local ref
  ref="$(awk -v kind="$2" '{ ref = $NF; $NF = ""; sub(/ +$/, "") } $0 == kind { print ref }' "$1")"
  [[ "$ref" =~ ^[^[:space:]@]+@sha256:[0-9a-f]{64}$ ]] || { echo "$1: no single digest for $2" >&2; return 1; }
  echo "$ref"
}

case "$stage" in
  check)
    crate=$(cargo metadata --manifest-path "$root/Cargo.toml" --locked --no-deps --format-version 1 |
      jq -r '.packages[] | select(.name == "zeabur-builder") | .version')
    chart=$(yq '.version + " " + .appVersion' "$root/deploy/chart/Chart.yaml")
    status=0
    [[ "$crate" == "$plain" ]] || { echo "the crate zeabur-builder is $crate, not $plain" >&2; status=1; }
    [[ "$chart" == "$plain $plain" ]] ||
      { echo "deploy/chart/Chart.yaml has version and appVersion $chart, not $plain" >&2; status=1; }
    zeabur-oss-release notes "$root/CHANGELOG.md" "$version" >/dev/null || status=1
    exit $status
    ;;

  build)
    os="$3" arch="$4"
    [[ "$os" == linux ]] || { echo "the images are Linux only, not $os" >&2; exit 2; }
    case "$(uname -s):$arch:$(uname -m)" in
      Linux:amd64:x86_64 | Linux:arm64:aarch64) ;;
      *) echo "cannot build $arch here: each build is native and on Linux" >&2; exit 2 ;;
    esac
    mkdir -p "$5"
    dist="$(cd "$5" && pwd)"
    # mkShell sets SOURCE_DATE_EPOCH to 1980; the commit's time replaces it.
    SOURCE_DATE_EPOCH="$(git -C "$root" show -s --format=%ct HEAD)"
    export SOURCE_DATE_EPOCH
    "$scripts/build-images.sh" zeabur-builder "release-$arch"
    for name in "${names[@]}"; do
      docker image save --output "$dist/$name.tar" "zeabur-builder/$name:release-$arch"
    done
    # The images as built, with the chart of this commit, on k3s.
    ZEABUR_CI_SHELL=1 ZEABUR_CI_IMAGE_TAG="release-$arch" "$scripts/ci.sh" kubernetes
    ;;

  publish)
    artifacts="$(cd "$3" && pwd)"
    mkdir -p "$4"
    out="$(cd "$4" && pwd)"
    : "${GITHUB_REPOSITORY:?}" "${GITHUB_ACTOR:?}" "${GH_TOKEN:?}"
    owner="${GITHUB_REPOSITORY%%/*}"
    owner="${owner,,}"
    charts="ghcr.io/$owner/charts"
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT
    # Registry logins live in this run's own files, gone when it ends. Helm reads the same file.
    export DOCKER_CONFIG="$work/docker" HELM_REGISTRY_CONFIG="$work/docker/config.json"
    mkdir -p "$DOCKER_CONFIG"

    # zeabur-oss-release refuses an existing tag for each push; this refuses all of them before the
    # first, so a release never stops halfway on a tag that was there from the start.
    if gh release view "$version" --repo "$GITHUB_REPOSITORY" >/dev/null 2>&1; then
      echo "release $version already exists; inspect it before trying again" >&2
      exit 1
    fi
    printf '%s' "$GH_TOKEN" | docker login ghcr.io --username "$GITHUB_ACTOR" --password-stdin
    printf '%s' "$GH_TOKEN" | helm registry login ghcr.io --username "$GITHUB_ACTOR" --password-stdin
    for ref in "${names[@]/#/ghcr.io/$owner/}" "$charts/zeabur-builder"; do
      if skopeo inspect --raw --authfile "$DOCKER_CONFIG/config.json" "docker://$ref:$plain" >/dev/null 2>&1; then
        echo "$ref:$plain already exists; inspect it before trying again" >&2
        exit 1
      fi
    done

    cp -R "$root/deploy/chart" "$work/chart"
    rm -f "$out/digests.txt" "$out/subjects.json"
    for name in "${names[@]}"; do
      zeabur-oss-release push-image --repository "ghcr.io/$owner/$name" --tag "$plain" --label "$name" \
        --digests "$out/digests.txt" --subjects "$out/subjects.json" \
        "linux/amd64=$artifacts/candidate-linux-amd64/$name.tar" \
        "linux/arm64=$artifacts/candidate-linux-arm64/$name.tar"
      index="$(recorded "$out/digests.txt" "$name index")"
      index="${index#*@}"
      key="$(chart_key "$name")"
      # sed rather than `yq -i`, which would drop the comments and blank lines of values.yaml.
      sed -i "/^$key:\$/,/^[a-z]/ s|^  digest: \"\"\$|  digest: \"$index\"|" "$work/chart/values.yaml"
      KEY="$key" INDEX="$index" yq -e '.[strenv(KEY)].digest == strenv(INDEX)' "$work/chart/values.yaml" >/dev/null ||
        { echo "could not pin $key.digest in values.yaml" >&2; exit 1; }
    done

    # The chart at the version of Chart.yaml (checked equal to the release's by `release.sh check`).
    helm lint --strict "$work/chart" --set registry.host=registry.example:5000
    helm package "$work/chart" --destination "$out"
    zeabur-oss-release push-chart --repository "$charts" \
      --digests "$out/digests.txt" --subjects "$out/subjects.json" "$out/$chart_file"
    cat "$out/subjects.json" "$out/digests.txt"
    ;;

  assets)
    published="$3/published"
    mkdir -p "$4/assets"
    out="$(cd "$4" && pwd)"
    cp "$published/$chart_file" "$published/digests.txt" "$out/assets/"
    recorded "$out/assets/digests.txt" chart >/dev/null
    zeabur-oss-release checksums "$out/assets"
    zeabur-oss-release notes "$root/CHANGELOG.md" "$version" --docs https://oss.zeabur.com/docs/builder/ \
      >"$out/notes.md"
    ;;

  verify)
    os="$3" arch="$4"
    [[ "$os" == linux ]] || { echo "the release is Linux only, not $os" >&2; exit 2; }
    directory="$(cd "$5" && pwd)"
    echo "verifying $version on $os/$arch"
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT
    # A fresh, empty registry configuration: a publisher's login must not hide a private package.
    export DOCKER_CONFIG="$work/docker" HELM_REGISTRY_CONFIG="$work/docker/config.json"
    mkdir -p "$DOCKER_CONFIG"
    release=(--repository zeabur-oss/builder --version "$version")

    zeabur-oss-release verify-assets "${release[@]}" "$directory" digests.txt "$chart_file"
    values="$(helm show values "$directory/$chart_file")"
    for name in "${names[@]}"; do
      zeabur-oss-release verify-image "${release[@]}" --digests "$directory/digests.txt" --label "$name" \
        linux/amd64 linux/arm64
      # The chart installs this index by default.
      index="$(recorded "$directory/digests.txt" "$name index")"
      key="$(chart_key "$name")"
      pinned="$(KEY="$key" yq '.[strenv(KEY)].repository + "@" + .[strenv(KEY)].digest' <<<"$values")"
      [[ "$pinned" == "$index" ]] || { echo "the chart's $key is $pinned, not $index" >&2; exit 1; }
    done
    zeabur-oss-release verify-chart "${release[@]}" --digests "$directory/digests.txt" "$directory/$chart_file"
    helm template builder "$directory/$chart_file" --set registry.host=registry.example:5000 \
      --set registry.tokenKey.namespace=zot --set registry.tokenKey.secret=zot-token-signer >/dev/null
    echo "release $version verified"
    ;;
esac
