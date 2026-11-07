# 변경 기록

## 0.1.2 — 2026-11-06

- 업데이트: 받은 파일을 릴리스에 함께 올린 SHA-256 체크섬(SHA256SUMS.txt)과 비교하고, 맞지 않으면 지웁니다. 체크섬이 없는 릴리스는 대신 릴리스 페이지를 엽니다
- 업데이트: 새 버전 확인과 다운로드가 https로만 오갑니다(리다이렉트 포함)

## 0.1.1 — 2026-11-06

- Homebrew로 설치할 수 있습니다: `brew install wwhalefe/tap/flockswitch`. udev 규칙과 앱 메뉴 항목을 추가하는 방법은 `brew info flockswitch`에 나옵니다
- 로그인 시 시작과 Wayland 단축키 명령이 업그레이드 때 지워지는 버전 폴더 대신 Homebrew의 `bin/flockswitch` 링크를 가리켜, `brew upgrade` 뒤에도 계속 동작합니다
- 장치 권한이 없다는 안내에 Homebrew로 설치했을 때의 방법도 적었습니다

## 0.1.0 — 2026-11-02

- 첫 리눅스 버전입니다. macOS용 flockswitch 0.4.3을 바탕으로 전환, 장치, 설정 창, 이동할 채널(이 PC의 채널은 고를 수 없음)이 같습니다. 아직 실제 리눅스 PC에서 시험하지 않았습니다
- 수신기와 블루투스 장치에 `/dev/hidraw`로 접근합니다. `install.sh`가 udev 규칙을 설치해 root 없이 장치를 열 수 있게 하고, 권한이 없으면 설정 창에 알려 줍니다
- 트레이 아이콘은 StatusNotifierItem(D-Bus)으로 띄워 GTK나 libappindicator가 필요 없습니다. 트레이가 없으면 flockswitch를 한 번 더 실행할 때 설정 창이 열립니다
- 키보드 단축키는 X11에서 동작합니다. 앱이 전역 단축키를 잡을 수 없는 Wayland에서는 단축키 탭에 시스템 키보드 설정에 지정할 `flockswitch switch 1/2/3` 명령을 보여 주고, 실행 중인 flockswitch가 바로 전환합니다
- x86_64와 arm64(glibc 2.35 이상)용 릴리스 파일과 `install.sh` / `uninstall.sh`를 제공합니다
