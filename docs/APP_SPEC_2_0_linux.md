# flockswitch for Linux 2.0.0: the same 2.0 as macOS 2.0.1 / Windows 2.0.0

Repo: `AppDev/flockswitch/linux` (WWhaleFe/flockswitch-linux, 1.x = 0.15.0), branch `v2`.
The first commit on `v2` (c3fbb6d) already imported:
- `netcore/` = exact copy of `../mac/netcore` (2.0.1). Never edit it here; a netcore
  change goes to mac, windows and linux identically.
- `src/net.rs`, `src/net_x11.rs`, `src/net_wayland.rs` and the network edits to
  config/edge/edge_portal/edge_wayland/i18n/main/settings_ui/setup/setup_logic/tray from
  the flockswitch2 repo's linux test.1 port (network input sharing only, old look, no
  clipboard, no master/sub UI, Logitech channel switch on crossing).
- `Cargo.toml` deps `arboard` (X11 + wayland-data-control, image-data) and `png` are added
  and fetched. Everything builds offline.

The goal: Linux 2.0.0 behaves and looks like the macOS 2.0.1 app, keeping everything Linux
1.x has (Wayland backends, setup check, GlobalShortcuts portal, udev, Homebrew notes,
backups, haptics, wheel mode, Logitech devices). Linux stays a beta.

## Reference (read, do not copy blindly)

- macOS 1.x -> 2.0.1 app changes: `git -C ../mac diff v0.14.0 v2.0.1 -- src .github README.md README.ko.md CHANGELOG.md CHANGELOG.ko.md PRIVACY.md`
  (config.rs, edge.rs, i18n.rs, main.rs, net.rs, settings_ui.rs, shared.rs, tray.rs, update.rs).
- Windows 1.x -> 2.0.0: `git -C ../windows diff v1.10.0 v2.0.0 -- src` (closer to Linux
  for the non-Mac parts: clipboard via arboard, Hangul rows shown disabled).
- Specs: `../Flockswitch2.0/docs/APP_SPEC_test2.md` (look, palette, cards, header network
  state, tray network line, effects tab, how-it-works window),
  `../Flockswitch2.0/docs/APP_SPEC_test3.md` (Logitech back, one edge owner, tabs,
  network sub-tabs, clipboard fixed 100 MB), `../mac/docs/APP_SPEC_2_0_replace.md`
  (rename back to flockswitch, migration, Hangul keys).
- Linux code that has no mac counterpart and must keep working: `edge_wayland.rs`,
  `edge_portal.rs`, `edge_layer.rs`, `wl.rs`, `overlay_wayland.rs`, `locate_wayland.rs`,
  `shortcuts_portal.rs`, `setup.rs`, `setup_logic.rs`, `window_size.rs`, Korean font
  lookup (`fc_match`), ASCII titles on GNOME Wayland.

## A. Names, paths, update channel (2.0 replaces Linux 1.x in place)

- Crate/binary `flockswitch`, version `2.0.0`, description like mac's. Settings folder
  stays `<config_dir>/flockswitch/` (`~/.config/flockswitch/`): a 0.15.0 `config.toml`,
  `devices.toml`, button state, wheel mode, backups load as they are.
- Single-instance ports, autostart entry, udev rule `70-flockswitch.rules`, desktop file
  `flockswitch.desktop`, app id for GlobalShortcuts: exactly 1.x's. Nothing named
  flockswitch2 anywhere except the migration below and its test.
- Updates: `WWhaleFe/flockswitch-linux`, assets `flockswitch-linux-<arch>-<v>.tar.gz`
  with the same layout as 1.x (`release.yml` Package step). The 0.15.0 updater must pick
  2.0.0 (numeric compare). Check `update.rs` `pick_asset` and the Homebrew path.
- One-time migration of test-build networking, same rule and same test as mac
  `Config::migrate_test_network`: source `<config_dir>/flockswitch2/flockswitch/`
  (config `[network]` incl. peers) and its network identity folder (find where netcore's
  `Identity::load_or_create(config_dir)` keeps `identity.der` / `identity.key` for the
  flockswitch2 build: `config_dir` was `<config_dir>/flockswitch2`; for 2.0 it is
  `<config_dir>/flockswitch`, check `net::config_dir()`), only when the 2.0 config has
  no `[network]` table; never delete the source; never overwrite an existing identity;
  marker file `migrated-from-flockswitch2`. Unit test with a temp dir.

## B. Network behaviour (match mac `src/net.rs` app glue)

- `runtime_config`: `on &= notice_accepted`; master hands `app_settings` (same JSON keys
  and the same `OWN_EDGE_KEYS` rule as mac, plus Linux-only keys if any must stay local);
  `apply_app_settings` on `on_master_settings`; `on_peer_changed` hook; `NetState` for the
  header and tray; `live_status()`; arrival locate highlight (Linux overlay/locate path,
  X11 and Wayland) when the cursor arrives or returns.
- `switch_logitech` hook is the no-op (network crossing never switches a Logitech
  channel). Remove the current `request_switch(channel)` call.
- One edge owner (test.3 B): `Config::resolve_edge_owner`, `network_edges_allowed()`,
  `logitech_edges_allowed()`, the same unit tests as mac. `edge.rs` (X11 watcher) and the
  Wayland backends: Logitech edge switching only when allowed; network crossing only when
  `network.on && !edges.on`. On GNOME Wayland both the Logitech edges and the network
  sender use the InputCapture portal: make sure only one InputCapture session exists at a
  time and that turning one owner off releases its barriers before the other arms.
