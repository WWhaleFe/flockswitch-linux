# flockswitch for Linux

**English** · [한국어](README.ko.md)

Press one mouse button and move your Logitech mouse **and** keyboard to another computer together.

flockswitch turns buttons of an MX Master mouse — the wheel mode-shift button, the gesture button and/or the wheel click (pressing the wheel) — into an Easy-Switch key. When you press one, every device in your group (keyboard, other mice) switches to the same Easy-Switch channel, then the mouse follows. It needs no network between the computers, so it works where Logitech Flow cannot (guest/public or locked-down networks).

This repository is the Linux version. The others live in [flockswitch-mac](https://github.com/WWhaleFe/flockswitch-mac), [flockswitch-windows](https://github.com/WWhaleFe/flockswitch-windows) and [flockswitch-android](https://github.com/WWhaleFe/flockswitch-android); all of them switch the same way and can be mixed.

> **Tested on Ubuntu 26.04 (GNOME 50, Wayland).** X11 (Xorg) sessions and KDE Plasma, sway or Hyprland have not been tried yet; reports are very welcome.

> Not affiliated with Logitech. "Logitech", "MX Master", "Easy-Switch", "Bolt" and "Unifying" are trademarks of Logitech.

## Supported mice

flockswitch was built and tested with the **MX Master 3** and **MX Master 4**. Other Logitech mice with the same buttons (HID++ 2.0 *Change Host* and reprogrammable controls) may work but have **not been tested yet**.

## How it works

flockswitch talks HID++ directly to the Logi Bolt / Unifying receiver (or a Bluetooth-connected device) through `/dev/hidraw`:

1. It diverts the buttons you turn on (controls `0x00C4` wheel mode-shift, `0x00C3` gesture, `0x0052` wheel click) so presses come to flockswitch.
2. On a press it sends *Change Host* (feature `0x1814`) to the group devices first, then to the mouse.
3. When the devices come back, it notices the link notification and diverts the button again.

Install flockswitch on **every** computer you switch between, and pair each device to the same channel number on each computer (e.g. channel 1 = Windows PC, channel 2 = Linux PC).

## Requirements

- x86_64 or arm64, with glibc 2.35 or newer (Ubuntu 22.04, Debian 12, Fedora 36 or newer)
- systemd-logind (nearly every desktop distribution), for the udev rule that lets you open the devices
- **Tray icon**: a panel that shows StatusNotifierItem icons — KDE Plasma, Ubuntu's GNOME, Cinnamon, XFCE and most others do. Plain GNOME (e.g. Fedora) needs the *AppIndicator and KStatusNotifierItem Support* extension. Without a tray, start flockswitch from the app menu again to open its settings.
- **Korean text** in the window: a font with Hangul, e.g. `fonts-noto-cjk` (Debian/Ubuntu) or `google-noto-sans-cjk-fonts` (Fedora)
- **Keyboard shortcuts on Wayland**: a desktop portal with GlobalShortcuts (GNOME 48 or newer and KDE Plasma have one; sway's portal does not) and flockswitch in the app menu (`flockswitch.desktop`: install.sh adds it, the setup check does for Homebrew). Check: `busctl --user get-property org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.portal.GlobalShortcuts version` prints a number. Without it, custom desktop shortcuts run `flockswitch switch N` and `flockswitch locate`
- **Screen-edge switching on Wayland**: nothing extra on KDE Plasma, sway or Hyprland. GNOME needs version 45 or newer and a desktop portal with input capture (`xdg-desktop-portal` 1.17+, `xdg-desktop-portal-gnome` 45+, usually already installed). Check: `busctl --user get-property org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.portal.InputCapture version` prints a number. On GNOME 44 or older use an X11 (Xorg) session
- Step-by-step install commands are under "Before you install" on the [release](../../releases/latest) page

flockswitch looks for these itself. When something is missing it sends one notification after it starts (useful on stock GNOME, where the tray icon may be what's missing), the tray menu shows **Setup needed: N…**, and Settings → General starts with a **Setup check** card: one row per missing item with why it's needed, **Install** and **Later**. Install first shows exactly the commands that will run; nothing runs until you press **Run**. Commands that need administrator rights run through `pkexec`, so your desktop asks for your password; if that isn't possible (no pkexec, or you cancelled), the card shows the same commands with `sudo` and a **Copy commands** button for a terminal. What it can do:

| Item | When it's offered | What runs |
|---|---|---|
| udev rule | `/etc/udev/rules.d/70-flockswitch.rules` is missing or differs from this version's, or a device couldn't be opened | writes the rule, `udevadm control --reload-rules`, `udevadm trigger --subsystem-match=hidraw` (password); then replug the receiver or reconnect Bluetooth |
| Desktop portal | GNOME 45+ on Wayland without input capture, `xdg-desktop-portal` / `xdg-desktop-portal-gnome` missing or too old | `apt-get` / `dnf` / `zypper` / `pacman` installs them (password); then log out and in. GNOME 44 or older is told to use an X11 session instead |
| Korean font | the window is in Korean and no font has Hangul (`fc-list :lang=ko`) | `fonts-noto-cjk` / `google-noto-sans-cjk-fonts` / `noto-sans-cjk-fonts` / `noto-fonts-cjk` (password); the window uses it at once. Until then this card is in English |
| Tray icon | GNOME without a StatusNotifierItem host (not Ubuntu, which has one) | `gnome-shell-extension-appindicator` (password), then `gnome-extensions enable appindicatorsupport@rgcjonas.gmail.com`; log out and in |
| App menu entry | installed with Homebrew, no menu entry yet | a link in `~/.local/share/applications` (no password) |
| New version | installed with Homebrew and a newer release is out | `brew trust --formula wwhalefe/tap/flockswitch` (Homebrew 6+), `brew update`, `brew upgrade --formula wwhalefe/tap/flockswitch`, then flockswitch restarts into it (no password) |

**Later** hides an item on this computer (kept in `config.toml` as `[setup] snoozed`, not in settings backups); **Show again** brings them back. Everything printed goes to the log. Detecting reads files and asks the session bus, `fc-list` and `dpkg-query` / `rpm -q` / `pacman -Q`; it never needs a password or the network. The manual commands below still work as before.

## Install

With [Homebrew](https://brew.sh):

```sh
brew install --formula wwhalefe/tap/flockswitch

# First-time setup: udev rule (once, with sudo) and app menu entry. The settings
# window's Setup check offers to do both for you.
sudo install -m 644 "$(brew --prefix)/opt/flockswitch/share/flockswitch/70-flockswitch.rules" /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=hidraw
mkdir -p ~/.local/share/applications
ln -sf "$(brew --prefix)/opt/flockswitch/share/applications/flockswitch.desktop" ~/.local/share/applications/
```

`brew update && brew upgrade --formula wwhalefe/tap/flockswitch` updates it. A Homebrew copy never updates without asking: when a new version is out, the tray menu shows **Update available (x.y.z)…** and Settings → General offers **Update now**, which shows these commands and runs them once you press Run (with **Install updates automatically** on, a notification asks). The tap also has a macOS cask of the same name, so without `--formula` and `wwhalefe/tap/` Homebrew tries to load the cask too and stops.

On Homebrew 6 or newer, if you see `Refusing to load … from untrusted tap wwhalefe/tap` (installed with an older Homebrew), run once: `brew trust --formula wwhalefe/tap/flockswitch`

Or get `flockswitch-linux-<x86_64|aarch64>-<version>.tar.gz` from [Releases](../../releases), then:

```sh
tar xzf flockswitch-linux-x86_64-<version>.tar.gz
cd flockswitch-linux-x86_64-<version>
./install.sh
```

`install.sh` puts the program in `~/.local/bin`, adds flockswitch to the app menu, installs the udev rule `/etc/udev/rules.d/70-flockswitch.rules` (sudo asks for your password once) and starts flockswitch. If a receiver was plugged in, replug it once (or reconnect Bluetooth) so the rule applies.

Updates install themselves: when the daily check finds a new version (or you press **Update now**), flockswitch downloads it, checks it against the release's SHA256SUMS.txt, replaces the program in `~/.local/bin` along with its app-menu entry and icon, and restarts. Turn this off in Settings → General (`flockswitch set auto_update false`); **Check now** followed by **Update now** still installs a new version whenever you ask. It never asks for a password; if a release changes the udev rule, the setup check offers the new one. A program in a folder you cannot write to (for example `/usr/bin`) is only downloaded.

## Use

The tray icon's menu shows the status, turns switching on or off, checks for updates, opens a report to the developer, and opens **Settings…** — a window with three tabs where every change applies at once:

- **Switching**: on/off, and per button (wheel mode-shift, gesture, wheel click — several can be on): the channel it goes to — one channel, chosen on each computer since settings are kept per computer; this PC's own channel is shown but cannot be chosen. The gesture button can also switch by holding it and moving the mouse up, down, left or right, each direction with its own channel.
- **Shortcuts** (on the same tab): on X11, keyboard shortcuts, one per channel (Ctrl+Alt+1 / 2 / 3 until you change them), pressed on any keyboard. The ⓘ beside the title (and beside the find-the-cursor shortcut) lists which combinations work: Ctrl and/or Alt with a number or letter (Shift may be added), or an F key; the desktop's own combinations and ones another app has taken cannot be used, and the row says so. **On Wayland** they go through the system's shortcut service: see [Find the cursor and shortcuts on Wayland](#find-the-cursor-and-shortcuts-on-wayland).
- **Edges** (same tab): rest the cursor against an outer edge of the screen for a moment (0.3 to 2 s, your choice) and the group and the master mouse go to the channel set for that side. No network: each computer watches its own screen, so turn it on on every computer and give each side the channel of the computer that stands there. While it waits an effect runs down near the cursor (ring, semicircle, bar, pulse, laser, vortex, scan, liquid or random). Each effect keeps its own colour and size, and the ring, semicircle and bar their own direction (the ring and semicircle can also shrink away instead); **Random** picks an effect each time, in that effect's colour and size, with a random direction and shrink-away where it has them. You can require Ctrl to be held. When the mouse comes back, the cursor appears at the edge it left by (X11). The corners do not count, and with several displays an edge that continues onto another display does not switch. Works on X11 and, in its own way, on Wayland: see [Screen edges on Wayland](#screen-edges-on-wayland).
- **Devices**: the master mouse, and the devices that move along (Bolt, Unifying and Bluetooth devices can be mixed; devices on another computer are listed too)
- **General**: haptics on a mouse with a motor, currently the MX Master 4 (a summary and the **Haptics Settings…** window: on/off, strength, a pulse as the mouse leaves and one when it is back, each with its own waveform or random, rising or even pulses at an edge, and **Test** to try them), find the cursor (a pulse ring, radar, spotlight or random effect in the colour and size you choose, from a shortcut, on arrival from another computer, with `flockswitch locate`, or by shaking the mouse for 0.5 to 2 s without a pause, optionally only while Ctrl, Alt or Shift is held; on X11 it needs a compositor, and on Wayland shaking is not available, see [below](#find-the-cursor-and-shortcuts-on-wayland)), language (Korean or English), start at login, updates (check for a new version, install it automatically or with **Update now**), report to the developer, settings backup (below), reset settings, uninstall, and at the top the setup check when something is missing (above). The settings window opens tall enough for the whole General tab, and grows (as far as the screen allows) when a page needs more room

Linux has no Logi Options+, so no button has to be set aside first. If you use **Solaar**, give the buttons you turn on in flockswitch no Solaar setting or rule; both apps can stay installed otherwise. When flockswitch quits or a button is turned off, the button is put back exactly as it was.

Command line: `flockswitch help` (`list`, `switch <N>`, `locate`, `set <key> <value>`, `run`, `quit`).

Settings live in `~/.config/flockswitch/`.

**Settings backup** (General tab): **Save** writes the current settings to `~/Documents/flockswitch/flockswitch-settings-<computer>-<date-time>.toml` (the Documents folder is `$XDG_DOCUMENTS_DIR` where set), **Load…** lists the files there, newest first, and after you confirm replaces every setting with the chosen file's, **Manage…** lists them with **Rename** and **Delete** (to the Trash with `gio trash`, or deleted for good where there is none), and **Open folder** shows the folder. The folder holds up to 10 files (the card shows how many, e.g. 3/10); with 10, Save writes nothing and says so until some are renamed away or deleted. The file is the same TOML the Windows and macOS versions write, so a file from either loads here: settings Linux does not have are skipped, missing ones keep their defaults, and a file that is not valid settings is refused with the reason. Channels are set per computer, so check them after loading another computer's settings.

### Screen edges on Wayland

No app may read the cursor on Wayland, so flockswitch picks what the desktop offers when it starts, logs it (`edge backend: …` in `~/.config/flockswitch/flockswitch.log`) and shows it on the edges page:

| Session | How the edges are watched | What you do once |
|---|---|---|
| X11 (Xorg), any desktop | reads the cursor, as before | nothing |
| KDE Plasma, sway, Hyprland and other wlroots desktops (Wayland) | a transparent strip one pixel wide along each edge that has a channel (wlr-layer-shell) | nothing |
| GNOME 45 or newer (Wayland) | the desktop portal's input capture: a barrier on each edge that has a channel; when the cursor pushes against it the system hands the cursor to flockswitch until it switches or you move back in | allow input capture when the system asks. GNOME 51 remembers the answer; GNOME 45 to 50 ask each time flockswitch starts (45 to 48 also when the edges with a channel or the displays change) |
| GNOME 44 or older, or a compositor with neither (e.g. Weston) | not available: the edges page says why | choose an X11 session on the login screen |

On Wayland the cursor is not put back at the edge it left by (no app may move it). The countdown is drawn on KDE, sway and Hyprland, and on GNOME through XWayland when its screen layout matches the desktop's (otherwise the wait runs without it; the haptic pulses work everywhere). The strips take the clicks on the outermost row of pixels of those edges. With "only while Ctrl is held": on GNOME press Ctrl once the cursor is at the edge (the system does not report keys pressed before); on the layer-shell desktops a strip briefly takes the keyboard while the cursor rests on it to see Ctrl (for at most 1.5 s without Ctrl, and never while you type). `FLOCKSWITCH_EDGE_BACKEND=layer-shell`, `input-capture` or `none` forces a way, for testing.

### Find the cursor and shortcuts on Wayland

No app may follow the cursor on Wayland, but one learns where it is while it is over its own surface. So for the moment the highlight shows (about a second, 2 s at most), flockswitch puts a transparent surface that takes the pointer over every screen, reads the position there and follows it, then takes it away:

| Session | How the position is read | Brought up by |
|---|---|---|
| X11 (Xorg), any desktop | reads the cursor, as before | the shortcut, arrival from another computer, shaking, `flockswitch locate` |
| GNOME (Wayland) | a transparent window through XWayland over every screen, when XWayland's screens line up with the displays (as for the edge countdown) | the shortcut (through the system), arrival, `flockswitch locate` |
| KDE Plasma, sway, Hyprland (Wayland) | a transparent layer-shell surface on each output | the same |

While the highlight shows, the first click lands on that surface and only ends it (that click goes no further); typing is not affected. Shaking the mouse needs the cursor all the time and works on X11 only. `flockswitch locate` asks the running flockswitch to show it (finding the cursor must be on), for a custom desktop shortcut.

The keyboard shortcuts (the channels' and the find-the-cursor one) go through the system's shortcut service, the desktop portal's GlobalShortcuts. flockswitch asks for them with the keys from the settings (Ctrl+Alt+1 / 2 / 3 and Ctrl+Alt+L until you change them); the system asks you once in its own window and then keeps the keys you approved under flockswitch's app id, so after that you change them in the system's keyboard settings (**Change keys in the system…** on the Shortcuts page opens them where the portal is version 2). The page shows the key the system uses under each shortcut, in amber where it is not the one chosen there. Turned down: changing a shortcut or restarting flockswitch asks again. The portal allows shortcuts only for apps in the app menu (`flockswitch.desktop`). Where the desktop has no such service (sway, older GNOME) or it was turned down, the page lists commands for the desktop's own custom shortcuts: `~/.local/bin/flockswitch switch 1` (`2`, `3`) and `~/.local/bin/flockswitch locate`; the running flockswitch acts at once. The log lines start with `locate:` and `shortcuts:`.

## Uninstall

Run `./uninstall.sh` from the downloaded folder. It puts the buttons back to normal, removes start-at-login and the settings, then the program, the app menu entry and the udev rule.

Installed with Homebrew: run `flockswitch uninstall`, then `brew uninstall --formula wwhalefe/tap/flockswitch`, and remove the udev rule and menu entry as `brew info --formula wwhalefe/tap/flockswitch` shows.

## Privacy

flockswitch talks to the Logitech devices connected to this PC. Its only internet connection is to GitHub, to see whether a newer version exists (at start and once a day; turn it off in Settings → General) and to download it, by itself or when you press the button; nothing about you or your devices is sent. "Report to the developer" opens your own mail app, so you see everything before sending. (A port on `127.0.0.1` is held only to stop a second copy from running; nothing is sent or received on it.)

## If something does not work

Open a [problem report](../../issues/new/choose) with:

- the output of `flockswitch list`
- your distribution, desktop (GNOME, KDE, …) and session (X11 or Wayland), and how the devices connect (Bolt / Unifying / Bluetooth)
- `~/.config/flockswitch/flockswitch.log`

"No permission to use the devices" in the settings window means the udev rule is missing: install it from the setup check on the General tab (or run `./install.sh` again; installed with Homebrew: the udev commands `brew info --formula wwhalefe/tap/flockswitch` shows), then replug the receiver.

## Status

Tried on real hardware: Ubuntu 26.04, GNOME 50, Wayland session.

| Feature | Status |
|---|---|
| Switching with the mouse button | works |
| Screen edges on GNOME Wayland (input capture portal) | works |
| Find the cursor on GNOME Wayland (through XWayland) | works |
| Keyboard shortcuts on GNOME Wayland (GlobalShortcuts portal) | works |
| Setup check | works |
| Updating a Homebrew install | works |
| Screen edges and find the cursor on KDE Plasma, sway, Hyprland (layer shell) | implemented, not tried yet |
| X11 (Xorg) session: edges, find the cursor, shortcuts | implemented, not tried yet |

## Build

```sh
sudo apt install libudev-dev pkg-config   # Fedora: systemd-devel pkgconf
cargo build --release                      # target/release/flockswitch
```

Everything else (Wayland, X11, OpenGL, D-Bus, libei) is loaded at run time or talked to in pure Rust, so no other development packages are needed.

## License

MIT
