### Before you install

| What | When | Install |
|---|---|---|
| Homebrew | To install with Homebrew (1 below) | Otherwise see "Install without Homebrew" |
| Korean font | Only to show Korean in the settings window | `sudo apt-get install -y fonts-noto-cjk` (Fedora: `google-noto-sans-cjk-fonts`) |
| Tray icon extension | Stock GNOME on Fedora or Debian (not Ubuntu or KDE) | `gnome-shell-extension-appindicator` (3 below) |
| Desktop portal | Screen-edge switching on GNOME Wayland | `xdg-desktop-portal`, `xdg-desktop-portal-gnome` (usually already there, 4 below) |
| udev rule | Always (permission to open the devices) | Part of the install commands |

**1. Homebrew** (skip if you have it)

```sh
# Ubuntu / Debian
sudo apt-get install -y build-essential procps curl file git
# Fedora
sudo dnf group install -y development-tools && sudo dnf install -y procps-ng curl file git

# Install Homebrew, then make brew available in this and new terminals
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
echo 'eval "$(/home/linuxbrew/.linuxbrew/bin/brew shellenv)"' >> ~/.bashrc
eval "$(/home/linuxbrew/.linuxbrew/bin/brew shellenv)"
```

**2. Korean font**

```sh
sudo apt-get install -y fonts-noto-cjk            # Ubuntu / Debian
sudo dnf install -y google-noto-sans-cjk-fonts    # Fedora
```

**3. Tray icon** — nothing to do on KDE, Ubuntu, Cinnamon or XFCE. On stock GNOME (Fedora, Debian) install the extension, log out and back in, then enable it:

```sh
sudo dnf install -y gnome-shell-extension-appindicator      # Fedora
sudo apt-get install -y gnome-shell-extension-appindicator  # Debian
# after logging back in
gnome-extensions enable appindicatorsupport@rgcjonas.gmail.com
```

**4. Screen-edge switching on Wayland** — check your session first:

```sh
echo $XDG_SESSION_TYPE $XDG_CURRENT_DESKTOP   # e.g. wayland ubuntu:GNOME
gnome-shell --version                          # GNOME only
# GNOME: is the input capture portal there? (a number such as "u 1" means yes)
busctl --user get-property org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.portal.InputCapture version
```

| You see | What to do |
|---|---|
| `x11` | Nothing |
| KDE Plasma, sway, Hyprland | Nothing |
| GNOME 45 or newer (Ubuntu 24.04+, Fedora 39+) | Click **Allow** when the system asks about input capture the first time (GNOME 45–50 ask each time flockswitch starts) |
| GNOME 45 or newer, but `busctl` gives an error | Install or update the portal, then log in again: `sudo apt-get install -y xdg-desktop-portal xdg-desktop-portal-gnome` (Fedora: `sudo dnf install -y xdg-desktop-portal xdg-desktop-portal-gnome`) |
| GNOME 44 or older (Ubuntu 22.04, Debian 12) | Not available on Wayland. Log out, pick "Ubuntu on Xorg" or "GNOME on Xorg" from ⚙ at the bottom right of the login screen, and log in |

When Settings → Switching → Edges shows a red box, its "Found" line has the GNOME version and the portal's answer. Please include that line when you report a problem.

**5. (Minimal installs only) If the settings window doesn't open**

```sh
sudo apt-get install -y libxkbcommon0 libgl1 libegl1          # Ubuntu / Debian
sudo dnf install -y libxkbcommon mesa-libGL mesa-libEGL       # Fedora
```

### Install with Homebrew (Linux)

```sh
# Install
brew install wwhalefe/tap/flockswitch

# First-time setup (udev rule and app menu entry)
sudo install -m 644 "$(brew --prefix)/opt/flockswitch/share/flockswitch/70-flockswitch.rules" /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=hidraw
mkdir -p ~/.local/share/applications
ln -sf "$(brew --prefix)/opt/flockswitch/share/applications/flockswitch.desktop" ~/.local/share/applications/

# Upgrade / Uninstall
brew update && brew upgrade flockswitch
flockswitch uninstall && brew uninstall flockswitch
```

After installing: replug the receiver once (or reconnect Bluetooth) and start flockswitch from the app menu. When `flockswitch list` shows your mouse and keyboard, you're set.

### Install without Homebrew

Download `flockswitch-linux-<x86_64|aarch64>-@VERSION@.tar.gz` from Assets below, then:

```sh
tar xzf flockswitch-linux-x86_64-@VERSION@.tar.gz
cd flockswitch-linux-x86_64-@VERSION@
./install.sh      # installs to ~/.local/bin, adds the menu entry and udev rule (sudo asks once), starts it
```

### If you see this after installing

- **"Looking for devices…" at the top**: the mouse and keyboard aren't connected to this computer right now. Switch them to this computer's channel (Bluetooth takes a few seconds to connect). If it stays, check `flockswitch list`; if the General tab says "No permission to use the devices", run the udev commands above and replug the receiver.
- **A red box on the Edges page**: follow the table in 4.
- **Find the cursor says "On Wayland…"**: finding the cursor needs the cursor position, so it works in an X11 session only.
