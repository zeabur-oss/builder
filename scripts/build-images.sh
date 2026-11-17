#!/usr/bin/env bash
# Builds the controller, Job and Railpack planner images for this machine's architecture:
#   scripts/build-images.sh <prefix> <tag>      # <prefix>/builder:<tag>, builder-job, builder-railpack
# ZEABUR_BUILDER_PROFILE names the Cargo profile of the binaries; the default is `release`.
# On Linux, run inside `nix develop`. On macOS the flake cannot cross-build musl binaries, so the
# Rust part runs in a Linux container with Nix (Docker), and only `docker build` runs here.
set -euo pipefail

prefix="${1:?usage: build-images.sh <prefix> <tag>}"
tag="${2:?usage: build-images.sh <prefix> <tag>}"
root="$(cd "$(dirname "$0")/.." && pwd)"
arch="$(uname -m | sed -e 's/^arm64$/aarch64/')"
target="$arch-unknown-linux-musl"
profile="${ZEABUR_BUILDER_PROFILE:-release}"
# Cargo writes the dev profile to target/<target>/debug.
dir="$profile"
[[ $profile == dev ]] && dir=debug
out="$root/target/image"
# Image binaries are always compiled, never taken from a compiler cache (CONTRIBUTING.md).
# cargo-auditable records the dependency tree in each binary for scanners.
unset RUSTC_WRAPPER

if [[ $(uname -s) == Linux ]]; then
  cargo auditable build --profile "$profile" --locked --target "$target" -p zeabur-builder -p zeabur-builder-job \
    --manifest-path "$root/Cargo.toml"
  bin="$root/target/$target/$dir"
else
  # Named volumes keep the Nix store, the Cargo registry and the build cache between runs.
  docker run --rm --platform "linux/$(uname -m | sed -e 's/^x86_64$/amd64/')" \
    -v "$root:/src" -v zeabur-builder-nix:/nix -v zeabur-builder-cargo:/root/.cargo \
    -v zeabur-builder-target:/target -e CARGO_TARGET_DIR=/target -e ZEABUR_BUILDER_PROFILE -w /src \
    docker.io/nixos/nix:2.34.9 \
    nix --extra-experimental-features 'nix-command flakes' develop --command bash -c "
      set -euo pipefail
      unset RUSTC_WRAPPER
      cargo auditable build --profile $profile --locked --target $target -p zeabur-builder -p zeabur-builder-job
      mkdir -p /src/target/linux
      cp /target/$target/$dir/zeabur-builder /target/$target/$dir/zeabur-builder-job /src/target/linux/"
  bin="$root/target/linux"
fi

rm -rf "$out"
mkdir -p "$out/controller" "$out/job"
cp "$bin/zeabur-builder" "$out/controller/"
cp "$bin/zeabur-builder-job" "$out/job/"
# Staged with explicit modes: a 0444 /etc could not be traversed by the non-root user.
install -d -m 0755 "$out/controller/etc" "$out/controller/etc/ssl" "$out/controller/etc/ssl/certs"
install -m 0444 "${ZEABUR_BUILDER_CA_BUNDLE:?run inside nix develop}" \
  "$out/controller/etc/ssl/certs/ca-certificates.crt"

docker build -q -f "$root/images/controller/Containerfile" -t "$prefix/builder:$tag" "$out/controller"
docker build -q -f "$root/images/job/Containerfile" -t "$prefix/builder-job:$tag" "$out/job"
docker build -q -f "$root/images/railpack/Containerfile" -t "$prefix/builder-railpack:$tag" "$root/images/railpack"
