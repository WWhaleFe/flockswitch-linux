# flockswitch (Linux)

[English](README.md) · **한국어**

마우스 버튼 한 번으로 Logitech 마우스와 **키보드를 함께** 다른 컴퓨터로 넘깁니다.

flockswitch는 MX Master 마우스의 **휠 모드 전환 버튼, 제스처 버튼, 휠 클릭(휠을 눌러 클릭)** 중 고른 버튼을 Easy-Switch 버튼으로 바꿉니다. 누르면 그룹으로 묶은 장치(키보드, 다른 마우스)가 먼저, 마우스가 마지막으로 **같은 Easy-Switch 채널**로 이동합니다. 컴퓨터끼리 네트워크 연결이 필요 없어서 Logitech Flow를 쓸 수 없는 곳(공용 네트워크, 보안이 엄격한 네트워크)에서도 동작합니다.

이 저장소는 리눅스 버전입니다. 다른 버전은 [flockswitch-mac](https://github.com/WWhaleFe/flockswitch-mac), [flockswitch-windows](https://github.com/WWhaleFe/flockswitch-windows), [flockswitch-android](https://github.com/WWhaleFe/flockswitch-android)에 있으며, 모두 같은 방식으로 전환되어 섞어 쓸 수 있습니다.

> **아직 실제 리눅스 PC에서 시험하지 않았습니다.** 장치와 통신하는 코드는 시험을 마친 macOS·Windows 버전과 같지만, 리눅스 기기에서는 아직 실행해 보지 않았습니다. 써 보신 결과를 알려 주시면 큰 도움이 됩니다.

> Logitech과 관계없는 개인 오픈소스입니다. "Logitech", "MX Master", "Easy-Switch", "Bolt", "Unifying"은 Logitech의 상표입니다.

## 지원 마우스

flockswitch는 **MX Master 3**와 **MX Master 4**를 바탕으로 만들고 시험했습니다. 같은 버튼과 기능(HID++ 2.0 *Change Host*, 버튼 재지정)을 가진 다른 Logitech 마우스도 동작할 수 있지만 **아직 시험하지 않았습니다**.

## 준비

1. 키보드와 마우스를 각 컴퓨터에 **같은 채널 번호**로 페어링해 둡니다. 예: 채널 1 = 윈도우 PC, 채널 2 = 리눅스 PC.
2. 오가는 **모든 컴퓨터에 flockswitch를 설치**합니다(맥은 flockswitch-mac, Windows PC는 flockswitch-windows).

## 필요한 환경

- x86_64 또는 arm64, glibc 2.35 이상 (Ubuntu 22.04, Debian 12, Fedora 36 이상)
- systemd-logind (대부분의 데스크톱 배포판). 장치를 열게 해 주는 udev 규칙에 필요합니다
- **트레이 아이콘**: StatusNotifierItem 아이콘을 보여 주는 패널. KDE Plasma, Ubuntu의 GNOME, Cinnamon, XFCE 등 대부분이 지원합니다. 기본 GNOME(예: Fedora)은 *AppIndicator and KStatusNotifierItem Support* 확장이 필요합니다. 트레이가 없으면 앱 메뉴에서 flockswitch를 한 번 더 실행하면 설정 창이 열립니다.
- 창의 **한글 표시**: 한글 글꼴. 예: `fonts-noto-cjk`(Debian/Ubuntu), `google-noto-sans-cjk-fonts`(Fedora)
- **Wayland에서 화면 모서리 전환**: KDE Plasma·sway·Hyprland는 따로 필요한 것이 없습니다. GNOME은 45 이상과 입력 캡처를 지원하는 데스크톱 포털(`xdg-desktop-portal` 1.17 이상, `xdg-desktop-portal-gnome` 45 이상, 보통 이미 설치됨)이 필요합니다. 확인: `busctl --user get-property org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.portal.InputCapture version`에 숫자가 나오면 됩니다. GNOME 44 이하는 X11(Xorg) 세션에서 쓰세요
- 설치 명령을 단계별로 정리한 안내는 [릴리스](../../releases/latest) 페이지의 "설치 전에 준비할 것"에 있습니다

## 설치

[Homebrew](https://brew.sh)를 쓰면:

```sh
brew install wwhalefe/tap/flockswitch

# 최초 1회: udev 규칙 설치(sudo) 및 앱 메뉴 추가
sudo install -m 644 "$(brew --prefix)/opt/flockswitch/share/flockswitch/70-flockswitch.rules" /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=hidraw
mkdir -p ~/.local/share/applications
ln -sf "$(brew --prefix)/opt/flockswitch/share/applications/flockswitch.desktop" ~/.local/share/applications/
```

업데이트는 `brew update && brew upgrade flockswitch`입니다 (Homebrew로 설치한 flockswitch는 스스로 업데이트를 설치하지 않습니다).

또는 [Releases](../../releases)에서 `flockswitch-linux-<x86_64|aarch64>-<버전>.tar.gz`를 받은 뒤:

```sh
tar xzf flockswitch-linux-x86_64-<버전>.tar.gz
cd flockswitch-linux-x86_64-<버전>
./install.sh
```

`install.sh`는 프로그램을 `~/.local/bin`에 넣고, 앱 메뉴에 flockswitch를 추가하고, udev 규칙 `/etc/udev/rules.d/70-flockswitch.rules`를 설치한 뒤(sudo가 비밀번호를 한 번 묻습니다) flockswitch를 실행합니다. 수신기가 꽂혀 있었다면 규칙이 적용되도록 한 번 뺐다 꽂아 주세요(블루투스는 다시 연결).

업데이트는 저절로 설치됩니다: 하루 한 번 확인할 때 새 버전을 찾으면(또는 **지금 업데이트**를 누르면) 내려받아 릴리스의 SHA256SUMS.txt와 비교한 뒤, `~/.local/bin`의 프로그램과 앱 메뉴 항목·아이콘을 바꾸고 flockswitch를 다시 시작합니다. 설정 → 일반에서 끌 수 있습니다(`flockswitch set auto_update false`). 꺼 두어도 **지금 확인** 뒤 **지금 업데이트**를 누르면 언제든 설치합니다. sudo는 묻지 않으며, 새 릴리스에서 udev 규칙이 바뀌었으면 로그에 `install.sh`를 한 번 실행하라고 남깁니다. 쓸 수 없는 폴더(예: `/usr/bin`)에 있는 프로그램은 내려받기만 합니다.

## 사용

트레이 아이콘 메뉴에서 상태 확인, 켜기/끄기, **설정…**, 개발자에게 리포트, 업데이트 확인, 종료를 할 수 있습니다. **설정…**을 누르면 설정 창이 열리고, 바꾸는 즉시 적용됩니다. 설정 창은 일반 탭 전체가 보이는 높이로 열리며, 내용이 더 길어지면 화면이 허락하는 만큼 저절로 커집니다.

| 탭 | 항목 | 내용 |
|---|---|---|
| 전환 | 마우스 버튼으로 전환 | 켜기/끄기 (끄면 버튼이 원래대로 동작) |
| 전환 | 버튼별 설정 | 휠 모드 전환·제스처·휠 클릭을 각각 켜기/끄기 (여러 개 가능) |
| 전환 | 이동할 채널 (버튼별) | 누르면 고른 채널 하나로 이동. 설정은 컴퓨터마다 따로라 각 컴퓨터에서 고르며, 이 컴퓨터의 채널은 고를 수 없음 |
| 전환 | 제스처 버튼 | 누르기와 누른 채 ↑↓←→ 움직이기, 다섯 가지에 각각 채널 지정 |
| 전환 | 단축키 | X11: 채널마다 키보드 단축키 하나(기본 Ctrl+Alt+1 / 2 / 3). 제목(과 마우스 찾기 단축키) 옆의 ⓘ에 쓸 수 있는 조합을 정리: Ctrl·Alt 중 하나 이상과 숫자·영문자(Shift 추가 가능), 또는 F키. 데스크톱이 쓰는 조합과 다른 앱이 잡아 둔 조합은 쓸 수 없고 그 줄에 알려 줌. **Wayland**: 앱이 전역 단축키를 등록할 수 없어서, 시스템 설정의 사용자 단축키에 `~/.local/bin/flockswitch switch 1`(2, 3) 명령을 지정하도록 안내. 실행 중인 flockswitch가 바로 전환 |
| 전환 | 모서리 | 커서를 화면 바깥쪽 가장자리에 대고 잠시(0.3~2초, 선택) 머물면 그 방향에 정한 채널로 그룹 장치와 마스터 마우스가 이동. 네트워크 없이 각 컴퓨터가 자기 화면만 보므로 컴퓨터마다 켜고 각 방향에 그쪽에 있는 컴퓨터의 채널을 지정. 기다리는 동안 커서 근처에 효과(링·반원·막대·펄스·레이저·보텍스·스캔·리퀴드·랜덤 중 선택)가 줄어들며, 효과마다 색상·크기를, 링·반원·막대는 줄어드는 방향도 따로 기억함(링·반원은 작아지며 사라지기도 선택). **랜덤**은 매번 효과 하나를 그 효과의 색상·크기로 보여 주고 방향과 작아지며 사라지기도 무작위로 정함. Ctrl을 누른 동안에만 전환하는 옵션도 있음. 마우스가 돌아오면 커서가 보냈던 쪽 가장자리에 나타남(X11). 네 귀퉁이는 제외되고, 모니터가 여러 대면 다른 모니터로 이어지는 가장자리는 전환하지 않음. Wayland에서도 동작하며 방식은 아래 [Wayland에서 화면 모서리 전환](#wayland에서-화면-모서리-전환) 참고 |
| 장치 | 마스터 마우스 | 버튼을 쓸 마우스 |
| 장치 | 함께 이동할 장치 | Bolt·Unifying·블루투스 장치를 섞어서 선택. 다른 컴퓨터에 가 있는 장치도 표시 |
| 일반 | 진동 (진동 모터가 있는 마우스, 현재 MX Master 4) | 요약과 **진동 설정…** 창: 켜기/끄기, 세기, 떠날 때·돌아왔을 때 진동(종류 또는 랜덤), 모서리에서 점점 강하게/일정하게, **진동 확인** |
| 일반 | 마우스 찾기 | 단축키, 다른 컴퓨터에서 도착, 마우스 흔들기로 커서 주변에 펄스 링·레이더·스포트라이트·랜덤 효과를 고른 색과 크기로 표시 (X11 세션과 컴포지터 필요). 흔들기는 0.5~2초 동안 쉬지 않고 흔들어야 하며, Ctrl·Alt·Shift를 누른 동안에만 반응하게 할 수 있음 |
| 일반 | 설정 백업 | 저장(문서/flockswitch 폴더에 파일로, 10개까지), 불러오기(저장한 파일을 최신순으로 고르고 확인한 뒤 모든 설정을 바꿈), 관리(이름 바꾸기·삭제), 폴더 열기 |
| 일반 | 초기화 | 언어를 뺀 모든 설정을 처음 상태로 되돌리기 |
| 일반 | 시작하기 전에 | Solaar와 함께 쓸 때의 주의, udev 규칙 안내 |
| 일반 | 업데이트 | 새 버전 확인(자동/지금), 자동 설치 또는 **지금 업데이트** |
| 일반 | 개발자에게 | 메일 앱 열기(버전·장치 정보 자동 기입), 주소 복사, GitHub 신고 |
| 일반 | 언어 | 한국어 / English (처음에는 컴퓨터 언어를 따름) |
| 일반 | 로그인 시 자동 실행 | 켜기/끄기 |
| 일반 | 제거 | 버튼 원래대로, 자동 실행 해제, 설정 삭제 후 종료 |

리눅스에는 Logi Options+가 없어서 버튼을 미리 비워 둘 필요가 없습니다. **Solaar**를 쓴다면 flockswitch에서 켠 버튼에는 Solaar의 버튼 설정이나 규칙을 걸지 마세요. 그 밖에는 두 앱을 함께 설치해 두어도 됩니다. flockswitch를 끄거나 버튼을 끄면 그 버튼은 원래 상태로 돌아갑니다.

명령줄: `flockswitch help` (`list`, `switch <N>`, `set <key> <value>`, `run`, `quit`)

설정 위치: `~/.config/flockswitch/`

**설정 백업**(일반 탭): **저장**은 지금 설정을 `~/문서/flockswitch/flockswitch-settings-<컴퓨터 이름>-<날짜-시각>.toml`에 씁니다(문서 폴더는 `$XDG_DOCUMENTS_DIR`, 없으면 `~/Documents`). **불러오기…**는 그 폴더의 파일을 최신순으로 보여 주고, 고른 파일을 확인한 뒤 모든 설정을 그 파일의 것으로 바꿉니다. **관리…**는 파일마다 **이름 바꾸기**와 **삭제**(`gio trash`로 휴지통에, 휴지통이 없으면 완전히 삭제)를 보여 주고, **폴더 열기**는 그 폴더를 엽니다. 폴더에는 10개까지 둘 수 있으며(카드에 3/10처럼 표시), 10개가 되면 저장은 아무것도 쓰지 않고 관리에서 정리하라고 알려 줍니다. 파일은 Windows판·macOS판과 같은 TOML이라 서로 불러올 수 있습니다. 리눅스에 없는 설정은 건너뛰고, 빠진 설정은 기본값으로 두며, 올바른 설정이 아닌 파일은 이유와 함께 거절합니다. 채널은 컴퓨터마다 따로이니 다른 컴퓨터의 설정을 불러왔다면 채널을 확인하세요.

### Wayland에서 화면 모서리 전환

Wayland에서는 앱이 커서 위치를 읽을 수 없어서, flockswitch가 시작할 때 데스크톱이 제공하는 방법을 골라 로그(`~/.config/flockswitch/flockswitch.log`의 `edge backend: …`)와 설정 창의 모서리 페이지에 알려 줍니다.

| 세션 | 가장자리를 감지하는 방법 | 처음 한 번 할 일 |
|---|---|---|
| X11(Xorg), 모든 데스크톱 | 지금처럼 커서 위치를 읽음 | 없음 |
| KDE Plasma, sway, Hyprland 등 wlroots 계열 (Wayland) | 채널을 정한 가장자리를 따라 1픽셀 너비의 투명한 띠(wlr-layer-shell) | 없음 |
| GNOME 45 이상 (Wayland) | 데스크톱 포털의 입력 캡처: 채널을 정한 가장자리마다 감지선을 두고, 커서가 거기 닿으면 전환하거나 안쪽으로 움직일 때까지 시스템이 커서를 flockswitch에 넘김 | 시스템이 입력 캡처를 허락할지 물으면 허용. GNOME 51은 답을 기억하고, GNOME 45~50은 flockswitch를 시작할 때마다 묻습니다(45~48은 채널을 정한 가장자리나 모니터 구성이 바뀔 때도) |
| GNOME 44 이하, 또는 둘 다 없는 컴포지터(예: Weston) | 쓸 수 없음: 모서리 페이지에 이유를 표시 | 로그인 화면에서 X11 세션 선택 |

Wayland에서는 마우스가 돌아와도 커서를 보냈던 쪽 가장자리로 옮기지 못합니다(앱이 커서를 옮길 수 없음). 대기 효과는 KDE·sway·Hyprland에서 그려지고, GNOME에서는 XWayland의 화면 배치가 데스크톱과 같을 때 XWayland로 그립니다(아니면 효과 없이 기다리며, 진동은 어디서나 동작). 띠는 그 가장자리 맨 바깥 한 줄의 클릭을 받습니다. 'Ctrl 키를 누른 동안에만 전환': GNOME에서는 커서가 가장자리에 닿은 뒤에 Ctrl을 누르세요(그 전에 누른 키는 시스템이 알려 주지 않음). layer-shell 데스크톱에서는 커서가 띠 위에 있는 동안 Ctrl을 확인하려고 키보드를 잠시 넘겨받습니다(Ctrl 없이 최대 1.5초, 다른 키를 누르면 바로 돌려줌). 마우스 찾기는 커서 위치가 있어야 해서 X11에서만 동작합니다. 시험용으로 `FLOCKSWITCH_EDGE_BACKEND=layer-shell`, `input-capture`, `none`으로 방법을 정할 수 있습니다.

## 제거

받은 폴더에서 `./uninstall.sh`를 실행합니다. 버튼을 원래대로 돌려놓고, 자동 실행과 설정을 지운 뒤, 프로그램과 앱 메뉴 항목, udev 규칙을 지웁니다.

Homebrew로 설치했다면 `flockswitch uninstall`을 실행한 다음 `brew uninstall flockswitch`를 실행하고, udev 규칙과 메뉴 항목은 `brew info flockswitch`에 나오는 대로 지웁니다.

## 개인정보

flockswitch는 이 PC에 연결된 Logitech 장치와 통신합니다. 인터넷 연결은 GitHub에서 새 버전이 있는지 확인할 때(시작할 때와 하루 한 번, 설정 → 일반에서 끌 수 있음)와 새 버전을 내려받을 때(저절로 또는 버튼을 눌러)뿐이며, 사용자나 장치에 대한 정보는 보내지 않습니다. "개발자에게 리포트"는 사용자의 메일 앱을 열 뿐이라 보내기 전에 내용을 모두 확인할 수 있습니다. (중복 실행을 막으려고 `127.0.0.1`의 포트 하나를 잡아 둘 뿐, 그 포트로 주고받는 데이터는 없습니다.)

## 문제가 생기면

[문제 신고](../../issues/new/choose) 양식에 아래를 붙여 주세요.

- `flockswitch list` 결과
- 배포판, 데스크톱(GNOME, KDE 등)과 세션(X11 / Wayland), 연결 방식(Bolt / Unifying / 블루투스)
- `~/.config/flockswitch/flockswitch.log`

설정 창에 "장치에 접근할 권한이 없습니다"가 보이면 udev 규칙이 없는 것입니다. `./install.sh`를 다시 실행하고(Homebrew로 설치했다면 `brew info flockswitch`에 나오는 udev 명령) 수신기를 뺐다 꽂아 주세요.

## 시험 현황

| 연결 | 상태 |
|---|---|
| Logi Bolt 수신기 | 구현, 리눅스에서 미확인 |
| Unifying 수신기 | 구현, 리눅스에서 미확인 |
| 블루투스 | 구현, 리눅스에서 미확인 |
| Wayland에서 화면 모서리 전환 (GNOME 입력 캡처, KDE·wlroots layer shell) | 구현, 미확인 |

## 빌드

```sh
sudo apt install libudev-dev pkg-config   # Fedora: systemd-devel pkgconf
cargo build --release                      # target/release/flockswitch
```

Wayland, X11, OpenGL, D-Bus, libei는 실행할 때 불러오거나 순수 Rust로 다루므로 다른 개발 패키지는 필요 없습니다.

## 라이선스

MIT
