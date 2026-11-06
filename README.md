# flockswitch for Linux

**English** · [한국어](README.ko.md)

Press one mouse button and move your Logitech mouse **and** keyboard to another computer together.

flockswitch turns buttons of an MX Master mouse — the wheel mode-shift button, the gesture button and/or the wheel click (pressing the wheel) — into an Easy-Switch key. When you press one, every device in your group (keyboard, other mice) switches to the same Easy-Switch channel, then the mouse follows. It needs no network between the computers, so it works where Logitech Flow cannot (guest/public or locked-down networks).

This repository is the Linux version. The others live in [flockswitch-mac](https://github.com/WWhaleFe/flockswitch-mac) and [flockswitch-windows](https://github.com/WWhaleFe/flockswitch-windows); all three switch the same way and can be mixed.

> **Not tried on a Linux PC yet.** The Linux version shares its device code with the macOS and Windows versions, which are tested, but it has not run on real Linux hardware. Reports are very welcome.

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

## Install

With [Homebrew](https://brew.sh):

```sh
brew install wwhalefe/tap/flockswitch
```

Then run the commands `brew info flockswitch` shows: they install the udev rule (once, with sudo) and add flockswitch to the app menu. `brew upgrade flockswitch` updates it.

Or get `flockswitch-linux-<x86_64|aarch64>-<version>.tar.gz` from [Releases](../../releases), then:

```sh
tar xzf flockswitch-linux-x86_64-<version>.tar.gz
cd flockswitch-linux-x86_64-<version>
./install.sh
```

`install.sh` puts the program in `~/.local/bin`, adds flockswitch to the app menu, installs the udev rule `/etc/udev/rules.d/70-flockswitch.rules` (sudo asks for your password once) and starts flockswitch. If a receiver was plugged in, replug it once (or reconnect Bluetooth) so the rule applies.

## Use

The tray icon's menu shows the status, turns switching on or off, checks for updates, opens a report to the developer, and opens **Settings…** — a window with three tabs where every change applies at once:

- **Switching**: on/off, and per button (wheel mode-shift, gesture, wheel click — several can be on): the channel it goes to — one channel, chosen on each computer since settings are kept per computer; this PC's own channel is shown but cannot be chosen. The gesture button can also switch by holding it and moving the mouse up, down, left or right, each direction with its own channel.
- **Shortcuts** (on the same tab): on X11, keyboard shortcuts, one per channel (Ctrl+Alt+1 / 2 / 3 until you change them), pressed on any keyboard. **On Wayland** no app can register shortcuts for the whole desktop, so the tab lists commands to bind instead: in your desktop's keyboard settings, add custom shortcuts that run `~/.local/bin/flockswitch switch 1` (and `2`, `3`). The running flockswitch does the switch at once.
- **Devices**: the master mouse, and the devices that move along (Bolt, Unifying and Bluetooth devices can be mixed; devices on another computer are listed too)
- **General**: updates (check for a new version, download it), report to the developer, language (Korean or English), start at login, uninstall

Linux has no Logi Options+, so no button has to be set aside first. If you use **Solaar**, give the buttons you turn on in flockswitch no Solaar setting or rule; both apps can stay installed otherwise. When flockswitch quits or a button is turned off, the button is put back exactly as it was.

Command line: `flockswitch help` (`list`, `switch <N>`, `set <key> <value>`, `run`, `quit`).

Settings live in `~/.config/flockswitch/`.

## Uninstall

Run `./uninstall.sh` from the downloaded folder. It puts the buttons back to normal, removes start-at-login and the settings, then the program, the app menu entry and the udev rule.

Installed with Homebrew: run `flockswitch uninstall`, then `brew uninstall flockswitch`, and remove the udev rule and menu entry as `brew info flockswitch` shows.

## Privacy

flockswitch talks to the Logitech devices connected to this PC. Its only internet connection is to GitHub, to see whether a newer version exists (at start and once a day; turn it off in Settings → General) and to download it when you press the button; nothing about you or your devices is sent. "Report to the developer" opens your own mail app, so you see everything before sending. (A port on `127.0.0.1` is held only to stop a second copy from running; nothing is sent or received on it.)

## If something does not work

Open a [problem report](../../issues/new/choose) with:

- the output of `flockswitch list`
- your distribution, desktop (GNOME, KDE, …) and session (X11 or Wayland), and how the devices connect (Bolt / Unifying / Bluetooth)
- `~/.config/flockswitch/flockswitch.log`

"No permission to use the devices" in the settings window means the udev rule is missing: run `./install.sh` again (installed with Homebrew: the udev commands `brew info flockswitch` shows), then replug the receiver.

## Status

| Connection | Status |
|---|---|
| Logi Bolt receiver | implemented, not tried on Linux yet |
| Unifying receiver | implemented, not tried on Linux yet |
| Bluetooth | implemented, not tried on Linux yet |

## Build

```sh
sudo apt install libudev-dev pkg-config   # Fedora: systemd-devel pkgconf
cargo build --release                      # target/release/flockswitch
```

Everything else (Wayland, X11, OpenGL, D-Bus) is loaded at run time, so no other development packages are needed.

## License

MIT
