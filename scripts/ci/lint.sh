#!/usr/bin/env bash
# The short checks: each needs about a minute to set up a runner and its dev shell but much less
# to run. `lint.sh` runs both groups one after another; CI runs them side by side. The cargo group
# stays in one process because its steps share the target/ lock.
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/lib.sh

static() {
  cargo fmt --all --check
  nixfmt --check flake.nix
  shellcheck scripts/*.sh scripts/ci/*.sh
  jactionlint
  bun install --frozen-lockfile
  node_modules/.bin/zeabur-oss-levels
  ./scripts/check-chart.sh
}

cargo_checks() {
  cargo lint
  RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items --locked
  cargo deny --locked check
  cargo clippy -p zeabur-builder-api --no-default-features --all-targets --locked -- -D warnings
  # A crate that only reads and writes the objects turns the schema feature off to drop these.
  if cargo tree --locked -e normal --no-default-features -p zeabur-builder-api --prefix none |
    grep -E '^(schemars|dyn-clone|ref-cast) '; then
    echo "zeabur-builder-api without its schema feature still depends on the crates above" >&2
    exit 1
  fi
}

case "${1:-all}" in
  static) static ;;
  cargo) cargo_checks ;;
  all) static; cargo_checks ;;
  *) echo "usage: lint.sh [static|cargo]" >&2; exit 2 ;;
esac
