# Changelog

## 0.1.0 — 2026-11-02

- First Linux version, from flockswitch for macOS 0.4.3: the same switching, devices, settings window and “Go to channel” (this PC's own channel cannot be chosen). Not tried on a Linux PC yet
- Talks to receivers and Bluetooth devices through `/dev/hidraw`. `install.sh` installs a udev rule so you can open them without root; the settings window says when that permission is missing
- Tray icon over StatusNotifierItem (D-Bus); no GTK or libappindicator needed. Without a tray, starting flockswitch again opens its settings
- Keyboard shortcuts on X11. On Wayland, which lets no app grab keys for the whole desktop, the shortcuts tab lists `flockswitch switch 1/2/3` to bind in the desktop's own keyboard settings; the running flockswitch switches at once
- Release files for x86_64 and arm64 (glibc 2.35 or newer), with `install.sh` / `uninstall.sh`
