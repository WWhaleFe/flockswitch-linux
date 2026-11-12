> [!IMPORTANT]
> **Homebrew로 설치했는데 업데이트가 안 된다면 (Homebrew 6 이상)**
>
> `brew upgrade flockswitch`가 `Error: Refusing to load cask wwhalefe/tap/flockswitch from untrusted tap wwhalefe/tap.`로 멈추면, 이 탭의 macOS용 cask가 Linux용 formula와 이름이 같아서입니다. 터미널에서 한 번만 실행하세요.
>
> ```bash
> brew trust --formula wwhalefe/tap/flockswitch
> brew update && brew upgrade --formula wwhalefe/tap/flockswitch
> brew list --versions flockswitch     # 새 버전 번호가 나오면 성공
> flockswitch quit                     # 그다음 앱 메뉴에서 flockswitch를 다시 실행
> ```
>
> 0.11.0부터는 새 버전이 나오면 앱이 설치할지 묻고(트레이 메뉴 "업데이트 가능", 설정 → 일반의 **지금 업데이트**), 승인하면 위 명령을 대신 실행한 뒤 다시 시작합니다. 0.11.0으로 넘어가는 이번 한 번은 위 명령을 직접 실행해 주세요.

### 빠른 설치 (한 줄)

```sh
curl -fsSL https://github.com/WWhaleFe/flockswitch-linux/releases/latest/download/get.sh | sh
```

이 컴퓨터에 맞는 파일(x86_64 또는 arm64)을 받아 아래 SHA256SUMS.txt와 비교하고, 프로그램을 `~/.local/bin`에, 앱 메뉴에 flockswitch를 넣습니다. 이어서 `flockswitch setup`이 이 컴퓨터에 더 필요한 것과 이유(udev 규칙, 그리고 해당하면 한글 글꼴·GNOME 트레이 아이콘 확장·데스크톱 포털)를 목록으로 보여 주고, 실행할 명령을 보여 준 뒤 한 번 묻습니다. 관리자 권한이 필요한 것은 모두 `sudo` 한 번으로 실행하므로 비밀번호도 한 번만 입력합니다. 끝으로 남은 일("로그아웃 후 다시 로그인" 등)을 알려 주고 flockswitch를 실행합니다. 묻지 않고 설치하려면 `… | sh -s -- --yes`. sudo를 붙이지 말고 내 계정으로 실행하세요. 그 뒤 업데이트는 flockswitch가 스스로 합니다.

### Homebrew

```sh
brew install --formula wwhalefe/tap/flockswitch && flockswitch setup
```

`flockswitch setup`이 앱 메뉴 항목도 추가하니, 그다음 앱 메뉴에서 flockswitch를 실행하세요. 업데이트(또는 설정 → 일반 → **지금 업데이트**: 승인하면 이 명령을 실행): `brew update && brew upgrade --formula wwhalefe/tap/flockswitch`. 제거: `flockswitch uninstall && brew uninstall --formula wwhalefe/tap/flockswitch`. Homebrew가 없다면 위의 한 줄 설치는 Homebrew 없이 됩니다(Homebrew 설치: [brew.sh](https://brew.sh)).

### 직접 설치

아래 Assets에서 `flockswitch-linux-<x86_64|aarch64>-@VERSION@.tar.gz`를 받은 뒤:

```sh
tar xzf flockswitch-linux-x86_64-@VERSION@.tar.gz
cd flockswitch-linux-x86_64-@VERSION@
./install.sh      # ~/.local/bin 설치, 앱 메뉴, flockswitch setup(비밀번호 한 번), 실행
```

<details>
<summary>설정 명령을 직접 실행하려면 (<code>flockswitch setup</code>이 실행하는 것)</summary>

`flockswitch setup --dry-run`은 이 컴퓨터에 필요한 것만 보여 줍니다.

| 항목 | 필요한 경우 | 명령 |
|---|---|---|
| udev 규칙 | 항상 (장치를 열 권한). 끝나면 수신기를 뺐다 꽂기 | `sudo install -m 644 70-flockswitch.rules /etc/udev/rules.d/` (Homebrew: `"$(brew --prefix)/opt/flockswitch/share/flockswitch/70-flockswitch.rules"`), `sudo udevadm control --reload-rules`, `sudo udevadm trigger --subsystem-match=hidraw` |
| 한글 글꼴 | 설정 창의 한글 표시 | `sudo apt-get install -y fonts-noto-cjk` (Fedora: `google-noto-sans-cjk-fonts`) |
| 트레이 아이콘 | Fedora·Debian의 기본 GNOME (Ubuntu·KDE는 필요 없음). 끝나면 로그아웃 후 다시 로그인 | `sudo dnf install -y gnome-shell-extension-appindicator` (Debian: `apt-get`), `gnome-extensions enable appindicatorsupport@rgcjonas.gmail.com` |
| 데스크톱 포털 | Wayland의 GNOME 45 이상에서 입력 캡처가 없을 때 화면 모서리 전환. 끝나면 로그아웃 후 다시 로그인 | `sudo apt-get install -y xdg-desktop-portal xdg-desktop-portal-gnome` (Fedora: `dnf`) |
| 앱 메뉴 항목 | Homebrew만 | `mkdir -p ~/.local/share/applications && ln -sf "$(brew --prefix)/opt/flockswitch/share/applications/flockswitch.desktop" ~/.local/share/applications/` |
| 창 라이브러리 | 최소 설치 환경에서 설정 창이 열리지 않을 때 | `sudo apt-get install -y libxkbcommon0 libgl1 libegl1` (Fedora: `libxkbcommon mesa-libGL mesa-libEGL`) |

Wayland에서 화면 모서리 전환: KDE Plasma·sway·Hyprland는 할 일 없음. GNOME 45 이상은 처음 켤 때(GNOME 45~50은 켤 때마다) "입력 캡처" 허용 창에서 **허용**. GNOME 44 이하(Ubuntu 22.04, Debian 12)는 입력 캡처가 없으니 로그아웃 → 로그인 화면 오른쪽 아래 ⚙에서 "Ubuntu on Xorg" 또는 "GNOME on Xorg"를 고르고 로그인.

</details>

### 설치 후 이런 화면이 보이면

- **상단에 "장치를 찾는 중…"**: 마우스·키보드가 지금 이 컴퓨터에 연결되어 있지 않습니다. 이 컴퓨터의 채널로 돌려놓고(블루투스면 연결될 때까지 몇 초) 기다리세요. 계속 그대로면 `flockswitch list` 결과를 확인하고, "장치에 접근할 권한이 없습니다"가 보이면 `flockswitch setup`을 실행하고(또는 일반 탭의 설치·설정 점검) 수신기를 뺐다 꽂으세요.
- **모서리 페이지의 빨간 안내**: `flockswitch setup`을 실행하거나 위의 "Wayland에서 화면 모서리 전환"을 보세요. 그 아래 "확인한 내용" 줄에 GNOME 버전과 포털의 답이 나오니, 문제를 알릴 때 함께 보내 주세요.
- **마우스 찾기 "Wayland에서는…"**: 마우스 찾기는 커서 위치가 필요해서 X11 세션에서만 동작합니다.
