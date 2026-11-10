# Changelog

## 0.7.0 — 2026-11-10

- Updates install themselves: when the daily check finds a new version, flockswitch downloads it, checks it against the release's SHA256SUMS.txt, replaces the program (and the app-menu entry and icon `install.sh` made) and restarts into it. **Update now** on the General tab and in the tray menu does the same at once. Switch it off with **Install updates automatically** (`set auto_update false`). A Homebrew copy is left to `brew upgrade flockswitch`, and a program in a folder you cannot write to is only downloaded, as before
- The haptics window's **Feel** button is now called **Test**
- The settings window starts a little taller (840×900) to make room for the new switch on the General tab

## 0.6.0 — 2026-11-10

- Haptics window: each situation on one line (name, pulse, Feel, switch), and 'use haptics / strength' set apart from the situations with more room between rows
- Haptics settings tidied: the pulse as the mouse leaves and the one when it is back are switched on and off on their own, each with its pulse beside it. Waiting at an edge offers **Rising / Even**; the edge waveform, which went unused by default, is folded into that choice (`set haptics.arrive true|false`)
- **Feel**: the haptics window plays the chosen pulse on the mouse at the chosen strength, leaving the mouse's own strength as it was
- The window says plainly that only a Logitech mouse with a haptic motor (currently the MX Master 4) vibrates
- Random pulse: choose **Random** for the leaving or arriving pulse and each one is a different pulse the mouse offers
- Effect size: the edge countdown and the find-the-cursor highlight each have a size, 75 to 150% (`set edge.anim_size`, `set locate.size`; 50 to 200% on the command line)
- A more consistent settings window: buttons, drop-downs and segment bars share one height, buttons a minimum width, row labels one weight, and the other-colour button is round like the rest

## 0.5.0 — 2026-11-09

- Effect colours: the screen-edge countdown and the find-the-cursor highlight each have their own colour (seven ready-made, any other from the picker). `set edge.anim_color #0A84FF`, `set locate.color #34C759`
- Random effects: choose **Random** for the edge countdown or the highlight and each one shows a different effect
- A larger settings window (840×840 to start, 760×640 to 1200×1100) with two columns on the Switching and General tabs; the Switching tab fits without scrolling at the default size

## 0.4.0 — 2026-11-09

- Settings window: its size stays within fixed limits (540×680 to start, 480×580 to 680×820), it no longer maximizes, and it keeps its size when you change tabs (a long page scrolls inside). The General tab gains Show log and Reset settings
- Haptics window: **Haptics Settings…** on the General tab opens a modal window with on/off, strength and the waveforms for switching, arrival and waiting at an edge, closed with Apply (stay open), OK (save and close) or Close (revert). A waveform you apply is played once
- Screen-edge countdown: while the cursor waits, a yellow effect runs down near it and the switch happens as it vanishes. Choose ring, semicircle, bar, pulse wave, neon laser, particle vortex, cyber scan or liquid, with direction, shrink and preview. Moving away calls it off at once. Drawn in click-through windows on an X11 session with a compositor (and a 32-bit visual); elsewhere, Wayland included, switching works but nothing is drawn
- Option to switch at a screen edge **only while Ctrl is held** (`set edge.control true`)
- Rising haptics: while waiting at an edge the pulses get quicker and stronger, with one more as it switches
- Find the cursor: a pulse ring, radar or spotlight around the cursor, from a shortcut, when the mouse arrives from another computer, or when it is shaken (Settings → General; X11 only)
- Receivers: devices that answer slowly while waking are read again, stale entries for a device that moved to another slot go away, a device that reports no wireless ID is no longer listed under an empty one, and offline paired devices are looked for more often while no mouse has been found
- A device without switching is no longer read again forever

## 0.3.0 — 2026-11-08

- Haptics on a mouse with a motor (MX Master 4, HID++ feature 0x19B0): a pulse as the mouse is switched away and one when it comes back here, and short pulses while the cursor waits at a screen edge to switch (Settings → General → **Haptics**; both can be turned off). The strength follows the haptics setting in Logi Options+. A mouse without a motor is simply quiet; `list` shows the waveforms a mouse offers, and `set haptics.switch_waveform`, `haptics.arrive_waveform`, `haptics.edge_waveform` choose them (0-14, 27)
- Haptics: the strength can be chosen (Settings → General → Haptics: Options+'s own setting, or 25/50/75/100 %). A chosen strength goes to the mouse at once, with a pulse to judge it by, and is applied again whenever the mouse is here, since the mouse forgets it when it loses power; `set haptics.intensity <0-100>`

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
