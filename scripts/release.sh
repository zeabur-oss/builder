#!/usr/bin/env bash
# One stage of a release, as the shared release workflow of zeabur-oss/.github runs it (from
# .github/workflows/release.yml). Each stage enters the Nix dev shell of the checkout in the
# current directory: with verify-only, the workflow runs a newer commit's scripts in the shell of
# the tag being verified.
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

# The section of CHANGELOG.md for this version, without its heading.
changelog_section() {
  awk -v heading="## [$plain]" '
    index($0, heading) == 1 { found = 1; next }
    found && /^## / { exit }
    found { print }
    END { if (!found) exit 1 }
  ' "$root/CHANGELOG.md"
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
    changelog_section >/dev/null || { echo "CHANGELOG.md has no section ## [$plain]" >&2; status=1; }
    exit $status
    ;;

  build)
    os="$3" arch="$4" dist="$5"
    [[ "$os" == linux ]] || { echo "the images are Linux only, not $os" >&2; exit 2; }
    "$scripts/release-build.sh" "$version" "$arch" "$dist"
    # The images as built, with the chart of this commit, on k3s.
    ZEABUR_CI_SHELL=1 ZEABUR_CI_IMAGE_TAG="release-$arch" "$scripts/ci.sh" kubernetes
    ;;

  publish)
    "$scripts/release-publish.sh" "$version" "$3" "$4"
    ;;

  assets)
    published="$3/published"
    mkdir -p "$4/assets"
    out="$(cd "$4" && pwd)"
    assets="$out/assets"
    chart_file="zeabur-builder-$plain.tgz"
    cp "$published/$chart_file" "$published/digests.txt" "$assets/"
    chart=$(awk '$1 == "chart" { print $2 }' "$assets/digests.txt")
    [[ "$chart" =~ ^[^[:space:]@]+@sha256:[0-9a-f]{64}$ ]] ||
      { echo "digests.txt records no single chart digest" >&2; exit 1; }
    (
      cd "$assets"
      sha256sum -- "$chart_file" digests.txt >SHA256SUMS
      # Keyless, with the workflow's identity token: the same identity as the images' signatures.
      cosign sign-blob --yes --bundle SHA256SUMS.sigstore.json SHA256SUMS
    )
    repository="${GITHUB_REPOSITORY:-zeabur-oss/builder}"
    {
      changelog_section
      cat <<EOF

Source commit: ${GITHUB_SHA:-$(git -C "$root" rev-parse HEAD)}

## Install

\`\`\`sh
helm install builder oci://${chart%@*} --version $plain \\
  --namespace builder-system --set registry.host=<registry>
\`\`\`

The chart installs the images below by digest. Verify them first; the signatures name the shared
release workflow of zeabur-oss/.github, run for $repository at $version:

\`\`\`sh
cosign verify $chart \\
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \\
  --certificate-identity-regexp '^https://github\\.com/zeabur-oss/\\.github/\\.github/workflows/release\\.yml@' \\
  --certificate-github-workflow-repository $repository \\
  --certificate-github-workflow-ref refs/tags/$version
gh attestation verify oci://$chart --repo $repository \\
  --signer-workflow zeabur-oss/.github/.github/workflows/release.yml
\`\`\`

## Digests

\`\`\`text
$(cat "$assets/digests.txt")
\`\`\`
EOF
    } >"$out/notes.md"
    ls -l "$assets"
    ;;

  verify)
    os="$3" arch="$4"
    [[ "$os" == linux ]] || { echo "the release is Linux only, not $os" >&2; exit 2; }
    echo "verifying $version on $os/$arch"
    "$scripts/release-verify.sh" "$version" "$5"
    ;;
esac
