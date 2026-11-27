#!/bin/sh
# Cross-checks this tree for Linux from macOS: `cargo check --target x86_64-unknown-linux-gnu`
# in a scratch copy (target/v2-check-src) that uses hidapi's linux-native backend, with a
# pkg-config stub that answers for libudev. Arguments go to cargo check, e.g.
#   sh scripts/check-linux.sh --all-targets --offline
# Everything it needs is created here under target/, so a cleaned target/ is fine.
set -eu
cd "$(dirname "$0")/.."
S=target/v2-check-src
PKG=target/v2-check-pkg
mkdir -p "$S" "$PKG"
rsync -a --delete --exclude target --exclude .git --exclude 'IMG_*' --exclude '*.png' src netcore packaging build.rs Cargo.toml Cargo.lock "$S/"
mkdir -p "$S/packaging/icon"
cp packaging/icon/flockswitch.png "$S/packaging/icon/" 2>/dev/null || true
sed -i '' 's#^hidapi = .*#hidapi = { version = "2", default-features = false, features = ["linux-native"] }#' "$S/Cargo.toml"
cat > "$PKG/libudev.pc" <<'PC'
Name: libudev
Description: check-only stub
Version: 1.0
Libs: -ludev
Cflags:
PC
cat > "$PKG/pkg-config" <<'PK'
#!/bin/sh
case "$*" in
  *libudev*)
    case "$*" in
      *--modversion*) echo 1.0 ;;
      *--variable=*) echo /usr ;;
      *--libs*) echo -ludev ;;
    esac
    exit 0 ;;
esac
exec /opt/homebrew/bin/pkg-config "$@"
PK
chmod +x "$PKG/pkg-config"
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/opt/homebrew/bin:$PATH"
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_PATH="$PWD/$PKG"
export PATH="$PKG_CONFIG_PATH:$PATH"
export CC_x86_64_unknown_linux_gnu=/opt/homebrew/opt/llvm/bin/clang
export AR_x86_64_unknown_linux_gnu=/opt/homebrew/opt/llvm/bin/llvm-ar
export CFLAGS_x86_64_unknown_linux_gnu='--target=x86_64-unknown-linux-gnu -ffreestanding -DRING_CORE_NOSTDLIBINC'
export CARGO_TARGET_DIR="$PWD/target/v2-check"
cargo check --manifest-path "$S/Cargo.toml" --target x86_64-unknown-linux-gnu "$@"
