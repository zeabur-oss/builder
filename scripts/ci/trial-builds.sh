#!/usr/bin/env bash
# The agent's trial builds on a real buildkitd: an image from another registry, a download and
# the host network are refused, and a `# syntax` line is ignored (the ignored tests of
# zeabur-builder-agent). It starts a rootful buildkitd with sudo, using the machine's runc, and
# stops it at the end; set ZB_TEST_BUILDKIT to use a running one instead.
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/lib.sh

if [[ -z "${ZB_TEST_BUILDKIT:-}" ]]; then
  state="$(mktemp -d)"
  # The log belongs to this user; only buildkitd runs as root.
  # shellcheck disable=SC2024
  sudo env PATH="$PATH" buildkitd --root "$state/root" --addr tcp://127.0.0.1:1234 \
    >"$state/buildkitd.log" 2>&1 &
  pid=$!
  trap 'sudo kill "$pid" 2>/dev/null || true' EXIT
  export ZB_TEST_BUILDKIT=tcp://127.0.0.1:1234
  for _ in $(seq 60); do
    buildctl --addr "$ZB_TEST_BUILDKIT" debug workers >/dev/null 2>&1 && break
    sleep 1
  done
  buildctl --addr "$ZB_TEST_BUILDKIT" debug workers >/dev/null || {
    cat "$state/buildkitd.log"
    exit 1
  }
fi

cargo nextest run -p zeabur-builder-agent --locked --run-ignored only
