# flockswitch (Linux)

[English](README.md) · **한국어**

컴퓨터 사이에 마우스·키보드·클립보드를 공유하거나, Easy-Switch로 Logitech 장치를 함께 넘깁니다.

flockswitch는 MX Master 마우스의 **휠 모드 전환 버튼, 제스처 버튼, 휠 클릭(휠을 눌러 클릭)** 중 고른 버튼을 Easy-Switch 버튼으로 바꿉니다. 누르면 그룹으로 묶은 장치(키보드, 다른 마우스)가 먼저, 마우스가 마지막으로 **같은 Easy-Switch 채널**로 이동합니다. 이 Logitech 전환 방식은 컴퓨터끼리 네트워크 연결이 필요 없어서 Logitech Flow를 쓸 수 없는 곳(공용 네트워크, 보안이 엄격한 네트워크)에서도 동작합니다.

이 저장소는 리눅스 버전입니다. 다른 버전은 [flockswitch-mac](https://github.com/WWhaleFe/flockswitch-mac), [flockswitch-windows](https://github.com/WWhaleFe/flockswitch-windows), [flockswitch-android](https://github.com/WWhaleFe/flockswitch-android)에 있으며, 모두 같은 방식으로 전환되어 섞어 쓸 수 있습니다.

> [!CAUTION]
> **베타 버전입니다.** 2.0.0은 실제 Linux PC(Ubuntu 26.04, GNOME 50, Wayland)에서 Mac과 함께 네트워크 공유까지 확인했습니다. X11(Xorg) 세션과 KDE Plasma·sway·Hyprland에서는 아직 시험하지 않았습니다. 써 보신 결과를 알려 주시면 큰 도움이 됩니다.

> Logitech과 관계없는 개인 오픈소스입니다. "Logitech", "MX Master", "Easy-Switch", "Bolt", "Unifying"은 Logitech의 상표입니다.

## 2.1 네트워크 공유

같은 신뢰할 수 있는 사설 네트워크의 내 컴퓨터에서 브랜드에 관계없이 마우스·키보드와
노트북 내장 입력 장치를 함께 씁니다. 각 컴퓨터에서 **설정 → 연결과 배치**의 주의 사항에
동의하고, **마스터** 한 대와 **서브** 최대 세 대를 정한 뒤 공유를 켜세요. 두 화면의
6자리 숫자가 같을 때만 짝을 맺습니다. 마스터의 5 x 3 **배치 지도**에서 마스터와 서브의
위치를 정하며, 나란히 놓은 서브끼리 서로 바로 넘어갑니다(GNOME Wayland의 InputCapture
장벽도 배치를 따릅니다). 마스터에서는 짝 맺은 컴퓨터마다 그 컴퓨터의 마우스·키보드로
다른 컴퓨터를 쓸 수 있는지도 따로 끄고 켤 수 있습니다. 서브는 마스터의 배치·넘기기·
클립보드·효과 설정을 따르며(서브에서 바꾼 키 규칙은 마스터에 전달됩니다), 모서리 담당·
로지텍 채널과 Linux 설치 설정은 각 컴퓨터에 남습니다.

이웃한 방향의 화면 끝으로 밀면 넘어가고, 들어온 쪽 끝으로 밀면 돌아옵니다.
**Ctrl+Alt+Shift+Q**로 즉시 돌아올 수도 있습니다. 컴퓨터가 보이지 않을 때는 **연결 다시
찾기**를 누르세요. Linux가 절전에서 깨어나거나(`login1` `PrepareForSleep`) 화면 잠금을
풀면(`ScreenSaver` `ActiveChanged`) 자동으로 다시 연결합니다. 네트워크 이동은 Logitech
채널을 바꾸지 않습니다. 기존 버튼·제스처·채널 단축키는 유지되며, 공유할 때는 버튼 전환에서
이지스위치를 끄는 것을 권장하는 안내가 표시됩니다. 공유를 켜면 모서리는 네트워크가
담당합니다. 로지텍 모서리 전환을 다시 켜면 확인 창을 거쳐 네트워크 넘기기를 잠시 멈춥니다.
입력 받기는 계속 동작합니다.

클립보드 공유를 켜면 글·이미지·파일이 커서를 따라가며, 서브끼리도 마스터를 거쳐
주고받습니다. 제한은 **100MB 고정**입니다. 받은 파일은 **다운로드/flockswitch**에
보관하고 파일 관리자에 바로 붙여넣을 수 있습니다(다운로드 폴더를 찾지 못하면 네트워크 설정
옆의 received 폴더를 사용). **키보드 → OS 전환 시**에서 Linux와 Mac·Windows·Android 쌍별·
방향별로 보조키 기준(기능·자리·바꾸지 않음과 대응표), 한/영 전환 키, Caps Lock 길게 누르기,
한자 키, 이동 키, 화면 캡처를 정하고 **운영체제별 키 비교** 창에서 한눈에 확인할 수 있습니다.

| 세션 | 네트워크 보내기 | 네트워크 받기 | 클립보드 | 카운트다운 | 실기기 확인 |
|---|---|---|---|---|---|
| X11(Xorg) | X11 캡처 | XTest | 실행 중 유지하는 X11 선택 영역 | 네트워크 넘김 지연 포함 지원 | 2.0 미확인, X11 실기기 시험 이력 없음 |
| GNOME Wayland | 포인터·키보드 InputCapture | RemoteDesktop ConnectToEIS | data-control이 없어 XWayland 경유 | 포털 구역과 X11 화면 좌표가 맞으면 표시, 아니면 지연만 적용 | 2.0.0을 Ubuntu 26.04 / GNOME 50에서 Mac과 함께 확인 |
| KDE / 기타 Wayland | 불가, 받기 전용 | 컴포지터가 RemoteDesktop ConnectToEIS를 제공할 때 지원 | 제공 시 네이티브 data-control, 아니면 XWayland | 로지텍 모서리는 layer-shell 표시, 네트워크 송신·카운트다운 없음 | 실기기 시험 이력 없음 |

GNOME Wayland는 공유를 시작할 때 **입력 캡처와 원격 데스크톱 두 권한**을 요청합니다.
**GNOME 도우미 확장**(아래)을 설치하면 두 질문이 모두 사라집니다. 한 번 설치하고 한 번
로그아웃했다 다시 로그인하면 GNOME 버전과 관계없이 시스템이 묻지 않으며, 확장은 이름이
`flockswitch`인 프로그램에만 답합니다. 확장이 없으면 여기 적은 대로 포털로 동작합니다.
로지텍과 네트워크 모서리는 하나의 InputCapture 세션을 공유하며, 담당을 바꿀 때 이전
캡처와 장벽을 해제한 뒤 새 장벽을 설정합니다. **네트워크 → 이 컴퓨터**에는 지원 검사
결과가 나오고, 보내기·받기를 사용할 수 없으면 문제 카드가 표시됩니다. Wayland 네이티브
클립보드는 컴포지터 기능에 따라 다릅니다. XWayland 대체 경로에는 XWayland와 DISPLAY가
필요합니다.

mDNS(`_flockswitch._tcp`, UDP 5353)로 서로 찾고 인증된 TLS 1.3(기본 TCP 47160)으로
입력·클립보드를 주고받으며 중계 서버는 없습니다. 찾지 못하면 **IP로 추가**를 사용하세요.
`ufw`를 사용한다면 신뢰하는 로컬 서브넷에서만 TCP 47160과 UDP 5353을 허용하세요.
[개인정보 안내](PRIVACY.md)를 참고하세요. 기존 0.15.0 설정·장치 목록·버튼 상태·휠 모드·
백업은 `~/.config/flockswitch/`에 그대로 유지됩니다.


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
- **Wayland에서 키보드 단축키**: GlobalShortcuts를 제공하는 데스크톱 포털(GNOME 48 이상과 KDE Plasma에는 있고, sway의 포털에는 없음)과 앱 메뉴의 flockswitch 항목(`flockswitch.desktop`: install.sh가 추가하고, Homebrew 설치는 설치·설정 점검이 추가)이 필요합니다. 확인: `busctl --user get-property org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.portal.GlobalShortcuts version`에 숫자가 나오면 됩니다. 없으면 데스크톱의 사용자 단축키에 `flockswitch switch N`과 `flockswitch locate`를 지정해 씁니다
- **Wayland에서 화면 모서리 전환**: KDE Plasma·sway·Hyprland는 따로 필요한 것이 없습니다. GNOME은 45 이상과 입력 캡처를 지원하는 데스크톱 포털(`xdg-desktop-portal` 1.17 이상, `xdg-desktop-portal-gnome` 45 이상, 보통 이미 설치됨)이 필요합니다. 확인: `busctl --user get-property org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.portal.InputCapture version`에 숫자가 나오면 됩니다. GNOME 44 이하는 X11(Xorg) 세션에서 쓰세요
- **GNOME 도우미 확장**(Wayland의 GNOME, 권장): 컴포지터 안에서 포인터를 읽고 마우스·키보드를 가져가거나 넣어 주는 작은 GNOME Shell 확장(`flockswitch@wwhalefe.github.io`)입니다. 시스템이 입력 캡처나 원격 조작 허용을 묻지 않게 됩니다(앞의 것은 GNOME 50 이하가 기억하지 못하고, 뒤의 것은 기억(Remember)을 켜지 않으면 다시 묻습니다). `install.sh`와 `get.sh`가 설치하고, `flockswitch setup`이 권하며(Homebrew 설치도), 앱 업데이트가 파일을 새것으로 유지합니다. 설치한 뒤 **한 번 로그아웃했다 다시 로그인**하세요. Wayland의 GNOME은 로그인할 때만 새 확장을 찾습니다. 확장은 이름이 `flockswitch`인 프로그램에만 답하고, X11이나 다른 데스크톱에서는 쓰지 않으며, `FLOCKSWITCH_GNOME_HELPER=0`으로 끌 수 있습니다. 확장이 있으면 모서리 전환과 공유가 X11처럼(앱이 커서를 읽어) 동작하고, 마우스를 흔들어 커서 찾기도 됩니다
- 항목별 명령은 아래 [직접 설정하기](#직접-설정하기)에 있고, `flockswitch setup`이 대신 실행해 줍니다

이 항목들은 flockswitch가 직접 확인합니다. 터미널에서는 `flockswitch setup`([설치](#설치) 참고), 앱에서는 다음과 같습니다. 빠진 것이 있으면 시작한 뒤 알림을 한 번 보내고(트레이 아이콘이 빠진 기본 GNOME에서 유용. 맨 처음 실행할 때는 알림 대신 설정 창이 설치·설정 점검을 연 채로 한 번 뜹니다), 트레이 메뉴에 **설정 필요 항목 N개…**가 생기며, 설정 → 일반 맨 위에 **설치·설정 점검** 카드가 나옵니다. 빠진 항목마다 필요한 이유와 **설치**·**나중에** 버튼이 있고, 두 개 이상이면 **모두 설치**가 관리자 권한 명령을 한데 모아 실행해 비밀번호를 한 번만 묻습니다. 설치를 누르면 실행할 명령을 그대로 먼저 보여 주고, **실행**을 눌러야 시작합니다. 관리자 권한이 필요한 명령은 `pkexec`로 실행해 데스크톱이 비밀번호를 묻습니다. 그렇게 할 수 없으면(pkexec가 없거나 취소함) 같은 명령을 `sudo`를 붙여 보여 주고 **명령 복사** 버튼으로 터미널에 붙여 넣을 수 있게 합니다. 할 수 있는 일:

| 항목 | 알려 주는 경우 | 실행하는 것 |
|---|---|---|
| udev 규칙 | `/etc/udev/rules.d/70-flockswitch.rules`가 없거나 이 버전의 규칙과 다를 때, 또는 장치를 열 권한이 없을 때 | 규칙 쓰기, `udevadm control --reload-rules`, `udevadm trigger --subsystem-match=hidraw` (비밀번호). 그다음 수신기를 뺐다 꽂거나 블루투스를 다시 연결 |
| GNOME 도우미 확장 | Wayland의 GNOME에서 확장이 없거나, 꺼져 있거나, 설치했지만 아직 불러오지 않았을 때 | 확장을 `~/.local/share/gnome-shell/extensions/`에 복사하고 `gnome-extensions enable flockswitch@wwhalefe.github.io` 실행 (비밀번호 없음). 그다음 한 번 로그아웃 후 다시 로그인 |
| 데스크톱 포털 | Wayland의 GNOME 45 이상에서 입력 캡처가 없고 `xdg-desktop-portal`·`xdg-desktop-portal-gnome`이 없거나 오래되었을 때 | `apt-get`·`dnf`·`zypper`·`pacman`으로 설치 (비밀번호). 그다음 로그아웃 후 다시 로그인. GNOME 44 이하는 설치 대신 X11 세션을 안내 |
| 한글 글꼴 | 창이 한국어이고 한글 글꼴이 없을 때(`fc-list :lang=ko`) | `fonts-noto-cjk`·`google-noto-sans-cjk-fonts`·`noto-sans-cjk-fonts`·`noto-fonts-cjk` (비밀번호). 설치하면 창이 바로 그 글꼴을 씀. 그 전까지 이 카드는 영어로 표시 |
| 트레이 아이콘 | StatusNotifierItem을 보여 줄 곳이 없는 GNOME (이미 있는 Ubuntu는 제외) | `gnome-shell-extension-appindicator` (비밀번호), 이어서 `gnome-extensions enable appindicatorsupport@rgcjonas.gmail.com`. 그다음 로그아웃 후 다시 로그인 |
| 앱 메뉴 항목 | Homebrew로 설치했고 앱 메뉴 항목이 없을 때 | `~/.local/share/applications`에 링크 만들기 (비밀번호 없음) |
| 새 버전 | Homebrew로 설치했고 새 릴리스가 있을 때 | `brew trust --formula wwhalefe/tap/flockswitch`(Homebrew 6 이상), `brew update`, `brew upgrade --formula wwhalefe/tap/flockswitch` 후 새 버전으로 다시 시작 (비밀번호 없음) |

**나중에**를 누른 항목은 이 컴퓨터에서 다시 알리지 않으며(`config.toml`의 `[setup] snoozed`, 설정 백업에는 넣지 않음) **다시 보기**로 되돌릴 수 있습니다. 실행 중 출력은 모두 로그에 남습니다. 확인할 때는 파일을 읽고 세션 버스, `fc-list`, `dpkg-query`·`rpm -q`·`pacman -Q`에 물어볼 뿐 비밀번호나 네트워크가 필요하지 않습니다.

## 설치

**명령 한 줄**이면 됩니다(한 번 묻고, 비밀번호도 한 번).

```sh
curl -fsSL https://github.com/WWhaleFe/flockswitch-linux/releases/latest/download/get.sh | sh
```

이 컴퓨터에 맞는 최신 릴리스(x86_64 또는 arm64)를 받아 릴리스의 `SHA256SUMS.txt`와 맞을 때만 진행하고, 프로그램을 `~/.local/bin`에 넣고 앱 메뉴에 flockswitch를 추가한 뒤 `flockswitch setup`(아래)을 실행하고 flockswitch를 시작합니다. 묻지 않고 설치하려면 `… | sh -s -- --yes`. sudo를 붙이지 말고 내 계정으로 실행하세요. 다시 실행하면 최신 버전으로 다시 설치합니다.

**`flockswitch setup`**은 이 컴퓨터에 더 필요한 것과 그 이유를 목록으로 보여 줍니다. 장치를 열게 해 주는 udev 규칙(항상), 그리고 해당할 때 한글 글꼴, 트레이 아이콘 확장(기본 GNOME), 화면 모서리 전환용 데스크톱 포털(Wayland의 GNOME), 앱 메뉴 항목(Homebrew)입니다. 실행할 명령을 그대로 보여 주고 `지금 설치할까요? [Y/n]`을 물은 뒤, 관리자 권한 명령은 모두 `sudo` 한 번으로(비밀번호 한 번, 터미널이 없으면 데스크톱의 비밀번호 창), 나머지는 내 권한으로 실행하고, 끝으로 남은 일("로그아웃 후 다시 로그인", "수신기 뺐다 꽂기" 등)을 알려 줍니다. `--yes`는 묻지 않고, `--dry-run`은 보여 주기만 하고, `--check`는 빠진 것이 있으면 1로 끝납니다(종료 코드는 `flockswitch setup --help`). 실행한 내용은 `~/.config/flockswitch/setup.log`에 남습니다. 언제든 다시 실행해도 되며, 앱의 설치·설정 점검(위)도 같은 일을 합니다.

[Homebrew](https://brew.sh)를 쓰면:

```sh
brew install --formula wwhalefe/tap/flockswitch && flockswitch setup
```

그다음 앱 메뉴에서 flockswitch를 실행하세요. 업데이트는 `brew update && brew upgrade --formula wwhalefe/tap/flockswitch`입니다. Homebrew로 설치한 flockswitch는 묻지 않고 업데이트하지 않습니다: 새 버전이 나오면 트레이 메뉴에 **업데이트 가능 (x.y.z)…**가 생기고, 설정 → 일반의 **지금 업데이트**가 이 명령을 보여 준 뒤 실행을 누르면 실행합니다('새 버전 자동 설치'가 켜져 있으면 알림으로 묻습니다). 이 탭에는 같은 이름의 macOS용 cask가 함께 있어서, `--formula`와 `wwhalefe/tap/`을 빼면 Homebrew가 cask까지 읽으려다 멈춥니다.

Homebrew 6 이상에서 `Refusing to load … from untrusted tap wwhalefe/tap` 오류가 나면(그 전 버전에서 설치한 경우), 한 번만 실행하세요: `brew trust --formula wwhalefe/tap/flockswitch`

직접 받으려면 [Releases](../../releases)에서 `flockswitch-linux-<x86_64|aarch64>-<버전>.tar.gz`를 받은 뒤:

```sh
tar xzf flockswitch-linux-x86_64-<버전>.tar.gz
cd flockswitch-linux-x86_64-<버전>
./install.sh      # 한 줄 설치가 내려받은 뒤에 하는 일과 같음. --yes는 묻지 않음
```

업데이트는 저절로 설치됩니다: 하루 한 번 확인할 때 새 버전을 찾으면(또는 **지금 업데이트**를 누르면) 내려받아 릴리스의 SHA256SUMS.txt와 비교한 뒤, `~/.local/bin`의 프로그램과 앱 메뉴 항목·아이콘을 바꾸고 flockswitch를 다시 시작합니다. 설정 → 일반에서 끌 수 있습니다(`flockswitch set auto_update false`). 꺼 두어도 **지금 확인** 뒤 **지금 업데이트**를 누르면 언제든 설치합니다. 비밀번호는 묻지 않으며, 새 릴리스에서 udev 규칙이 바뀌었으면 설치·설정 점검이 새 규칙 설치를 권합니다. 쓸 수 없는 폴더(예: `/usr/bin`)에 있는 프로그램은 내려받기만 합니다.

### 직접 설정하기

`flockswitch setup`이 실행하는 것과 같은 명령입니다. 직접 입력하고 싶을 때 쓰세요(`flockswitch setup --dry-run`이 이 컴퓨터에 필요한 것만 보여 줍니다).

```sh
# udev 규칙(항상). 압축을 푼 릴리스 폴더에서 실행. Homebrew는 파일이
# "$(brew --prefix)/opt/flockswitch/share/flockswitch/70-flockswitch.rules". 끝나면 수신기를 뺐다 꽂기
sudo install -m 644 70-flockswitch.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=hidraw

# 한글 글꼴(창이 한국어일 때). Fedora: google-noto-sans-cjk-fonts, openSUSE: noto-sans-cjk-fonts, Arch: noto-fonts-cjk
sudo apt-get install -y fonts-noto-cjk

# 기본 GNOME(Fedora, Debian. Ubuntu·KDE는 필요 없음)의 트레이 아이콘. 끝나면 로그아웃 후 다시 로그인
sudo dnf install -y gnome-shell-extension-appindicator      # Debian: sudo apt-get install -y …
gnome-extensions enable appindicatorsupport@rgcjonas.gmail.com

# GNOME 도우미 확장(Wayland의 GNOME): 권한 질문이 사라집니다. 끝나면 한 번 로그아웃 후 다시 로그인.
# 압축을 푼 릴리스 폴더에서 (Homebrew: "$(brew --prefix)/opt/flockswitch/share/flockswitch/gnome-extension").
mkdir -p ~/.local/share/gnome-shell/extensions/flockswitch@wwhalefe.github.io
cp gnome-extension/flockswitch@wwhalefe.github.io/* ~/.local/share/gnome-shell/extensions/flockswitch@wwhalefe.github.io/
gnome-extensions enable flockswitch@wwhalefe.github.io

# GNOME 45 이상(Wayland)에서 입력 캡처가 없을 때 화면 모서리 전환용. 끝나면 로그아웃 후 다시 로그인.
# GNOME 44 이하는 설치 대신 로그인 화면에서 "GNOME on Xorg"(또는 "Ubuntu on Xorg") 선택
sudo apt-get install -y xdg-desktop-portal xdg-desktop-portal-gnome   # Fedora: sudo dnf install -y …

# 앱 메뉴 항목(Homebrew만. install.sh는 직접 만듦)
mkdir -p ~/.local/share/applications
ln -sf "$(brew --prefix)/opt/flockswitch/share/applications/flockswitch.desktop" ~/.local/share/applications/
```

최소 설치 환경에서 설정 창이 열리지 않으면: `sudo apt-get install -y libxkbcommon0 libgl1 libegl1` (Fedora: `libxkbcommon mesa-libGL mesa-libEGL`).

## 사용

트레이 아이콘 메뉴에서 상태 확인, 켜기/끄기, **설정…**, 개발자에게 리포트, 업데이트 확인, 종료를 할 수 있습니다. **설정…**을 누르면 왼쪽 사이드바(네트워크·로지텍·앱, 11개 페이지)가 있는 크기 고정 설정 창이 열리고, 바꾸는 즉시 적용됩니다. 긴 페이지는 창 안에서 스크롤하며 창 크기는 바뀌지 않습니다. 사이드바 아래 상태 카드에 마우스 위치(연두색 점이 이 컴퓨터), 네트워크 상태, 마스터 마우스가 표시되고, 일반 옆 배지에 남은 Linux 설치 점검 항목 수가 나옵니다.

| 페이지 | 항목 | 내용 |
|---|---|---|
| 연결과 배치 | 공유·역할·짝 맺기·배치 지도 | 주의 사항 동의, 마스터/서브 역할, 6자리 짝 맺기, 주변 컴퓨터 및 IP 추가, 컴퓨터별 다른 컴퓨터 쓰기 스위치(마스터), **연결 다시 찾기**(절전 해제·잠금 해제 자동 재연결), 5 x 3 배치 지도(이웃 서브 직접 이동), Linux 세션 지원 안내 |
| 넘기기 | 지연·Control·도착·긴급 복귀 | 넘김 지연(효과 페이지의 카운트다운 사용), Ctrl 누른 채 넘기기, 도착 시 커서 표시, 긴급 복귀 단축키 Ctrl+Alt+Shift+Q |
| 키보드 | OS 전환 시 | Linux와 Mac·Windows·Android 쌍별·방향별 보조키 기준(기능·자리·바꾸지 않음과 대응표), 한/영 키, Caps Lock 길게 누르기, 한자 키, 이동 키, 화면 캡처(서브 변경은 마스터에 전달), **운영체제별 키 비교** 창 |
| 클립보드 | 공유·받은 파일·전송 | 커서를 따라가는 글·이미지·파일 공유, 서브끼리 공유(마스터 경유), Wayland XWayland 안내, 받은 폴더와 최근 파일, 전송 진행 상태 |
| 이 컴퓨터 | 정보 | 역할(연결과 배치에서 변경), 이름, 포트, IP 주소, 장치 ID |
| 버튼 전환 | 마우스 버튼 전환·휠 모드 | 켜기/끄기, 휠 모드 전환·제스처(누르기 및 ↑↓←→)·휠 클릭별 채널 지정(이 컴퓨터 채널 제외), 돌아오면 휠 모드 되돌리기(`set keep_wheel_mode`) |
| 단축키 전환 | 채널 단축키 | X11: 채널별 단축키(기본 Ctrl+Alt+1 / 2 / 3), ⓘ에 허용 조합 정리. **Wayland**: 데스크톱 포털 GlobalShortcuts 상태 및 시스템 설정 열기. 아래 [Wayland에서 마우스 찾기와 단축키](#wayland에서-마우스-찾기와-단축키) 참고 |
| 모서리 전환 | 화면 모서리 전환 | 방향별 채널, 머무는 시간(0.3~2초), 도착 시 커서 위치(X11), Ctrl 누른 동안에만 전환, Wayland 백엔드 상태 안내. 아래 [Wayland에서 화면 모서리 전환](#wayland에서-화면-모서리-전환) 참고 |
| 장치 | 마스터 마우스·함께 이동할 장치 | 버튼을 쓸 마우스와 Bolt·Unifying·블루투스 그룹 장치 선택(다른 컴퓨터에 가 있는 장치도 표시) |
| 효과 | 카운트다운·마우스 찾기·진동 | 모서리 카운트다운(종류·색상·크기·방향·미리 보기), 마우스 찾기(단축키·도착·`flockswitch locate`·X11 흔들기), 진동 요약과 **진동 설정…** 창(MX Master 4) |
| 일반 | 설치 점검·환경·업데이트·백업 | 맨 위 설치·설정 점검 카드(누락 항목 있을 때, 모두 설치 포함), 시작하기 전에 안내, 언어·로그인 시 자동 실행, 업데이트(자동/수동 및 새 버전 알려질 때 **지금 업데이트**), 설정 백업(저장·불러오기·관리·폴더 열기), 개발자에게, 초기화, 제거 |

리눅스에는 Logi Options+가 없어서 버튼을 미리 비워 둘 필요가 없습니다. **Solaar**를 쓴다면 flockswitch에서 켠 버튼에는 Solaar의 버튼 설정이나 규칙을 걸지 마세요. 그 밖에는 두 앱을 함께 설치해 두어도 됩니다. flockswitch를 끄거나 버튼을 끄면 그 버튼은 원래 상태로 돌아갑니다.

명령줄: `flockswitch help` (`setup`, `list`, `switch <N>`, `locate`, `set <key> <value>`, `run`, `quit`)

설정 위치: `~/.config/flockswitch/`

**설정 백업**(일반 탭): **저장**은 지금 설정을 `~/문서/flockswitch/flockswitch-settings-<컴퓨터 이름>-<날짜-시각>.toml`에 씁니다(문서 폴더는 `$XDG_DOCUMENTS_DIR`, 없으면 `~/Documents`). **불러오기…**는 그 폴더의 파일을 최신순으로 보여 주고, 고른 파일을 확인한 뒤 모든 설정을 그 파일의 것으로 바꿉니다. **관리…**는 파일마다 **이름 바꾸기**와 **삭제**(`gio trash`로 휴지통에, 휴지통이 없으면 완전히 삭제)를 보여 주고, **폴더 열기**는 그 폴더를 엽니다. 폴더에는 10개까지 둘 수 있으며(카드에 3/10처럼 표시), 10개가 되면 저장은 아무것도 쓰지 않고 관리에서 정리하라고 알려 줍니다. 파일은 Windows판·macOS판과 같은 TOML이라 서로 불러올 수 있습니다. 리눅스에 없는 설정은 건너뛰고, 빠진 설정은 기본값으로 두며, 올바른 설정이 아닌 파일은 이유와 함께 거절합니다. 채널은 컴퓨터마다 따로이니 다른 컴퓨터의 설정을 불러왔다면 채널을 확인하세요.

### Wayland에서 화면 모서리 전환

Wayland에서는 앱이 커서 위치를 읽을 수 없어서, flockswitch가 시작할 때 데스크톱이 제공하는 방법을 골라 로그(`~/.config/flockswitch/flockswitch.log`의 `edge backend: …`)와 설정 창의 모서리 페이지에 알려 줍니다.

| 세션 | 가장자리를 감지하는 방법 | 처음 한 번 할 일 |
|---|---|---|
| X11(Xorg), 모든 데스크톱 | 지금처럼 커서 위치를 읽음 | 없음 |
| KDE Plasma, sway, Hyprland 등 wlroots 계열 (Wayland) | 채널을 정한 가장자리를 따라 1픽셀 너비의 투명한 띠(wlr-layer-shell) | 없음 |
| GNOME (Wayland)과 [도우미 확장](#필요한-환경) | 확장이 X11처럼 커서를 읽음. 시스템이 묻지 않음 | 한 번 설치하고 로그아웃 후 다시 로그인 |
| GNOME 45 이상 (Wayland) | 데스크톱 포털의 입력 캡처: 채널을 정한 가장자리마다 감지선을 두고, 커서가 거기 닿으면 전환하거나 안쪽으로 움직일 때까지 시스템이 커서를 flockswitch에 넘김 | 시스템이 입력 캡처를 허락할지 물으면 허용. GNOME 51은 답을 기억하고, GNOME 45~50은 flockswitch를 시작할 때마다 묻습니다(45~48은 채널을 정한 가장자리나 모니터 구성이 바뀔 때도) |
| GNOME 44 이하, 또는 둘 다 없는 컴포지터(예: Weston) | 쓸 수 없음: 모서리 페이지에 이유를 표시 | 로그인 화면에서 X11 세션 선택 |

Wayland에서는 마우스가 돌아와도 커서를 보냈던 쪽 가장자리로 옮기지 못합니다(앱이 커서를 옮길 수 없음). 대기 효과는 KDE·sway·Hyprland에서 그려지고, GNOME에서는 XWayland의 화면 배치가 데스크톱과 같을 때 XWayland로 그립니다(아니면 효과 없이 기다리며, 진동은 어디서나 동작). 띠는 그 가장자리 맨 바깥 한 줄의 클릭을 받습니다. 'Ctrl 키를 누른 동안에만 전환': GNOME에서는 커서가 가장자리에 닿은 뒤에 Ctrl을 누르세요(그 전에 누른 키는 시스템이 알려 주지 않음). layer-shell 데스크톱에서는 커서가 띠 위에 있는 동안 Ctrl을 확인하려고 키보드를 잠시 넘겨받습니다(Ctrl 없이 최대 1.5초, 다른 키를 누르면 바로 돌려줌). 시험용으로 `FLOCKSWITCH_EDGE_BACKEND=layer-shell`, `input-capture`, `gnome-helper`, `none`으로 방법을 정할 수 있습니다. 도우미 확장을 쓰면 X11처럼 마우스가 돌아올 때 커서를 보냈던 쪽 가장자리에 놓습니다.

### Wayland에서 마우스 찾기와 단축키

Wayland에서는 앱이 커서를 계속 따라갈 수 없지만, 커서가 자기 면 위에 있는 동안에는 위치를 알 수 있습니다. 그래서 강조 표시가 보이는 동안(1초 남짓, 최대 2초)만 모든 화면 위에 커서를 받는 투명한 면을 깔고, 거기서 위치를 읽어 커서를 따라간 뒤 치웁니다.

| 세션 | 위치를 읽는 방법 | 표시하는 경우 |
|---|---|---|
| X11(Xorg), 모든 데스크톱 | 지금처럼 커서 위치를 읽음 | 단축키, 다른 컴퓨터에서 도착, 흔들기, `flockswitch locate` |
| GNOME (Wayland) | XWayland로 모든 화면을 덮는 투명한 창(모서리 대기 효과처럼 XWayland의 화면 배치가 데스크톱과 같을 때) | 단축키(시스템을 통해 등록), 도착, `flockswitch locate` |
| KDE Plasma, sway, Hyprland (Wayland) | 출력마다 투명한 layer-shell 면 | 위와 같음 |

강조 표시가 보이는 동안 첫 클릭은 그 면이 받아 표시를 끝내기만 하고 다른 곳에 전달되지 않습니다. 키 입력에는 영향이 없습니다. 마우스 흔들기는 커서를 계속 봐야 해서 X11에서만 동작합니다. `flockswitch locate`는 실행 중인 flockswitch에 표시를 요청하는 명령으로(마우스 찾기가 켜져 있어야 함), 데스크톱 사용자 단축키에 지정해 쓸 수 있습니다.

키보드 단축키(채널 단축키와 마우스 찾기 단축키)는 시스템의 단축키 기능인 데스크톱 포털의 GlobalShortcuts로 등록합니다. 설정의 키(바꾸기 전에는 Ctrl+Alt+1 / 2 / 3과 Ctrl+Alt+L)로 요청하면 시스템이 자기 창에서 한 번 묻고, 허용한 키를 flockswitch의 앱 ID로 기억합니다. 그 뒤에는 시스템 키보드 설정에서 바꿉니다(포털 버전 2에서는 단축키 페이지의 **시스템에서 키 바꾸기…**가 그 화면을 엽니다). 단축키 페이지는 단축키마다 시스템이 쓰는 키를 보여 주며, 여기서 고른 키와 다르면 주황색으로 표시합니다. 허용하지 않았다면 단축키를 바꾸거나 flockswitch를 다시 시작할 때 다시 묻습니다. 포털은 앱 메뉴에 항목이 있는 앱(`flockswitch.desktop`)에만 단축키를 허용합니다. 데스크톱에 그런 기능이 없거나(sway, 예전 GNOME) 허용하지 않았다면, 단축키 페이지에 데스크톱 사용자 단축키에 넣을 명령 `~/.local/bin/flockswitch switch 1`(2, 3)과 `~/.local/bin/flockswitch locate`를 보여 줍니다. 실행 중인 flockswitch가 바로 처리합니다. 로그 줄은 `locate:`, `shortcuts:`로 시작합니다.

## 제거

받은 폴더에서 `./uninstall.sh`를 실행합니다. 버튼을 원래대로 돌려놓고, 자동 실행과 설정을 지운 뒤, 프로그램과 앱 메뉴 항목, GNOME 도우미 확장, udev 규칙을 지웁니다.

Homebrew로 설치했다면 `flockswitch uninstall`을 실행한 다음 `brew uninstall --formula wwhalefe/tap/flockswitch`를 실행하고, udev 규칙과 메뉴 항목은 `brew info --formula wwhalefe/tap/flockswitch`에 나오는 대로 지웁니다.

## 개인정보

flockswitch는 이 PC에 연결된 Logitech 장치와 통신합니다. 인터넷 연결은 GitHub에서 새 버전이 있는지 확인할 때(시작할 때와 하루 한 번, 설정 → 일반에서 끌 수 있음)와 새 버전을 내려받을 때(저절로 또는 버튼을 눌러)뿐이며, 사용자나 장치에 대한 정보는 보내지 않습니다. "개발자에게 리포트"는 사용자의 메일 앱을 열 뿐이라 보내기 전에 내용을 모두 확인할 수 있습니다. (중복 실행을 막으려고 `127.0.0.1`의 포트 하나를 잡아 둘 뿐, 그 포트로 주고받는 데이터는 없습니다.)

## 문제가 생기면

[문제 신고](../../issues/new/choose) 양식에 아래를 붙여 주세요.

- `flockswitch list` 결과
- 배포판, 데스크톱(GNOME, KDE 등)과 세션(X11 / Wayland), 연결 방식(Bolt / Unifying / 블루투스)
- `~/.config/flockswitch/flockswitch.log`

설정 창에 "장치에 접근할 권한이 없습니다"가 보이면 udev 규칙이 없는 것입니다. `flockswitch setup`이나 일반 탭의 설치·설정 점검으로 설치한 뒤 수신기를 뺐다 꽂아 주세요.

## 시험 현황

실제 기기에서 시험한 환경: Ubuntu 26.04, GNOME 50, Wayland 세션.

| 기능 | 상태 |
|---|---|
| 마우스 버튼으로 전환 | 동작 확인 |
| GNOME Wayland에서 화면 모서리 전환 (입력 캡처 포털) | 동작 확인 |
| GNOME Wayland에서 마우스 찾기 (XWayland) | 동작 확인 |
| GNOME Wayland에서 키보드 단축키 (GlobalShortcuts 포털) | 동작 확인 |
| 설치·설정 점검 | 동작 확인 |
| Homebrew 설치본 업데이트 | 동작 확인 |
| GNOME 도우미 확장 (권한 질문 없이 모서리 전환, 공유, 마우스 찾기) | 구현, 실기기 미확인 |
| KDE Plasma·sway·Hyprland에서 화면 모서리와 마우스 찾기 (layer shell) | 구현, 미확인 |
| X11(Xorg) 세션: 화면 모서리, 마우스 찾기, 단축키 | 구현, 미확인 |

## 빌드

```sh
sudo apt install libudev-dev pkg-config   # Fedora: systemd-devel pkgconf
cargo build --release                      # target/release/flockswitch
```

Wayland, X11, OpenGL, D-Bus, libei는 실행할 때 불러오거나 순수 Rust로 다루므로 다른 개발 패키지는 필요 없습니다.

## 라이선스

MIT

네트워크 공유에 ufw를 쓰면 `sudo ufw allow 47160/tcp`와 `sudo ufw allow 5353/udp`(mDNS)를 실행하세요.
