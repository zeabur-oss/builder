#!/usr/bin/env bash
# One stage of a release, as the shared release workflow of zeabur-oss/.github runs it (from
# .github/workflows/release.yml). Each stage enters the Nix dev shell of the checkout in the
# current directory: with verify-only, the workflow runs a newer commit's scripts in the shell of
# the tag being verified. crane pushes the images, helm the chart, cosign signs and verifies them.
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

# The version's CHANGELOG.md section, as the release body. Its lines are not wrapped: a release
# body shows every line break.
notes() {
  awk -v v="$plain" '/^## /{ if (p) exit; p = ($2 == v || $2 == "[" v "]"); next } p' "$root/CHANGELOG.md"
}

# Fails unless the registry answers that the reference does not exist.
absent() { # <ref>
  local err
  if err="$(crane digest "$1" 2>&1)"; then
    echo "$1 already exists; inspect it before trying again" >&2
    return 1
  fi
  [[ "$err" == *MANIFEST_UNKNOWN* ]] || { echo "cannot check $1: $err" >&2; return 1; }
}

# Appends a line to digests.txt and, for what is attested, an entry to subjects.json.
record() { # <kind> <repository>@<digest> [attest]
  printf '%s  %s\n' "$1" "$2" >>"$out/digests.txt"
  if [[ -n "${3:-}" ]]; then
    jq -c --arg name "${2%@*}" --arg digest "${2#*@}" '. + [{name: $name, digest: $digest}]' \
      "$out/subjects.json" >"$out/subjects.json.new"
    mv "$out/subjects.json.new" "$out/subjects.json"
  fi
}

# The platform of a `docker image save` archive, from its image config.
platform() { # <image.tar>
  local config
  config="$(tar -xOf "$1" manifest.json | jq -r 'if length == 1 then .[0].Config else error("not one image") end')"
  tar -xOf "$1" "$config" | jq -r '.os + "/" + .architecture'
}

# Who signed a release: the shared release workflow, run for this repository at the tag.
signer() {
  : "${RELEASE_CERT_IDENTITY_REGEXP:?}" "${RELEASE_SIGNER_WORKFLOW:?}" "${RELEASE_SOURCE_REPOSITORY:?}"
  [[ "${RELEASE_SOURCE_REF:?}" == "refs/tags/$version" ]] ||
    { echo "RELEASE_SOURCE_REF is $RELEASE_SOURCE_REF, not refs/tags/$version" >&2; exit 1; }
  cosign_id=(--certificate-oidc-issuer https://token.actions.githubusercontent.com
    --certificate-identity-regexp "$RELEASE_CERT_IDENTITY_REGEXP"
    --certificate-github-workflow-repository "$RELEASE_SOURCE_REPOSITORY"
    --certificate-github-workflow-ref "$RELEASE_SOURCE_REF")
  gh_id=(--repo "$RELEASE_SOURCE_REPOSITORY" --signer-workflow "$RELEASE_SIGNER_WORKFLOW"
    --source-ref "$RELEASE_SOURCE_REF")
}

