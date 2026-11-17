# shellcheck shell=bash
# Sourced by the scripts: every `cargo` goes through kache.
cargo() {
  kache cargo -- "$@"
}