- `app_drives_edges` and the countdown: on X11 the app's edge watcher drives crossing with
  `cross_delay_ms` and the countdown like mac (`network_side_ready`, `net::cross`). On
  Wayland, where there is no global cursor, keep netcore's edge hits (`arm_edges`,
  `take_edge_hit`) and add the delay/countdown only where the backend can draw it
  (layer-shell strips, portal barrier hit); where it cannot, document it in the code and
  in the README support table and keep crossing working without the countdown.
- Clipboard (new on Linux): implement `clipboard_text`, `set_clipboard_text`,
  `clipboard_files`, `set_clipboard_files`, `clipboard_image_png`,
  `set_clipboard_image_png`, `received_dir` on both `X11` and `Wayland` platforms via
  `arboard` (keep one long-lived `arboard::Clipboard` per platform on a dedicated thread
  or behind a Mutex: on X11 arboard serves the selection only while the instance lives;
  on Wayland arboard uses data-control when offered, otherwise falls back to X11 through
  XWayland; GNOME has no data-control). Files as `text/uri-list` (arboard `file_list`).
  Images RGBA <-> PNG with the `png` crate (as mac `encode_png` / `decode_png`, with the
  same size guard). `received_dir` = same choice as Windows/mac (Downloads/flockswitch or
  what netcore expects). Limit fixed 100 MB, no setting (test.3). Failures are logged, never
  panic. Unit tests for the PNG conversion and uri-list parsing that run on any host.
- Hangul (2.0 replace B): the two `mac_*_to_hangul` rows are shown on Linux disabled with
  the hint "맥에서 설정합니다" / "Set on the Mac" (like Windows). `lang1_on_mac` as now.
- Status file and permission/probe text: keep `net::support_text` (X11 / GNOME Wayland /
  other Wayland receive-only) and show it in the Network tab ([이 컴퓨터] sub-tab) and in
  the problem card when sending or receiving is unavailable.

## C. Settings window and tray (match mac 2.0.1 look and layout)

- Look from test.2 (palette, white cards on light gray, ink text, one lime #D4F53C accent
  with ink text, dashed hatched "other computer" card, header with network state next to
  the device line). Keep Linux's font handling (Hangul font via fc-match, weights per
  the Linux 1.x code) and window sizing guards (`window_size.rs`, `keep_size`).
- Top tabs exactly as mac: [전환] [네트워크] [장치] [효과] [일반] / Switching, Network,
  Devices, Effects, General. General keeps Linux's setup-check cards, backups, updates,
  Homebrew notes, beta notice.
- Network tab as mac: header row (how-it-works button opening the how-it-works window,
  role badge), consent card until accepted, share switch, problem card, pairing card while
  pairing, then sub-tabs [연결] [이 컴퓨터] [넘기기] [키보드·클립보드] (SEG_SMALL, last
  one remembered in window state, not config) with the same rows, texts and (i) notes as
  mac. Easy-Switch recommendation amber note and [이지스위치 끄기] button (test.3 B), the
  confirm window when turning Logitech edges on while sharing, disabled edge rows with the
  hint while sharing.
- Master/sub: role switch, up to three subs, subs show the master's settings read-only
  where mac does.
- Tray: network state line and items as mac `tray.rs` (ksni backend stays).
- i18n: every new ko/en text from mac's i18n.rs that the Linux UI uses; Linux wording for
  OS-specific lines (no "Accessibility", no Bonjour; mention firewall `ufw` only in the
  README). No em dash characters anywhere.

## D. Packaging, CI, docs

- `.github/workflows/ci.yml`: run on `main` and `v2`; `cargo test --locked`, plus
  `cargo test --locked --manifest-path netcore/Cargo.toml`, plus the X11 smoke tests
  (section E) under `xvfb-run`. `release.yml`: also run netcore tests; asset names
  unchanged. Keep `.github/scripts/build-deps.sh` working (add any new -dev package the
  build needs; arboard X11 needs none beyond libxcb which is already linked by winit at run
  time: verify with `readelf -d` that no new NEEDED library appears, or list it).
- `packaging/linux/install.sh` / `get.sh` / `uninstall.sh` / release notes: same names
  as 1.x; release notes mention network sharing and that GNOME Wayland asks for two
  permissions (input capture, remote desktop).
- README(.ko).md: network sharing section and the support table (X11 send+receive, GNOME
  Wayland send+receive with InputCapture+RemoteDesktop, KDE/other Wayland receive only via
  ConnectToEIS, clipboard on X11 / GNOME via XWayland / data-control compositors), beta
  notice kept. CHANGELOG(.ko).md `## 2.0.0 ` entry like mac's. PRIVACY note like
  windows (local network only between the user's own computers; internet only for
  update checks). The README support table must say plainly what was not tested on real
  hardware.

## E. Checks (all must pass, no warnings)

1. `sh target/check-v2.sh --all-targets` (cross-check from macOS) with zero warnings.
2. `cargo test --offline --manifest-path netcore/Cargo.toml` on the host.
3. Pure-logic tests that can run on macOS (config parsing/migration/edge owner, PNG and
   uri-list helpers, edge_logic, setup_logic, locate_logic) extracted or run the way the
   repo already does (`rustc --test`), listed in the final report with counts.
4. X11 smoke tests marked `#[ignore]` (run in CI with `xvfb-run cargo test -- --ignored`):
   X11 platform `displays()`, `cursor()`/`warp()` round trip, `inject` of a pointer move,
   clipboard text set/get round trip, PNG image round trip, file list round trip.
5. `grep -rn flockswitch2 src packaging docs README* CHANGELOG*` only hits the migration
   code, its test and this spec. `grep -rn $'—'` finds nothing in files you touched.
6. A config written by 0.15.0 (no `[network]`) and one written by the flockswitch2 linux
   test build both load; test for each.
