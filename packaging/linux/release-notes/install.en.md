> [!IMPORTANT]
> **If Homebrew can't upgrade flockswitch (Homebrew 6 or newer)**
>
> If `brew upgrade flockswitch` stops with `Error: Refusing to load cask wwhalefe/tap/flockswitch from untrusted tap wwhalefe/tap.`, it's because this tap's macOS cask has the same name as the Linux formula. Run once in a terminal:
>
> ```bash
> brew trust --formula wwhalefe/tap/flockswitch
> brew update && brew upgrade --formula wwhalefe/tap/flockswitch
> brew list --versions flockswitch     # shows the new version
> flockswitch quit                     # then start flockswitch again from the app menu
> ```
>
> From 0.11.0 on, when a new version is out the app asks first (tray menu "Update available", Settings → General **Update now**) and, once you approve, runs these commands and restarts. Moving to 0.11.0 itself needs the commands above once.

### Quick install (one line)

```sh
curl -fsSL https://github.com/WWhaleFe/flockswitch-linux/releases/latest/download/get.sh | sh
```

Downloads the file for this computer (x86_64 or arm64), checks it against SHA256SUMS.txt below, puts the program in `~/.local/bin` and flockswitch in the app menu, then `flockswitch setup` lists what this computer still needs and why (the udev rule, and where they apply the Korean font, the GNOME tray icon extension and the desktop portal), shows the commands and asks once; everything that needs administrator rights runs in one `sudo`, so you type your password once. Finally it says what is left (such as "log out and in once") and starts flockswitch. `… | sh -s -- --yes` installs without asking. Run it as yourself, not with sudo. From then on flockswitch updates itself.

### Homebrew

```sh
brew install --formula wwhalefe/tap/flockswitch && flockswitch setup
```

`flockswitch setup` also adds the app menu entry; then start flockswitch from the app menu. Upgrade (or Settings → General → **Update now**, which runs it after you approve): `brew update && brew upgrade --formula wwhalefe/tap/flockswitch`. Uninstall: `flockswitch uninstall && brew uninstall --formula wwhalefe/tap/flockswitch`. No Homebrew yet? The one line above needs none (Homebrew itself: [brew.sh](https://brew.sh)).

### Install by hand

Download `flockswitch-linux-<x86_64|aarch64>-@VERSION@.tar.gz` from Assets below, then:

```sh
tar xzf flockswitch-linux-x86_64-@VERSION@.tar.gz
cd flockswitch-linux-x86_64-@VERSION@
./install.sh      # to ~/.local/bin, app menu entry, then flockswitch setup (one password), then starts it
```

<details>
<summary>The setup commands, to run yourself (what <code>flockswitch setup</code> runs)</summary>

`flockswitch setup --dry-run` prints only the ones this computer needs.

| What | When | Commands |
|---|---|---|
| udev rule | Always (permission to open the devices); then replug the receiver | `sudo install -m 644 70-flockswitch.rules /etc/udev/rules.d/` (Homebrew: `"$(brew --prefix)/opt/flockswitch/share/flockswitch/70-flockswitch.rules"`), `sudo udevadm control --reload-rules`, `sudo udevadm trigger --subsystem-match=hidraw` |
| Korean font | The window in Korean | `sudo apt-get install -y fonts-noto-cjk` (Fedora: `google-noto-sans-cjk-fonts`) |
| Tray icon | Stock GNOME on Fedora or Debian (not Ubuntu or KDE); then log out and in | `sudo dnf install -y gnome-shell-extension-appindicator` (Debian: `apt-get`), `gnome-extensions enable appindicatorsupport@rgcjonas.gmail.com` |
| Desktop portal | Screen edges on GNOME 45+ Wayland without input capture; then log out and in | `sudo apt-get install -y xdg-desktop-portal xdg-desktop-portal-gnome` (Fedora: `dnf`) |
| App menu entry | Homebrew only | `mkdir -p ~/.local/share/applications && ln -sf "$(brew --prefix)/opt/flockswitch/share/applications/flockswitch.desktop" ~/.local/share/applications/` |
| Window libraries | Minimal installs where the settings window doesn't open | `sudo apt-get install -y libxkbcommon0 libgl1 libegl1` (Fedora: `libxkbcommon mesa-libGL mesa-libEGL`) |

Screen-edge switching on Wayland: nothing to do on KDE Plasma, sway or Hyprland; on GNOME 45 or newer click **Allow** when the system asks about input capture (GNOME 45–50 ask each time flockswitch starts); GNOME 44 or older (Ubuntu 22.04, Debian 12) has no input capture, so pick "Ubuntu on Xorg" or "GNOME on Xorg" from ⚙ at the bottom right of the login screen.

</details>

### If you see this after installing

- **"Looking for devices…" at the top**: the mouse and keyboard aren't connected to this computer right now. Switch them to this computer's channel (Bluetooth takes a few seconds to connect). If it stays, check `flockswitch list`; if it says "No permission to use the devices", run `flockswitch setup` (or use the setup check on the General tab) and replug the receiver.
- **A red box on the Edges page**: run `flockswitch setup`, or see "Screen-edge switching on Wayland" above. Its "Found" line has the GNOME version and the portal's answer; please include it when you report a problem.
- **Find the cursor says "On Wayland…"**: finding the cursor needs the cursor position, so it works in an X11 session only.
