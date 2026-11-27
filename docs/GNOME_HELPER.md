# GNOME helper extension (2.2.0)

## Why

On GNOME Wayland the app needs two portals: InputCapture (to be master: notice the
screen edge and take this computer's mouse and keyboard) and RemoteDesktop (to be
controlled: inject input). GNOME 50 and older cannot remember InputCapture, so the system
asks at every start; RemoteDesktop asks again whenever the answer was given without
Remember. Users find this unacceptable. A small GNOME Shell extension shipped with the app
does the same work inside the compositor, so after one install and one log out/in no
permission dialog appears again. Without the extension (not installed, disabled, other
desktop) everything stays exactly as today (portals).

## The extension (already written, do not change its API without updating both sides)

`packaging/linux/gnome-extension/flockswitch@wwhalefe.github.io/{metadata.json,extension.js}`

Session bus name `io.github.wwhalefe.Flockswitch`, object `/io/github/wwhalefe/Flockswitch`,
interface `io.github.wwhalefe.Flockswitch1`. It answers only callers whose process
executable is named `flockswitch` (others get AccessDenied). Captured input is sent as
unicast signals to the caller that started the capture.

Methods:

| Method | In | Out | Meaning |
|---|---|---|---|
| Hello | | u version (1) | presence check |
| GetPointer | | d x, d y, u mods | pointer in logical (stage) coordinates; mods are Clutter modifier bits, same values as X11 (Shift 1<<0, Lock 1<<1, Control 1<<2, Mod1/Alt 1<<3) |
| GetMonitors | | a(iiii) | x, y, width, height of each monitor, logical |
| Warp | d x, d y | | move the pointer |
| StartCapture | | b ok | modal grab: from now on this computer's pointer, buttons, keys and scroll do not reach the desktop and are sent to the caller; the pointer is hidden and held where it was |
| StopCapture | | | give input back |
| KeepAlive | | | must be called at least every 3 s while capturing, or the extension gives input back itself and emits CaptureEnded |
| Motion | d dx, d dy | | inject relative pointer motion |
| MoveTo | d x, d y | | inject absolute pointer motion |
| Button | u evdev code (BTN_LEFT 0x110 ...), b down | | inject a button |
| Key | u evdev key code, b down | | inject a key |
| Scroll | d dx, d dy | | inject scrolling, in wheel clicks, Clutter sign (positive = down / right) |

Signals (unicast to the capturing caller): `CapturedMotion(d dx, d dy)` (relative, from the
device), `CapturedButton(u evdev, b down)`, `CapturedKey(u evdev, b down)`,
`CapturedScroll(d dx, d dy)` (wheel clicks, Clutter sign: positive = down / right),
`CaptureEnded()` (the extension gave input back on its own: caller vanished or no
KeepAlive).

The extension releases keys and buttons it injected when it is disabled.

## Sign and code conventions (netcore `InputEvent`)

- `InputEvent::Scroll { dx, dy }`: dy > 0 is wheel up, dx > 0 is left (see
  `flock_net::linux_input::wheel`). So captured `(dx, dy)` from the extension becomes
  `Scroll { dx: -dx, dy: -dy }`, and injecting `Scroll { dx, dy }` calls the extension with
  `(-dx, -dy)`.
- Buttons: `flock_net::linux_input::button(evdev)` and `evdev_button(MouseButton)`.
- Keys: `evdev_to_hid` / `hid_to_evdev`.

## What to build in the app (Rust)

1. `src/gnome_ext.rs`: blocking zbus client (use `ashpd::zbus`, already a dependency; a
   dedicated session connection, short timeouts, ~300 ms). Public functions:
   - `available() -> bool`: GNOME Wayland session (`edge_wayland::session_is_wayland()` and
     XDG_CURRENT_DESKTOP contains gnome), name has an owner and `Hello` answers with
     version >= 1. Cache the result; while unavailable, re-check at most every 10 s, so
     turning the extension on later is picked up without restarting the app. Never call D-Bus
     when not on GNOME Wayland. Can be forced off with `FLOCKSWITCH_GNOME_HELPER=0`.
   - `pointer() -> Option<(f64, f64, u32)>`, `displays() -> Vec<Rect>`, `warp(x, y)`.
   - Inject helpers using no-reply calls (`call_noreply` / NO_REPLY_EXPECTED) so 1000 Hz motion
     does not wait for round trips: `motion(dx, dy)`, `move_to(x, y)`, `button(evdev, down)`,
     `key(evdev, down)`, `scroll(dx, dy)`.
   - Capture: `start_capture(sink: Sender<InputEvent>) -> anyhow::Result<()>` subscribes to the
     signals (match on our object path and interface, only from the bus name's current
     owner), calls StartCapture (false -> error), spawns a thread that forwards signals as
     `InputEvent`s and calls KeepAlive every 1 s; `stop_capture()` calls StopCapture and ends
     the thread. On `CaptureEnded` the forwarder stops and the platform must hand control back
     (make `Platform::ready()` report an error for a moment, the same way the X11/Wayland
     platforms report a lost capture, so netcore returns to local; read
     `netcore/src/service.rs` to see what netcore does when `ready()` fails while Controlling).
   - Pure mapping functions with unit tests: signal -> `InputEvent`, `InputEvent` -> extension
     call arguments, modifier mask -> control held.