# The chart layer of a chart in the registry is the packaged file.
chart_is() { # <repository>@<digest> <chart.tgz>
  local layer
  layer="$(crane manifest "$1" |
    jq -r '[.layers[] | select(.mediaType == "application/vnd.cncf.helm.chart.content.v1.tar+gzip")]
      | if length == 1 then .[0].digest else error("not one chart layer") end')"
  [[ "$layer" == "sha256:$(sha256sum "$2" | cut -d' ' -f1)" ]] ||
    { echo "$1 is not $2" >&2; return 1; }
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
    # Cargo only takes SemVer, so this also refuses a tag that is not v and SemVer. An image tag
    # cannot hold the + of build metadata.
    [[ "$version" != *+* ]] || { echo "$version has build metadata, which an image tag cannot hold" >&2; exit 1; }
    [[ "$version" == "v$crate" ]] || { echo "the tag $version is not v$crate, the crate zeabur-builder's version" >&2; status=1; }
    [[ "$chart" == "$plain $plain" ]] ||
      { echo "deploy/chart/Chart.yaml has version and appVersion $chart, not $plain" >&2; status=1; }
    [[ -n "$(notes | tr -d '[:space:]')" ]] || { echo "CHANGELOG.md has no section for $plain" >&2; status=1; }
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

    # Every tag is refused before the first push, so a release never stops halfway on a tag that was
    # there from the start.
    if gh release view "$version" --repo "$GITHUB_REPOSITORY" >/dev/null 2>&1; then
      echo "release $version already exists; inspect it before trying again" >&2
      exit 1
    fi
    printf '%s' "$GH_TOKEN" | crane auth login ghcr.io --username "$GITHUB_ACTOR" --password-stdin
    for ref in "${names[@]/#/ghcr.io/$owner/}" "$charts/zeabur-builder"; do
      absent "$ref:$plain"
    done

    cp -R "$root/deploy/chart" "$work/chart"
    rm -f "$out/digests.txt"
    echo '[]' >"$out/subjects.json"
    for name in "${names[@]}"; do
      repo="ghcr.io/$owner/$name"
      # Each platform image by its digest alone; the index that lists them is pushed last, with the
      # tag, so a run that fails before it leaves no tag and can be repeated.
      manifests=()
      for arch in amd64 arm64; do
        tar="$artifacts/candidate-linux-$arch/$name.tar"
        [[ "$(platform "$tar")" == "linux/$arch" ]] || { echo "$tar is not a linux/$arch image" >&2; exit 1; }
        ref="$repo@$(crane digest --tarball "$tar")"
        crane push "$tar" "$ref"
        cosign sign --yes "$ref"
        record "$name linux/$arch" "$ref"
        manifests+=(--manifest "$ref")
      done
      ref="$(crane index append --docker-empty-base "${manifests[@]}" --tag "$repo:$plain")"
      cosign sign --yes "$ref"
      record "$name index" "$ref" attest
      index="${ref#*@}"
      key="$(chart_key "$name")"
      # sed rather than `yq -i`, which would drop the comments and blank lines of values.yaml.
      sed -i "/^$key:\$/,/^[a-z]/ s|^  digest: \"\"\$|  digest: \"$index\"|" "$work/chart/values.yaml"
      KEY="$key" INDEX="$index" yq -e '.[strenv(KEY)].digest == strenv(INDEX)' "$work/chart/values.yaml" >/dev/null ||
        { echo "could not pin $key.digest in values.yaml" >&2; exit 1; }
    done

    # The chart at the version of Chart.yaml (checked equal to the release's by `release.sh check`).
    helm lint --strict "$work/chart" --set registry.host=registry.example:5000
    helm package "$work/chart" --destination "$out"
    helm push "$out/$chart_file" "oci://$charts"
    ref="$charts/zeabur-builder@$(crane digest "$charts/zeabur-builder:$plain")"
    chart_is "$ref" "$out/$chart_file"
    cosign sign --yes "$ref"
    record chart "$ref" attest
    cat "$out/subjects.json" "$out/digests.txt"
    ;;

  assets)
    published="$3/published"
    mkdir -p "$4/assets"
    out="$(cd "$4" && pwd)"
    cp "$published/$chart_file" "$published/digests.txt" "$out/assets/"
    recorded "$out/assets/digests.txt" chart >/dev/null
    (cd "$out/assets" && LC_ALL=C sha256sum -- * >SHA256SUMS)
    cosign sign-blob --yes --bundle "$out/assets/SHA256SUMS.sigstore.json" "$out/assets/SHA256SUMS"
    {
      notes
      printf '\nInstall, upgrade and verify: [oss.zeabur.com/docs/builder](https://oss.zeabur.com/docs/builder/)\n'
    } >"$out/notes.md"
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
    signer

    # SHA256SUMS is signed and lists exactly the assets.
    cosign verify-blob "${cosign_id[@]}" --bundle "$directory/SHA256SUMS.sigstore.json" "$directory/SHA256SUMS"
    listed="$(awk '{ print $2 }' "$directory/SHA256SUMS" | sort)"
    [[ "$listed" == "$(printf '%s\n' digests.txt "$chart_file" | sort)" ]] ||
      { echo "SHA256SUMS lists ${listed//$'\n'/ }, not digests.txt and $chart_file" >&2; exit 1; }
    (cd "$directory" && sha256sum --check --strict SHA256SUMS)
    for asset in digests.txt "$chart_file"; do
      gh attestation verify "$directory/$asset" "${gh_id[@]}"
    done

    values="$(helm show values "$directory/$chart_file")"
    for name in "${names[@]}"; do
      # The index holds exactly the recorded platform images, each signed.
      index="$(recorded "$directory/digests.txt" "$name index")"
      want="$(for arch in amd64 arm64; do
        ref="$(recorded "$directory/digests.txt" "$name linux/$arch")"
        [[ "${ref%@*}" == "${index%@*}" ]] || { echo "$ref is not in ${index%@*}" >&2; exit 1; }
        echo "linux/$arch ${ref#*@}"
      done)"
      got="$(crane manifest "$index" | jq -r '.manifests[] | "\(.platform.os)/\(.platform.architecture) \(.digest)"' | sort)"
      [[ "$got" == "$want" ]] || { echo "the index $index holds $got, not $want" >&2; exit 1; }
      while read -r _ digest; do cosign verify "${cosign_id[@]}" "${index%@*}@$digest" >/dev/null; done <<<"$want"
      cosign verify "${cosign_id[@]}" "$index" >/dev/null
      gh attestation verify "oci://$index" "${gh_id[@]}"
      # The chart installs this index by default.
      key="$(chart_key "$name")"
      pinned="$(KEY="$key" yq '.[strenv(KEY)].repository + "@" + .[strenv(KEY)].digest' <<<"$values")"
      [[ "$pinned" == "$index" ]] || { echo "the chart's $key is $pinned, not $index" >&2; exit 1; }
    done
    chart="$(recorded "$directory/digests.txt" chart)"
    chart_is "$chart" "$directory/$chart_file"
    cosign verify "${cosign_id[@]}" "$chart" >/dev/null
    gh attestation verify "oci://$chart" "${gh_id[@]}"
    helm template builder "$directory/$chart_file" --set registry.host=registry.example:5000 \
      --set registry.tokenKey.namespace=zot --set registry.tokenKey.secret=zot-token-signer >/dev/null
    echo "release $version verified"
    ;;
esac
