# Changelog

## 0.14.0 — 2026-11-12

Installing takes one command and one password; every feature stays as it was.

- **One-line install**: `curl -fsSL https://github.com/WWhaleFe/flockswitch-linux/releases/latest/download/get.sh | sh` downloads the latest release for this computer (x86_64 or arm64), stops unless it matches the release's SHA256SUMS.txt, and runs its install.sh (`… | sh -s -- --yes` installs without asking). It works with curl or wget and refuses to run as root. Every release now carries `get.sh`, also listed in SHA256SUMS.txt
- **`flockswitch setup`**, the setup check in a terminal: a short numbered list of what this computer still needs and why (the udev rule, and where they apply a font with Hangul, the GNOME tray icon extension, the desktop portal for screen edges, the app menu entry of a Homebrew install), the exact commands, then `Install these now? [Y/n]` (asked on the terminal, so it works under `curl | sh`). Every step that needs administrator rights runs in one `sudo` (pkexec, the desktop's dialog, where there is no terminal), so the password is asked once; then the steps run as you, then what is left ("log out and in", "replug the receiver"). One item failing does not stop the others. `--yes` doesn't ask, `--dry-run` only shows, `--check` exits 1 when something is missing; the exit status says what happened (`flockswitch setup --help`), and what runs is written to `~/.config/flockswitch/setup.log`. It looks at the same things as the settings window, including the desktop portal (asked briefly when flockswitch is not running); a Homebrew update is not part of it
- install.sh runs `flockswitch setup` (passing `--yes` on), so the udev rule and what the desktop needs come together with one password; if that cannot run, it installs the udev rule with sudo as before. It still works the same from an unpacked release folder
- Homebrew: `brew install --formula wwhalefe/tap/flockswitch && flockswitch setup` does the first-time setup (udev rule, app menu entry and the rest)
- Settings → General → Setup check: with two or more items missing, **Install all** shows every command in one confirmation and runs all the administrator steps in one pkexec, so the password is asked once; each row then shows how its own item went
- On the very first start (no settings yet), if something is missing, the settings window opens once on the setup check instead of the notification (kept in `config.toml` as `[setup] first_check` until then)
- README and the release notes start with the one-line install, then Homebrew, then the release file; the commands for setting up by hand are kept, shorter

## 0.13.0 — 2026-11-12

Nothing works differently; the tray app, which runs all day, holds less memory and does less work.

- The tray icon's click events no longer pile up in a queue nothing read, so memory no longer creeps up over a long uptime
- The memory of an effect drawn over the screen (the edge countdown, the cursor highlight) goes back to the system when it ends. Before, after one large effect (such as the spotlight over a whole display) the memory of later, smaller ones stayed with the app: up to tens of MB on a large display. flockswitch now has the system allocator give every large buffer back, and keeps two memory pools instead of one per thread
- On X11 and through XWayland (GNOME on Wayland) an effect keeps one copy of its frame instead of two: half the memory while it shows (a spotlight over a 2560×1440 display: 15 MB instead of 30 MB)
- The drawing thread starts with the first effect instead of at start; the update timer and the Wayland find-the-cursor watcher use small stacks
- The tray menu, the screen-edge and find-the-cursor watchers and the setup check no longer copy the whole status (with the device list) several times a second to see whether it changed, and the X11 shortcuts are looked at again only when the settings change
- Programs started from the tray (the settings window, the browser for the release page or a report) no longer stay behind as finished processes until flockswitch quits
- Without the udev rule, the same "no permission to open …" warning is no longer written to the log every few seconds, only when it changes
- Smaller program: the regular-expression support of `RUST_LOG` filters is left out (levels such as `RUST_LOG=debug` work as before)
- README: tested on Ubuntu 26.04 (GNOME 50, Wayland): switching, screen edges through input capture, find the cursor, shortcuts, the setup check and Homebrew updates. X11 sessions and KDE Plasma, sway and Hyprland are not tried yet

## 0.12.0 — 2026-11-12

- **Find the cursor on Wayland.** No app may follow the cursor there, so for the moment the highlight shows (about a second, 2 s at most) flockswitch puts a transparent surface that takes the pointer over every screen and reads the position there: on GNOME a window through XWayland (when XWayland's screens line up with the displays, as for the edge countdown), on KDE Plasma, sway and Hyprland a layer-shell surface on each output. The highlight follows the pointer while it shows. It comes up when the mouse arrives from another computer, with the shortcut, and with the new command `flockswitch locate` (for a custom desktop shortcut; the running flockswitch shows it). The first click while it shows only ends it and goes no further; typing is not affected. Shaking the mouse needs the cursor all the time and stays X11 only, as the settings window now says. X11 sessions work exactly as before
- **Keyboard shortcuts on Wayland** go through the system's shortcut service, the desktop portal's GlobalShortcuts: flockswitch asks for the channel shortcuts and the find-the-cursor one with the keys from the settings (Ctrl+Alt+1 / 2 / 3 and Ctrl+Alt+L by default), the system asks once in its own window, and pressing one does what the X11 shortcut does. GNOME keeps the keys you approve under flockswitch's app id, so after that they are changed in the system's keyboard settings (**Change keys in the system…** on the Shortcuts page opens them where the portal is version 2). Changing the shortcut settings registers them again; turned down, it asks again only after a shortcut changes or flockswitch restarts. The portal allows shortcuts only for apps in the app menu (`flockswitch.desktop`, added by install.sh or the setup check)
- The Shortcuts page on Wayland shows how that stands: registered (with the key the system uses under each shortcut, in amber where it is not the one chosen here), being asked, turned down, no menu entry, or no shortcut service on this desktop; in the last cases it lists commands for the desktop's own custom shortcuts, now including `flockswitch locate`
- Settings → General → Find the cursor says on Wayland what brings the highlight up there, and in red when this display layout cannot show it
- The log says each step: lines starting with `locate:` (how it is drawn, where the position came from, how it ended) and `shortcuts:` (the portal's version, what was asked for and the keys the system bound, each press)

## 0.11.1 — 2026-11-12

- Fixed screen-edge switching on GNOME 50's Wayland session (Ubuntu 26.04 and others) reporting "the desktop portal has no InputCapture". The portal has it, but GNOME 50 lists a capability older libraries don't know (it reports 15), and reading that value failed. flockswitch now looks only at the capabilities it knows, and uses input capture

## 0.11.0 — 2026-11-11

- **Setup check**: flockswitch finds what this computer is missing and installs it after you approve. Settings → General starts with a card, shown only when something is missing, with one row per item, why it's needed, **Install** (**Add**, **Turn on** or **Update** where that fits) and **Later**. Install first shows the exact commands and whether the system will ask for your password; nothing runs until **Run**. Administrator steps run through one `pkexec /bin/sh -c '…'` per item (fixed commands only), so the desktop's own password dialog appears; without pkexec, or when the dialog is cancelled, the same commands are shown with `sudo` and a **Copy commands** button. The row then says how it went (done, what to do next, or the exit code and the last lines of output), every line goes to the log, and the check runs again
- What it checks: the **udev rule** (missing, different from this version's, or a device that could not be opened; writes it, reloads udev and applies it, then flockswitch looks for the devices again — replug the receiver or reconnect Bluetooth), the **desktop portal** for screen edges on GNOME 45+ Wayland when input capture is missing (`xdg-desktop-portal`, `xdg-desktop-portal-gnome` with apt-get, dnf, zypper or pacman, then log in again; GNOME 44 or older is told about the X11 session instead, with nothing to install), a **Korean font** when the window is in Korean and `fc-list :lang=ko` finds none (the window loads it right away; until then the card is in English), the **tray icon** on GNOME with no StatusNotifierItem host (`gnome-shell-extension-appindicator`, then `gnome-extensions enable appindicatorsupport@rgcjonas.gmail.com`; Ubuntu already has one), the **app menu entry** of a Homebrew install (a link in `~/.local/share/applications`), and a **new version** of a Homebrew install
- Homebrew updates: **Update now** (setup check, General tab) runs `brew trust --formula wwhalefe/tap/flockswitch` on Homebrew 6+, `brew update` and `brew upgrade --formula wwhalefe/tap/flockswitch`, then flockswitch restarts into the new version from `$(brew --prefix)/bin/flockswitch` and the settings window comes back. brew is found from the program's own path, `$HOMEBREW_PREFIX` or `/home/linuxbrew/.linuxbrew`, not PATH. With **Install updates automatically** on, a Homebrew copy still asks first: a notification says a new version is there. Copies installed with `install.sh` update themselves as before
- Tray: **Setup needed: N…** and, for Homebrew, **Update available (x.y.z)…** open the settings window on the General tab, and one notification per start says what is missing (clicking it opens the same page). Nothing is installed from the tray
- **Later** hides an item on this computer (`[setup] snoozed` in `config.toml`, left out of settings backups and kept by Reset and Load); a new version is set aside for that version only. **Show again** brings them back
- The check only reads files and asks the session bus, `fc-list`, `dpkg-query` / `rpm -q` / `pacman -Q`, `gsettings` and `brew --version`; it needs no password and no network. X11 sessions skip the portal
- "No permission to use the devices" and the red box on the Edges page now have a button to the setup check; README and the release notes say the app finds and installs what is missing, with the manual commands kept as the fallback

## 0.10.2 — 2026-11-11

- Fixed the Homebrew upgrade instructions. The tap also has a macOS cask of the same name, so on Homebrew 6 or newer `brew upgrade flockswitch` tried to load the cask too and stopped with `Refusing to load cask … from untrusted tap`. The settings window, tray menu, log, README, release notes and `brew info` now say `brew upgrade --formula wwhalefe/tap/flockswitch`, and tell anyone who installed earlier to run `brew trust --formula wwhalefe/tap/flockswitch` once
- The install command is now `brew install --formula wwhalefe/tap/flockswitch`

## 0.10.1 — 2026-11-11

- On GNOME's Wayland session the settings window's title bar showed Korean as boxes: winit draws that title bar with one font that has no Hangul, so the title there is "flockswitch"
- The red box on the Edges page now has a "Found" line with the GNOME version and the desktop portal's answer about input capture (error text included), so a screenshot shows the cause. On GNOME 45 or newer it says to install the portal packages
- When the input capture portal or the compositor isn't ready yet (started at login), flockswitch looks again every 30 s, up to ten times. Where the GNOME version can't be read over D-Bus, it asks `gnome-shell --version`
- The release page lists what to install first (Homebrew, a Korean font, the tray extension, the desktop portal, the udev rule) and explains the screens people often see after installing; the README's requirements say what Wayland edge switching needs

## 0.10.0 — 2026-11-11

- Switching at the screen edge works on Wayland. flockswitch picks what the desktop offers and logs it (`edge backend: …`): on **GNOME 45 or newer** the desktop portal's input capture (a barrier on each edge that has a channel; when the cursor pushes against it the system hands the cursor to flockswitch, which switches after the time at the edge or gives it back as soon as you move back in, click or type), on **KDE Plasma, sway, Hyprland** and other wlroots desktops a transparent one-pixel strip along those edges (wlr-layer-shell). The corners and edges that continue onto another display are left out as on X11, the haptic pulses work the same, and a change of displays moves the barriers or strips. X11 sessions work exactly as before
- GNOME asks once whether flockswitch may capture input. GNOME 51 remembers the answer (the restore token is kept in `~/.config/flockswitch/input-capture.token`); GNOME 45 to 50 ask each time flockswitch starts, and 45 to 48 also when the edges with a channel or the displays change. Not allowed: the edges page says so, and changing the edge settings asks again
- The countdown is drawn on Wayland too: on a click-through layer-shell surface on KDE, sway and Hyprland, and on GNOME through XWayland when its screen layout matches the desktop's. Preview in the settings window works there as well (at the middle of the edge)
- "Only while Ctrl is held" on Wayland: on GNOME press Ctrl once the cursor is at the edge; on KDE, sway and Hyprland a strip takes the keyboard while the cursor rests on it to see Ctrl (for at most 1.5 s without Ctrl, and it gives it back at once when another key is pressed)
- On Wayland the cursor cannot be put back at the edge it left by, and finding the cursor stays X11 only; the settings window says what works on this desktop instead of "needs X11", and in red why not where nothing does (GNOME 44 or older, a compositor with neither)
- `FLOCKSWITCH_EDGE_BACKEND=layer-shell|input-capture|none` forces a way, for testing

## 0.9.0 — 2026-11-10

- Edge countdown: each effect (ring, semicircle, bar, pulse, laser, vortex, scan, liquid) keeps its own colour and size, and the ring, semicircle and bar their own direction (shrink-away for the ring and semicircle). Choosing Both ways for the ring no longer changes the bar; switching effect shows that effect's settings, and Preview uses them. Saved as `[edges.effects.<effect>]` (`color`, `size`, `direction`, `shrink`), the same as the Windows and macOS versions write; settings from before take their single colour, size, direction and shrink for every effect, and the old keys are still written for the chosen effect. Command line: `set edge.ring.color #0A84FF`, `set edge.bar.direction both`, `set edge.semicircle.shrink true`, `set edge.pulse.size 125`; `set edge.anim_color …` (and `anim_size`, `anim_direction`, `anim_shrink`) changes the chosen effect, or every effect while None or Random is chosen
- **Random** picks an effect each time with that effect's own colour and size, and now also a random direction (ring, semicircle, bar) and a random shrink-away (ring, semicircle), so it shows the variety; a chosen effect keeps its saved direction and shrink. The edges page says so under Random instead of showing controls for it
- Settings backup: shorter buttons (**Save**, **Load…**, **Manage…**, **Open folder**), with the number of saved files beside the title (3/10) and the explanation in the ⓘ. The folder holds up to 10 settings files: with 10, Save writes nothing and says so in red, with Manage… highlighted. **Manage…** lists the files newest first with their date, each with **Rename** (in place: not empty, up to 60 characters, none of / \ : * ? " < > |, never over another file) and **Delete** (confirmed in the row with **Really delete**, then moved to the Trash with `gio trash`, or deleted for good where there is no Trash; the window says which)
- The General tab fits without scrolling with every option for finding the cursor shown (highlight, shortcut, arrival, shake with its two rows), in Korean and English, even with a shortcut-taken note and an update error: the settings window opens 840×1070, and grows once (up to the screen's height less a margin, and at most 1300) when the page shown is taller than the window; a window taller than the screen is made to fit at the start. The grown size is the one kept across scale changes, and a window you made larger is never made smaller

## 0.8.1 — 2026-11-10

- The General tab is laid out as on the Mac: haptics under Find the cursor on the left, language and start at login at the top right
- More room between rows and the lines between them. Rows with a segment bar or buttons on the right (size, language, shortcut) sat almost on the line below, because the control pulled the row's height in; now the space above and below is even
- The settings window opens 980 tall to fit the extra room, so the General tab still fits without scrolling

## 0.8.0 — 2026-11-10

- Shaking to find the cursor no longer goes off at the slightest wobble: the mouse has to be shaken without a pause for the time chosen under **When the mouse is shaken** (0.5, 1, 1.5 or 2 s; 1 s to start with), and it can be set to count only while Ctrl, Alt or Shift is held (`set locate.shake_time 1000`, `set locate.shake_key ctrl`). Settings from before keep working, with 1 s and no key
- **Settings backup** on the General tab: **Save** writes the settings to `Documents/flockswitch` as `flockswitch-settings-<computer>-<date-time>.toml`, **Load…** lists those files newest first and, once you confirm, replaces every setting with the chosen file's, and **Open folder** shows the folder. The files are the same as the Windows and macOS versions', so they load on any of them (a shortcut Linux cannot use is left empty, an unknown shake key becomes none); a file that is not valid settings is refused with the reason. Channels are set per computer, so check them after loading another computer's settings
- Keyboard shortcuts: a round ⓘ beside the title of the Shortcuts page and beside the find-the-cursor shortcut explains which combinations work on Linux and which do not (Ctrl and/or Alt with a number or letter, or an F key; not Super, Shift alone or the desktop's own combinations; one another app has taken is refused and the row says so; Esc cancels; Wayland needs the desktop's own shortcuts). The rule under the shortcuts moved into it
- Updates: **Update now** after **Check now** installs a new version whether or not **Install updates automatically** is on (that switch only covers the installs the daily check makes by itself); a release without a file for this computer opens its page instead of failing. The two explanations under the update switches moved into ⓘ tooltips, as did the one in **To the developer**
- The settings window and the haptics window keep their size when moved to a monitor with a different scale (or when the scale changes): the size in points is remembered and asked for again if the window comes out larger or smaller
- The settings window starts at 840×960 so the General tab, with its new card, fits without scrolling; haptics and language swapped columns to balance them
- **Show log** is gone from the General tab (the log is still written to `~/.config/flockswitch/flockswitch.log`)

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
