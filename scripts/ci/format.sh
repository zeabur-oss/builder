#!/usr/bin/env bash
# shellcheck source-path=SCRIPTDIR
source "$(dirname "$0")/lib.sh"

cargo fmt --all --check
nixfmt --check flake.nix
shellcheck scripts/*.sh scripts/ci/*.sh
actionlint
bun install --frozen-lockfile
bun fmt --check scripts/release.ts
bun check scripts/
