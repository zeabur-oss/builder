# shellcheck shell=bash
# Sourced by the scripts: `cargo` goes through kache when the shell has it, plain Cargo otherwise.
# Only the subcommands that compile use it; fmt and deny run as they are.
cargo() {
  case "${1:-}" in
    build | check | clippy | doc | lint | test | nextest)
      if command -v kache >/dev/null; then
        kache cargo -- "$@"
        return
      fi
      ;;
  esac
  command cargo "$@"
}
