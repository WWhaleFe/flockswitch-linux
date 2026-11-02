#!/bin/sh
# Removes flockswitch: gives the buttons back, removes start-at-login and settings,
# then the program, the app menu entry and (with sudo) the udev rule.
# flockswitch를 제거합니다: 버튼을 원래대로 돌려놓고 자동 실행과 설정을 지운 뒤,
# 프로그램, 앱 메뉴 항목, (sudo로) udev 규칙을 지웁니다.
set -eu
bin="$HOME/.local/bin"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
rule=/etc/udev/rules.d/70-flockswitch.rules

if [ -x "$bin/flockswitch" ]; then
    "$bin/flockswitch" uninstall || true
fi
rm -f "$bin/flockswitch" "$data/applications/flockswitch.desktop"
rm -rf "$data/flockswitch"
update-desktop-database "$data/applications" >/dev/null 2>&1 || true

if [ -e "$rule" ]; then
    echo "Removing the udev rule (sudo asks for your password)."
    echo "udev 규칙을 지웁니다 (sudo가 비밀번호를 묻습니다)."
    sudo rm -f "$rule"
    sudo udevadm control --reload-rules
fi
echo "flockswitch removed. / flockswitch를 제거했습니다."
