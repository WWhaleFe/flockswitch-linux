### 설치 전에 준비할 것

| 항목 | 필요한 경우 | 설치 |
|---|---|---|
| Homebrew | Homebrew로 설치할 때 (아래 1) | 없으면 아래 "Homebrew 없이 설치" |
| 한글 글꼴 | 설정 창의 한글 표시 | `sudo apt-get install -y fonts-noto-cjk` (Fedora: `google-noto-sans-cjk-fonts`) |
| 트레이 아이콘 확장 | Fedora·Debian의 기본 GNOME (Ubuntu·KDE는 필요 없음) | `gnome-shell-extension-appindicator` (아래 3) |
| 데스크톱 포털 | GNOME Wayland에서 화면 모서리 전환 | `xdg-desktop-portal`, `xdg-desktop-portal-gnome` (보통 이미 설치됨, 아래 4) |
| udev 규칙 | 항상 (장치를 열 권한) | 설치 명령에 포함 |

**1. Homebrew** (이미 있으면 건너뛰기)

```sh
# Ubuntu / Debian
sudo apt-get install -y build-essential procps curl file git
# Fedora
sudo dnf group install -y development-tools && sudo dnf install -y procps-ng curl file git

# Homebrew 설치 후, 지금 터미널과 새 터미널에서 brew를 쓸 수 있게 등록
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
echo 'eval "$(/home/linuxbrew/.linuxbrew/bin/brew shellenv)"' >> ~/.bashrc
eval "$(/home/linuxbrew/.linuxbrew/bin/brew shellenv)"
```

**2. 한글 글꼴**

```sh
sudo apt-get install -y fonts-noto-cjk            # Ubuntu / Debian
sudo dnf install -y google-noto-sans-cjk-fonts    # Fedora
```

**3. 트레이 아이콘** — KDE, Ubuntu, Cinnamon, XFCE는 할 일 없음. 기본 GNOME(Fedora, Debian)이면 확장을 설치하고 로그아웃 → 다시 로그인한 뒤 켭니다.

```sh
sudo dnf install -y gnome-shell-extension-appindicator      # Fedora
sudo apt-get install -y gnome-shell-extension-appindicator  # Debian
# 다시 로그인한 뒤
gnome-extensions enable appindicatorsupport@rgcjonas.gmail.com
```

**4. Wayland에서 화면 모서리 전환** — 내 환경을 먼저 확인하세요.

```sh
echo $XDG_SESSION_TYPE $XDG_CURRENT_DESKTOP   # 예: wayland ubuntu:GNOME
gnome-shell --version                          # GNOME일 때만
# GNOME: 입력 캡처 포털이 있는지 ("u 1" 같은 숫자가 나오면 있음)
busctl --user get-property org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.portal.InputCapture version
```

| 결과 | 할 일 |
|---|---|
| `x11` | 없음 |
| KDE Plasma, sway, Hyprland | 없음 |
| GNOME 45 이상 (Ubuntu 24.04 이상, Fedora 39 이상) | 처음 켤 때(GNOME 45~50은 켤 때마다) "입력 캡처" 허용 창에서 **허용** |
| GNOME 45 이상인데 `busctl`이 오류를 냄 | 포털 설치·업데이트 후 다시 로그인: `sudo apt-get install -y xdg-desktop-portal xdg-desktop-portal-gnome` (Fedora: `sudo dnf install -y xdg-desktop-portal xdg-desktop-portal-gnome`) |
| GNOME 44 이하 (Ubuntu 22.04, Debian 12) | Wayland에서는 쓸 수 없음. 로그아웃 → 로그인 화면 오른쪽 아래 ⚙에서 "Ubuntu on Xorg" 또는 "GNOME on Xorg"를 고르고 로그인 |

설정 → 전환 → 모서리에 빨간 안내가 나오면 그 아래 "확인한 내용" 줄에 GNOME 버전과 포털의 답이 나옵니다. 문제를 알릴 때 이 줄을 함께 보내 주세요.

**5. (최소 설치 환경만) 설정 창이 열리지 않으면**

```sh
sudo apt-get install -y libxkbcommon0 libgl1 libegl1          # Ubuntu / Debian
sudo dnf install -y libxkbcommon mesa-libGL mesa-libEGL       # Fedora
```

### Homebrew 설치 (Linux)

```sh
# 설치
brew install wwhalefe/tap/flockswitch

# 초기 설정 (최초 1회 — udev 규칙 및 앱 메뉴 등록)
sudo install -m 644 "$(brew --prefix)/opt/flockswitch/share/flockswitch/70-flockswitch.rules" /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=hidraw
mkdir -p ~/.local/share/applications
ln -sf "$(brew --prefix)/opt/flockswitch/share/applications/flockswitch.desktop" ~/.local/share/applications/

# 업데이트 / 제거
brew update && brew upgrade flockswitch
flockswitch uninstall && brew uninstall flockswitch
```

설치 후: 수신기를 한 번 뺐다 꽂고(블루투스는 다시 연결) 앱 메뉴에서 flockswitch를 실행하세요. `flockswitch list`에 마우스·키보드가 보이면 준비가 끝난 것입니다.

### Homebrew 없이 설치

아래 Assets에서 `flockswitch-linux-<x86_64|aarch64>-@VERSION@.tar.gz`를 받은 뒤:

```sh
tar xzf flockswitch-linux-x86_64-@VERSION@.tar.gz
cd flockswitch-linux-x86_64-@VERSION@
./install.sh      # ~/.local/bin 설치, 앱 메뉴, udev 규칙(sudo 비밀번호 한 번), 실행
```

### 설치 후 이런 화면이 보이면

- **상단에 "장치를 찾는 중…"**: 마우스·키보드가 지금 이 컴퓨터에 연결되어 있지 않습니다. 이 컴퓨터의 채널로 돌려놓고(블루투스면 연결될 때까지 몇 초) 기다리세요. 계속 그대로면 `flockswitch list` 결과를 확인하고, 일반 탭에 "장치에 접근할 권한이 없습니다"가 보이면 위의 udev 명령을 실행한 뒤 수신기를 뺐다 꽂으세요.
- **모서리 페이지의 빨간 안내**: 위 4번 표대로 하세요.
- **마우스 찾기 "Wayland에서는…"**: 마우스 찾기는 커서 위치가 필요해서 X11 세션에서만 동작합니다.
