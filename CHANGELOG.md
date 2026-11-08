# Changelog

## 0.3.0 — 2026-11-08

- Haptics on a mouse with a motor (MX Master 4, HID++ feature 0x19B0): a pulse as the mouse is switched away and one when it comes back here, and short pulses while the cursor waits at a screen edge to switch (Settings → General → **Haptics**; both can be turned off). The strength follows the haptics setting in Logi Options+. A mouse without a motor is simply quiet; `list` shows the waveforms a mouse offers, and `set haptics.switch_waveform`, `haptics.arrive_waveform`, `haptics.edge_waveform` choose them (0-14, 27)

## 0.2.0 — 2026-11-08

- Switching at the screen edge, as Logi Flow does but without a network: rest the cursor against an outer edge of the screen for a moment and the group and the master mouse go to the channel set for that side (Settings → Switching → **Edges**: each side's channel, the time at the edge from 0.3 to 2 s, and whether the cursor is put at that edge when the mouse comes back). Each computer watches only its own screen; the corners do not count, nor does an edge that continues onto another display. On Wayland no app can read the cursor position, so this needs an X11 (Xorg) session.
- Settings: the channel rows of the buttons, the gesture directions and the screen edges now start with a gray **Off** segment, which replaces the on/off switch above each of them. Gray, not the accent colour, so a chosen Off reads as switched off
- Command line: `set edges true|false`, `set edge.left|right|top|bottom <1-3|none>`, `set edge.dwell <ms>`, `set edge.cursor true|false`

## 0.1.2 — 2026-11-06

- Updates: a downloaded update is checked against the SHA-256 checksums published with the release (SHA256SUMS.txt) and deleted if it does not match. For a release without checksums, the release page opens instead
- Updates: the check and the download go over https only, redirects included

## 0.1.1 — 2026-11-06

- Install with Homebrew: `brew install wwhalefe/tap/flockswitch`. `brew info flockswitch` shows how to add the udev rule and the app menu entry
- Start at login and the Wayland shortcut commands use Homebrew's `bin/flockswitch` link instead of the versioned folder an upgrade deletes, so they keep working after `brew upgrade`
- The missing-permission note also says what to do when installed with Homebrew

## 0.1.0 — 2026-11-02

- First Linux version, from flockswitch for macOS 0.4.3: the same switching, devices, settings window and “Go to channel” (this PC's own channel cannot be chosen). Not tried on a Linux PC yet
- Talks to receivers and Bluetooth devices through `/dev/hidraw`. `install.sh` installs a udev rule so you can open them without root; the settings window says when that permission is missing
- Tray icon over StatusNotifierItem (D-Bus); no GTK or libappindicator needed. Without a tray, starting flockswitch again opens its settings
- Keyboard shortcuts on X11. On Wayland, which lets no app grab keys for the whole desktop, the shortcuts tab lists `flockswitch switch 1/2/3` to bind in the desktop's own keyboard settings; the running flockswitch switches at once
- Release files for x86_64 and arm64 (glibc 2.35 or newer), with `install.sh` / `uninstall.sh`
