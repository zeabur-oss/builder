#!/usr/bin/env bash
# Builds one architecture's release images and publishes nothing. `scripts/release.sh build` runs
# it on a runner of each architecture, then smokes the images (`ZEABUR_CI_IMAGE_TAG=release-<arch>
# scripts/ci.sh kubernetes`); release-publish.sh publishes them. Linux only, native architecture,
# inside `nix develop`, from a git checkout. Docker builds the images.
#   scripts/release-build.sh v0.1.1 arm64 dist
#
# <out> receives builder.tar, builder-job.tar and builder-railpack.tar (`docker image save` of
# zeabur-builder/<name>:release-<arch>).
set -euo pipefail

if [[ $# != 3 ]]; then
  echo "usage: release-build.sh <vVERSION> <amd64|arm64> <output-dir>" >&2
  exit 2
fi
version="$1"
arch="$2"
root="$(cd "$(dirname "$0")/.." && pwd)"
: "${IN_NIX_SHELL:?run inside nix develop}"
if [[ ! "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "expected a version like v0.1.1" >&2
  exit 2
fi
case "$(uname -s):$arch:$(uname -m)" in
  Linux:amd64:x86_64 | Linux:arm64:aarch64) ;;
  *) echo "cannot build $arch here: each build is native and on Linux" >&2; exit 2 ;;
esac
# mkShell sets SOURCE_DATE_EPOCH to 1980; the commit's time replaces it.
SOURCE_DATE_EPOCH="$(git -C "$root" show -s --format=%ct HEAD)"
export SOURCE_DATE_EPOCH
mkdir -p "$3"
output="$(cd "$3" && pwd)"

"$root/scripts/build-images.sh" zeabur-builder "release-$arch"
for name in builder builder-job builder-railpack; do
  docker image save --output "$output/$name.tar" "zeabur-builder/$name:release-$arch"
done
echo "built zeabur-builder/*:release-$arch for $version (not published)"
ls -l "$output"
