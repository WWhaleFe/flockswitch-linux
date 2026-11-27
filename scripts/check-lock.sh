#!/bin/sh
# Fails when Cargo.lock does not match Cargo.toml (what `cargo --locked` in CI would reject).
set -eu
cd "$(dirname "$0")/.."
if ! cargo metadata --locked --offline --format-version 1 >/dev/null 2>&1 \
   && ! cargo metadata --locked --format-version 1 >/dev/null 2>&1; then
  echo "Cargo.lock is out of date: run scripts/bump-version.sh <version> or cargo update -p flockswitch" >&2
  exit 1
fi
