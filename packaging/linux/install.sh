#!/bin/sh
# Installs flockswitch for you: the program in ~/.local/bin and an entry in the app menu,
# then `flockswitch setup`: the udev rule that lets you reach Logitech devices and what
# this desktop still needs (Korean font, tray icon extension, desktop portal), together
# with one password; then starts flockswitch. `./install.sh --yes` installs without asking.
# flockswitch를 현재 사용자에게 설치합니다: ~/.local/bin의 프로그램과 앱 메뉴 항목, 이어서
# `flockswitch setup`으로 Logitech 장치 접근용 udev 규칙과 이 데스크톱에 더 필요한 것
# (한글 글꼴, 트레이 아이콘 확장, 데스크톱 포털)을 비밀번호 한 번으로 함께 설치한 뒤
# 실행합니다. `./install.sh --yes`는 묻지 않고 설치합니다.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
bin="$HOME/.local/bin"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
rule=/etc/udev/rules.d/70-flockswitch.rules

yes=
for arg in "$@"; do
    case $arg in
        -y | --yes) yes=--yes ;;
        *)
            echo "usage: ./install.sh [--yes]" >&2
            exit 2
            ;;
    esac
done

# A running copy hands the buttons back and quits before it is replaced.
if [ -x "$bin/flockswitch" ]; then
    "$bin/flockswitch" quit >/dev/null 2>&1 || true
fi

mkdir -p "$bin" "$data/applications" "$data/flockswitch"
install -m 755 "$here/flockswitch" "$bin/flockswitch"
install -m 644 "$here/flockswitch.png" "$data/flockswitch/flockswitch.png"
sed -e "s|@BIN@|$bin/flockswitch|" -e "s|@ICON@|$data/flockswitch/flockswitch.png|" \
    "$here/flockswitch.desktop" > "$data/applications/flockswitch.desktop"
update-desktop-database "$data/applications" >/dev/null 2>&1 || true

echo "Installed: $bin/flockswitch"
echo "설치했습니다: $bin/flockswitch"
echo

# The udev rule and what this desktop still needs: one list, one question, one password.
# 0: all done; 1: not approved or left for the setup check in the settings window.
# Anything else (a step failed, no administrator rights, or it could not run): the
# udev rule the way this script always installed it.
status=0
"$bin/flockswitch" setup ${yes:+"$yes"} || status=$?
case $status in
    0 | 1) ;;
    *)
        if ! cmp -s "$here/70-flockswitch.rules" "$rule" 2>/dev/null; then
            echo
            echo "Installing the udev rule (sudo asks for your password once)."
            echo "udev 규칙을 설치합니다 (sudo가 비밀번호를 한 번 묻습니다)."
            sudo install -m 644 "$here/70-flockswitch.rules" "$rule"
            sudo udevadm control --reload-rules
            sudo udevadm trigger --subsystem-match=hidraw
            echo "If a receiver was plugged in, replug it once (or reconnect Bluetooth)."
            echo "수신기가 꽂혀 있었다면 한 번 뺐다 꽂아 주세요 (블루투스는 다시 연결)."
        fi
        ;;
esac

case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "Note: $bin is not on your PATH; the app menu entry works anyway." ;;
esac

# Start it now, detached from this terminal.
nohup "$bin/flockswitch" >/dev/null 2>&1 &