2. `edge::platform` (src/edge.rs): today it is X11 only. When X11 is not available but
   `gnome_ext::available()`, use the extension for `pointer()`, `cursor()`, `control_down()`,
   `key_held()` (mods use the X11 bit values), `displays()` and `place()`. `available()`
   becomes true in that case too.

3. Edge switching backend: when the extension is available on GNOME Wayland, use the same
   polling path X11 uses (edge.rs main loop) instead of the InputCapture backend
   (`edge_portal`), and do not open any InputCapture session. Find where edge.rs /
   edge_wayland.rs choose between X11 and the Wayland backends (`probe`, `run`,
   FLOCKSWITCH_EDGE_BACKEND; add the value `gnome-helper`). If the extension appears or
   disappears while running, switch at the next probe or restart of the edge worker; a full
   restart of the app is acceptable for the switch back.
   Report it in the probe log and in the Edges page status like the other backends
   ("GNOME helper" / "GNOME 도우미").

4. Network (src/net.rs, src/net_logic.rs): add `src/net_gnome.rs`, a `Platform` modelled on
   `net_x11.rs`: clipboard as X11/Wayland do (reuse `crate::clipboard::Clipboard`), displays
   and cursor from `edge::platform` (always the real position, like X11), warp via
   `edge::platform::place`, `control_down` from it, `start_capture`/`stop_capture` via
   `gnome_ext`, `inject` via `gnome_ext` (Motion relative; Button/Key/Scroll mapped as above).
   In `net.rs` where the platform is created (around the `Wayland::new` / `X11::new`
   choice) prefer `net_gnome` when `gnome_ext::available()`. `probe()` returns send and
   receive true with reason "GNOME helper extension" in that case, without touching any
   portal. `runtime_config(..)` must set `app_drives_edges = true` when the helper is used
   (the app's edge watcher drives crossing, as on X11); today it is `!wayland`, so pass the
   right flag. No RemoteDesktop session and no InputCapture session may be created while the
   helper is used.

5. Installing the extension:
   - `packaging/linux/install.sh` (used by get.sh and the release tarball): copy the extension
     folder to `~/.local/share/gnome-shell/extensions/flockswitch@wwhalefe.github.io/`
     (replace existing files), then if `gnome-extensions` exists run
     `gnome-extensions enable flockswitch@wwhalefe.github.io || true`. Print one line telling
     the user that on Wayland GNOME finds a newly installed extension after logging out and in
     once. `uninstall.sh` removes the folder (and disables it first if possible).
   - The release tarball must contain the extension folder: check `.github/workflows/release.yml`
     and add it where the other packaging files are put in the archive.
   - Homebrew formula installs files under `share/`; make `flockswitch setup`
     (`setup.rs`, `setup_logic.rs`) offer an item "GNOME helper extension": state
     installed+enabled / installed but not active yet (needs log out and in) / missing; the
     fix copies the folder from wherever the app's packaging files are found (the same place
     setup already finds `flockswitch.desktop` for Homebrew, plus the tarball layout) and runs
     `gnome-extensions enable`. Only on GNOME. Add pure tests like the existing setup items.
   - The app's self-update (src/update.rs) installs new tarballs: make sure an update also
     updates the extension files when they changed (same copy as install.sh). GNOME loads the
     new extension code after the next log in; that is fine.

6. Texts (src/i18n.rs, Korean and English, no em dash "—", use "-"): the Connect page note
   that says GNOME 50 asks for input capture at every start (`net_portal_every`) and the
   remote control note must not show when the helper is in use; instead show a short line
   such as "GNOME 도우미 확장으로 동작합니다. 시스템이 권한을 묻지 않습니다." /
   "Running through the GNOME helper extension; the system does not ask for permission."
   When on GNOME Wayland without the helper, the existing notes stay and add one sentence:
   "flockswitch setup으로 GNOME 도우미 확장을 설치하면 묻지 않습니다(설치 후 한 번 로그아웃)."
   / "Install the GNOME helper extension with flockswitch setup to stop these questions (log
   out and in once after installing)."

7. Version 2.2.0: use `scripts/bump-version.sh 2.2.0` (it updates Cargo.toml and Cargo.lock).
   Add CHANGELOG.md and CHANGELOG.ko.md entries at the top in the existing style, and the
   README sections that describe Wayland permission prompts (README.md, README.ko.md,
   packaging/linux/release-notes/install.{ko,en}.md) with the helper, its one-time log out,
   and that it answers only flockswitch.

## Checks

- `cargo test` must pass on Linux CI; from macOS run the existing cross-check
  (`sh target/check-v2.sh --all-targets --offline` if present, else
  `cargo check --target x86_64-unknown-linux-gnu` per the repo's notes) and
  `python3 scripts/test-host.py` if it exists.
- Unit tests for every pure function added (mappings, availability rules, setup item,
  runtime_config with the helper).
- `sh -n` on the shell scripts.
