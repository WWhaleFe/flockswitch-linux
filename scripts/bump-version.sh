#!/bin/sh
# Sets the app version in Cargo.toml and Cargo.lock together, so CI (cargo --locked) never
# finds a stale lock file. Usage: scripts/bump-version.sh 2.1.2
set -eu
new="${1:?usage: scripts/bump-version.sh <version>}"
cd "$(dirname "$0")/.."
sed -i.bak "0,/^version = \".*\"/s//version = \"$new\"/" Cargo.toml 2>/dev/null || true
# BSD sed (macOS) has no 0,/re/ address: fall back to replacing the first match only.
if ! grep -q "^version = \"$new\"" Cargo.toml; then
  awk -v v="$new" '!done && /^version = "/ { print "version = \"" v "\""; done = 1; next } { print }' Cargo.toml.bak > Cargo.toml
fi
rm -f Cargo.toml.bak
cargo update -p flockswitch --offline -q 2>/dev/null || cargo update -p flockswitch -q
scripts/check-lock.sh
echo "version $new set in Cargo.toml and Cargo.lock"
