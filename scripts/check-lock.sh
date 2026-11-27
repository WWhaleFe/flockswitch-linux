#!/bin/sh
# Fails when Cargo.lock does not match Cargo.toml (what `cargo --locked` in CI would reject).
set -eu
cd "$(dirname "$0")/.."
# Git hooks run with a minimal PATH: add the usual cargo locations.
PATH="$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
if ! command -v cargo >/dev/null 2>&1; then
  echo "check-lock: cargo not found, skipping the Cargo.lock check" >&2
  exit 0
fi
if ! cargo metadata --locked --offline --format-version 1 >/dev/null 2>&1 \
   && ! cargo metadata --locked --format-version 1 >/dev/null 2>&1; then
  echo "Cargo.lock is out of date: run scripts/bump-version.sh <version> or cargo update -p flockswitch" >&2
  exit 1
fi
