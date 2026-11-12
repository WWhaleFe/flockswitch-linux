#!/bin/sh
# Installs (or updates) flockswitch for Linux in one go:
#
#   curl -fsSL https://github.com/WWhaleFe/flockswitch-linux/releases/latest/download/get.sh | sh
#
# It downloads the latest release for this computer (x86_64 or aarch64) and the release's
# SHA256SUMS.txt, stops unless the file matches, unpacks it in a temporary folder and runs
# its install.sh: the program in ~/.local/bin, the app menu entry, then `flockswitch
# setup` (the udev rule and what this desktop still needs, with one password), and
# finally starts flockswitch. Run it as yourself, not with sudo.
#
#   ... | sh -s -- --yes      install without asking
#
# For testing: FLOCKSWITCH_ARCH=x86_64|aarch64 picks the file for that machine, and
# FLOCKSWITCH_GET_NO_INSTALL=1 only downloads, checks and unpacks (on any system) and
# leaves the folder there.
#
# Linux용 flockswitch를 한 번에 설치(또는 업데이트)합니다. 이 컴퓨터에 맞는 최신 릴리스와
# SHA256SUMS.txt를 받아 일치할 때만 임시 폴더에 풀고 install.sh를 실행합니다. sudo 없이
# 내 계정으로 실행하세요. `sh -s -- --yes`는 묻지 않고 설치합니다.
set -eu

repo=https://github.com/WWhaleFe/flockswitch-linux

ko=
case "${LC_ALL:-${LC_MESSAGES:-${LANG:-}}}" in
    ko*) ko=1 ;;
esac

# say "English" "한국어"
say() {
    if [ -n "$ko" ]; then
        printf '%s\n' "$2"
    else
        printf '%s\n' "$1"
    fi
}

die() {
    if [ -n "$ko" ]; then
        printf 'flockswitch 설치: %s\n' "$2" >&2
    else
        printf 'flockswitch install: %s\n' "$1" >&2
    fi
    exit 1
}

# fetch <url> <file>
fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --proto '=https' --retry 3 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        die "neither curl nor wget is installed" "curl도 wget도 설치되어 있지 않습니다"
    fi
}

# sha256 <file>: the hex digest, by whichever tool this system has.
sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$1" | sed 's/^.*= *//'
    else
        die "no SHA-256 tool (sha256sum, shasum or openssl) to check the download" \
            "다운로드를 확인할 SHA-256 도구(sha256sum, shasum, openssl)가 없습니다"
    fi
}

main() {
    yes=
    for arg in "$@"; do
        case $arg in
            -y | --yes) yes=--yes ;;
            -h | --help)
                say "usage: curl -fsSL ${repo}/releases/latest/download/get.sh | sh [-s -- --yes]" \
                    "사용법: curl -fsSL ${repo}/releases/latest/download/get.sh | sh [-s -- --yes]"
                return 0
                ;;
            *) die "unknown option '${arg}' (only --yes)" "모르는 옵션 '${arg}' (--yes만 있습니다)" ;;
        esac
    done
    test_only=${FLOCKSWITCH_GET_NO_INSTALL:-}

    if [ "$(id -u)" -eq 0 ]; then
        die "run this as yourself, not as root or with sudo: flockswitch installs for your user and asks for the password when it needs it" \
            "root나 sudo가 아니라 내 계정으로 실행하세요. flockswitch는 내 사용자에게 설치되며 필요할 때 비밀번호를 직접 묻습니다"
    fi
    if [ -z "$test_only" ] && [ "$(uname -s)" != Linux ]; then
        die "this installs the Linux version; see https://github.com/WWhaleFe for the others" \
            "Linux용 설치 스크립트입니다. 다른 버전은 https://github.com/WWhaleFe 에 있습니다"
    fi

    machine=${FLOCKSWITCH_ARCH:-$(uname -m)}
    case $machine in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) die "no release for this processor (${machine}); x86_64 and aarch64 are built" \
            "이 프로세서(${machine})용 릴리스가 없습니다. x86_64와 aarch64만 있습니다" ;;
    esac

    tmp=$(mktemp -d "${TMPDIR:-/tmp}/flockswitch-get.XXXXXX")
    if [ -z "$test_only" ]; then
        trap 'rm -rf "$tmp"' EXIT
        trap 'exit 130' INT TERM
    fi

    # The checksums name this release's files, and so its version.
    fetch "$repo/releases/latest/download/SHA256SUMS.txt" "$tmp/SHA256SUMS.txt" ||
        die "cannot download SHA256SUMS.txt from ${repo}/releases/latest" \
            "${repo}/releases/latest 에서 SHA256SUMS.txt를 받지 못했습니다"
    line=$(awk -v a="$arch" '{ f = $2; sub(/^\*/, "", f) } f ~ ("^flockswitch-linux-" a "-[0-9][0-9A-Za-z.+-]*\\.tar\\.gz$") && $1 ~ /^[0-9a-f]+$/ && length($1) == 64 { print $1, f; exit }' "$tmp/SHA256SUMS.txt")
    want=${line%% *}
    name=${line#* }
    if [ -z "$line" ]; then
        die "the latest release has no file for ${arch}" "최신 릴리스에 ${arch}용 파일이 없습니다"
    fi
    version=${name#flockswitch-linux-"$arch"-}
    version=${version%.tar.gz}

    say "Downloading flockswitch ${version} for ${arch}…" "flockswitch ${version} (${arch}) 받는 중…"
    # From that release itself, so the file and the checksums belong together even if a
    # newer release comes out meanwhile.
    fetch "$repo/releases/download/v$version/$name" "$tmp/$name" ||
        die "cannot download ${name}" "${name} 파일을 받지 못했습니다"
    have=$(sha256 "$tmp/$name")
    if [ "$have" != "$want" ]; then
        die "${name} does not match SHA256SUMS.txt (got ${have}, expected ${want}); nothing was installed" \
            "${name} 파일이 SHA256SUMS.txt와 맞지 않습니다(받은 값 ${have}, 기대한 값 ${want}). 아무것도 설치하지 않았습니다"
    fi
    say "Checked against SHA256SUMS.txt." "SHA256SUMS.txt와 일치합니다."

    tar -xzf "$tmp/$name" -C "$tmp"
    dir="$tmp/${name%.tar.gz}"
    if [ ! -f "$dir/install.sh" ] || [ ! -f "$dir/flockswitch" ]; then
        die "${name} does not hold install.sh and the program" "${name} 안에 install.sh와 프로그램이 없습니다"
    fi
    if [ -n "$test_only" ]; then
        say "Not installed (FLOCKSWITCH_GET_NO_INSTALL): unpacked in ${dir}" \
            "설치하지 않았습니다(FLOCKSWITCH_GET_NO_INSTALL). 푼 위치: ${dir}"
        return 0
    fi
    echo
    # Its questions and sudo's password prompt go to the terminal (/dev/tty), even when
    # this script comes through a pipe.
    (cd "$dir" && sh ./install.sh ${yes:+"$yes"}) </dev/null
}

main "$@"
