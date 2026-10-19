#!/usr/bin/env bash
# Runs every check CI runs, then (in a Wayland session) the in-app
# self-test against throwaway data. Exits non-zero on the first failure.
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n==> %s\n' "$*"; }

step "Formatting (cargo fmt --check)"
cargo fmt --check

step "Lints (cargo clippy, warnings are errors)"
cargo clippy --all-targets --locked -- -D warnings

step "Unit tests"
cargo test --locked

step "Release build"
cargo build --release --locked

if [[ -n "${WAYLAND_DISPLAY:-}" ]]; then
    step "In-app self-test (real GTK widgets, isolated data)"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    mkdir -p "$tmp/data" "$tmp/config"
    cargo build --locked
    XDG_DATA_HOME="$tmp/data" XDG_CONFIG_HOME="$tmp/config" \
        timeout 120 ./target/debug/shoal --self-test
else
    step "Skipping in-app self-test (no Wayland session)"
fi

printf '\nAll checks passed.\n'
