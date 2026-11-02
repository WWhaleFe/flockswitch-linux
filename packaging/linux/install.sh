#!/bin/sh
# Installs flockswitch for you: the program in ~/.local/bin, an entry in the app menu,
# and (once, with sudo) the udev rule that lets you reach Logitech devices.
# flockswitch를 현재 사용자에게 설치합니다: ~/.local/bin의 프로그램, 앱 메뉴 항목,
# 그리고 (한 번, sudo로) Logitech 장치에 접근하게 해 주는 udev 규칙.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
bin="$HOME/.local/bin"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
rule=/etc/udev/rules.d/70-flockswitch.rules

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

if ! cmp -s "$here/70-flockswitch.rules" "$rule" 2>/dev/null; then
    echo "Installing the udev rule (sudo asks for your password once)."
    echo "udev 규칙을 설치합니다 (sudo가 비밀번호를 한 번 묻습니다)."
    sudo install -m 644 "$here/70-flockswitch.rules" "$rule"
    sudo udevadm control --reload-rules
    sudo udevadm trigger --subsystem-match=hidraw
fi

echo
echo "Installed: $bin/flockswitch"
echo "설치했습니다: $bin/flockswitch"
case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "Note: $bin is not on your PATH; the app menu entry works anyway." ;;
esac
echo "If a receiver was plugged in, replug it once (or reconnect Bluetooth)."
echo "수신기가 꽂혀 있었다면 한 번 뺐다 꽂아 주세요 (블루투스는 다시 연결)."

# Start it now, detached from this terminal.
nohup "$bin/flockswitch" >/dev/null 2>&1 &
