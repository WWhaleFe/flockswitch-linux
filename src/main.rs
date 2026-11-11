//! flockswitch — use a spare Logitech mouse button as an Easy-Switch key that moves the
//! mouse and its companion keyboard to another computer together.

mod backup;
mod config;
mod device;
mod edge;
mod edge_layer;
mod edge_logic;
mod edge_portal;
mod edge_wayland;
mod engine;
mod hidpp;
mod hotkey;
mod i18n;
mod icon;
mod overlay;
mod overlay_render;
mod overlay_wayland;
mod settings_ui;
mod shared;
mod tray;
mod update;
mod window_size;
mod wl;

use anyhow::{bail, Result};
use config::Config;
use engine::Engine;
use shared::Shared;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

const USAGE: &str = "\
flockswitch — move your Logitech mouse and keyboard to another computer with one button

USAGE:
    flockswitch              start in the system tray (default)
    flockswitch settings     open the settings window
    flockswitch run          run without an icon, logging to the terminal
    flockswitch list         show devices, channels and the trigger button
    flockswitch switch <N>   move all following devices to channel N (1-3) now; asks the
                            running flockswitch if there is one (bind this to a desktop
                            shortcut on Wayland)
    flockswitch config       print the config file path (created on first use)
    flockswitch quit         ask the running flockswitch to give the button back and quit
    flockswitch uninstall    remove start-at-login and all settings (also quits the running
                            flockswitch), then delete the program file/folder yourself
    flockswitch set <key> <value>
                            change a setting, e.g.
                              set buttons wheel_mode,gesture   buttons that switch (also: middle)
                              set channels 1,2      channels to cycle through (every button;
                                                    one channel = always go there)
                              set gesture.channels 3           one button: wheel_mode / gesture / middle
                              set follow all        devices that move along (all, none, or B366,MX Keys)
                              set trigger_device auto
                              set enabled true|false
                              set hotkeys true      keyboard shortcuts on (any keyboard)
                              set hotkey.2 ctrl+alt+2          shortcut for channel 2 (none = off)
                              set edges true        switch when the cursor rests at a screen edge
                              set edge.right 2      the channel of that edge (left/right/top/bottom; none = off)
                              set edge.dwell 500    how long the cursor must rest there (ms)
                              set edge.cursor true  put the cursor at that edge when the mouse comes back
                              set edge.control true wait only while Ctrl is held
                              set edge.anim ring    none, ring, semicircle, bar, pulse, laser, vortex, scan, liquid, random
                              set edge.ring.color #FFCC00      each effect's own colour (ring, semicircle, bar,
                                                    pulse, laser, vortex, scan, liquid)
                              set edge.bar.size 125            its size in percent (50-200)
                              set edge.ring.direction both     clockwise, counterclockwise, both (ring, semicircle, bar)
                              set edge.ring.shrink true        shrink instead of draining (ring, semicircle)
                              set edge.anim_color #FFCC00      also anim_size, anim_direction, anim_shrink: the
                                                    chosen effect's (every effect's for none or random)
                              set haptics.switch true          a pulse as the mouse leaves (mice with a motor)
                              set haptics.arrive true          a pulse when the mouse is back
                              set haptics.edge true            pulses while waiting at a screen edge
                              set haptics.progressive true     those pulses get faster and stronger
                              set haptics.intensity 75         strength 1-100% (0 = as Logi Options+ sets it)
                              set haptics.switch_waveform 7    pulse 0-14, 27 or random (also arrive_waveform;
                                                    edge_waveform is the even pattern at an edge)
                              set locate true       show where the cursor is
                              set locate.style ring            ring, radar, spotlight, random
                              set locate.color #FFCC00         the colour of the highlight
                              set locate.size 125              its size in percent (50-200)
                              set locate.hotkey ctrl+alt+l     its shortcut (none = off)
                              set locate.arrival true          when the mouse arrives from another computer
                              set locate.shake true            when the mouse is shaken
                              set locate.shake_time 1000       for this long (ms) without a pause
                              set locate.shake_key ctrl        only while this key is held (none, ctrl, alt, shift)
                              set check_updates true           look for a new version at start and daily
                              set auto_update true             install it and restart by itself
    flockswitch help

Set RUST_LOG=debug for more detail.";

/// Port used only as a lock so two copies never react to the same button press.
const INSTANCE_PORT: u16 = 47_851;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The long-running modes take the single-instance lock before touching the log, so
    // a second copy does not wipe the running copy's log file.
    let tray_mode = args.is_empty() || args[0] == "tray";
    let _lock = if tray_mode || args[0] == "run" {
        match single_instance() {
            Some(lock) => Some(lock),
            None if tray_mode => match already_running() {
                Some(lock) => Some(lock),
                None => std::process::exit(0),
            },
            None => {
                eprintln!("error: {}", i18n::texts().already_running);
                std::process::exit(1);
            }
        }
    } else {
        None
    };
    Config::migrate_old_name();
    // The settings window has no console when started from the menu: log to its own
    // file so a window that fails to open (graphics drivers) leaves a trace.
    let log_file = if tray_mode {
        Some(log_path())
    } else if args.first().map(String::as_str) == Some("settings") {
        Some(log_path().with_file_name("settings.log"))
    } else {
        None
    };
    init_logging(log_file);
    // A panic (for example in a graphics driver) is written to the log, not only to a
    // console nobody sees.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("panic: {info}");
        default_hook(info);
    }));
    if let Err(e) = real_main(&args) {
        log::error!("{e:#}");
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main(args: &[String]) -> Result<()> {
    let cmd = args.first().map(String::as_str).unwrap_or("tray");
    match cmd {
        // `tray --settings`: started by the settings window after it installed an update.
        "tray" => tray::run(Arc::new(Shared::new(load_config())), args.get(1).map(String::as_str) == Some("--settings")),
        "run" => run_headless(),
        "settings" => {
            // One settings window at a time. Opening it again knocks on the open
            // window's port (nothing is sent), which brings that window to the front.
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], INSTANCE_PORT + 1));
            let Some(lock) = std::net::TcpListener::bind(addr).ok() else {
                let _ = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(1));
                return Ok(());
            };
            // `settings update` opens on the General tab, where the new version is.
            let update = args.get(1).map(String::as_str) == Some("update");
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| settings_ui::run(lock, update)))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("the settings window crashed")));
            match result {
                // Nothing left to try: say so instead of failing silently.
                Err(e) => {
                    let t = i18n::texts();
                    let log = log_path().with_file_name("settings.log");
                    show_message(&format!("{}\n\n{e:#}\n\n{} {}", t.settings_failed, t.details_in, log.display()));
                    Err(e)
                }
                ok => ok,
            }
        }
        "list" => list(),
        "switch" => {
            let Some(n) = args.get(1).and_then(|s| s.parse::<u8>().ok()).filter(|n| (1..=3).contains(n)) else {
                bail!("usage: flockswitch switch <1|2|3>");
            };
            if main_instance_running() {
                return ask_running_copy_to_switch(n);
            }
            let mut engine = Engine::new(Arc::new(Shared::new(Config::load_or_create()?)))?;
            engine.scan()?;
            engine.switch_to(n)
        }
        "quit" => {
            if main_instance_running() {
                ask_running_copy(&shared::quit_request_path())?;
            }
            Ok(())
        }
        "uninstall" => {
            // A running tray app owns the button: it hands it back and uninstalls itself.
            if main_instance_running() {
                ask_running_copy(&shared::uninstall_request_path())?;
            } else {
                uninstall();
            }
            println!("Settings and start-at-login removed. Delete the flockswitch program to finish.");
            Ok(())
        }
        "config" => {
            Config::load_or_create()?;
            println!("{}", Config::path().display());
            Ok(())
        }
        "set" => {
            let (Some(key), Some(value)) = (args.get(1), args.get(2)) else {
                bail!("usage: flockswitch set <key> <value>  (see 'flockswitch help')");
            };
            let mut cfg = Config::load_or_create()?;
            cfg.set(key, value)?;
            cfg.save()?;
            println!("{key} = {value}  ({})", Config::path().display());
            Ok(())
        }
        "help" | "-h" | "--help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => bail!("unknown command '{other}'\n\n{USAGE}"),
    }
}

fn run_headless() -> Result<()> {
    let cfg = load_config();
    if !cfg.enabled {
        log::info!("disabled in {}", Config::path().display());
    }
    let shared = Arc::new(Shared::new(cfg));
    let s = shared.clone();
    ctrlc::set_handler(move || s.stop.store(true, Ordering::SeqCst))?;
    edge_wayland::spawn(shared.clone());
    Engine::new(shared)?.run()
}

fn list() -> Result<()> {
    let cfg = Config::load_or_create()?;
    let mut engine = Engine::new(Arc::new(Shared::new(cfg.clone())))?;
    engine.scan()?;
    engine.describe();
    println!();
    for d in engine.devices().to_vec() {
        let ep = engine.endpoint(d.endpoint);
        let host = d
            .host_state(ep)?
            .map(|s| format!("channel {}/{}", s.current + 1, s.count))
            .unwrap_or_else(|| "not on this computer".into());
        // The trigger buttons this device has; "*" marks ones diverted right now, "+xy"
        // ones whose movement during a hold is diverted too (gestures).
        let buttons: Vec<String> = config::BUTTONS
            .iter()
            .filter_map(|&cid| match d.control_reporting(ep, cid) {
                Ok(Some(r)) => Some(format!(
                    "0x{cid:04X}{}{}",
                    if r.diverted { "*" } else { "" },
                    if r.raw_xy { "+xy" } else { "" }
                )),
                _ => None,
            })
            .collect();
        let trig = if buttons.is_empty() { String::new() } else { format!(", buttons {}", buttons.join(" ")) };
        // Haptic waveforms the device offers (MX Master 4), for the haptics.*_waveform settings.
        let haptics = match d.haptic_waveforms(ep) {
            Ok(Some(mask)) => {
                let ids: Vec<String> = (0..32).filter(|&i| mask & (1u32 << i) != 0).map(|i| i.to_string()).collect();
                format!(", haptic waveforms {}", ids.join(","))
            }
            _ => String::new(),
        };
        let kind = d.kind.map(|k| format!("{k:?}")).unwrap_or_else(|| "?".into());
        println!("  slot {:>3}  {:<28} {:<9} {host}{trig}{haptics}", d.index, d.label(), kind);
    }
    Ok(())
}

/// Loads the settings. A file that cannot be read is kept as config.invalid.toml and
/// the defaults are used, so a typo never stops the app from starting.
fn load_config() -> Config {
    match Config::load_or_create() {
        Ok(cfg) => cfg,
        Err(e) => {
            let path = Config::path();
            let backup = path.with_file_name("config.invalid.toml");
            log::warn!("{e:#}; using defaults (your file is kept as {})", backup.display());
            let _ = std::fs::rename(&path, &backup);
            let cfg = Config::default();
            let _ = cfg.save();
            cfg
        }
    }
}

/// Started while the menu-bar app already runs. Starting it again is how people look
/// for it, so the same version opens the settings window. A different version (for
/// example an older one still set to start at login) is asked to quit, and this copy
/// takes over, so the version the user just started is the one that runs.
fn already_running() -> Option<std::net::TcpListener> {
    let running = shared::load_status_file().version;
    if running == env!("CARGO_PKG_VERSION") {
        tray::open_settings_window();
        return None;
    }
    let request = shared::quit_request_path();
    if let Some(dir) = request.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&request, b"");
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Some(lock) = single_instance() {
            return Some(lock);
        }
    }
    let _ = std::fs::remove_file(&request);
    show_message(i18n::texts().old_copy_running);
    None
}

/// A message the user sees: a dialog (a tray app has no terminal), and stderr. Uses
/// whichever dialog tool the desktop has, and a notification as the last resort.
pub fn show_message(text: &str) {
    eprintln!("{text}");
    let tries: [(&str, Vec<&str>); 3] = [
        ("zenity", vec!["--warning", "--title=flockswitch", "--no-markup", "--text", text]),
        ("kdialog", vec!["--title", "flockswitch", "--sorry", text]),
        ("notify-send", vec!["--app-name=flockswitch", "flockswitch", text]),
    ];
    for (cmd, args) in tries {
        if std::process::Command::new(cmd).args(&args).status().is_ok() {
            return;
        }
    }
}

/// `flockswitch switch <N>` while flockswitch runs: the running copy already has the
/// devices open and moves them at once, instead of this one scanning them all again.
fn ask_running_copy_to_switch(channel: u8) -> Result<()> {
    let request = shared::switch_request_path();
    if let Some(dir) = request.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Written whole, then renamed, so the running copy never reads half a file.
    let tmp = request.with_extension("tmp");
    std::fs::write(&tmp, channel.to_string())?;
    std::fs::rename(&tmp, &request)?;
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if !request.exists() {
            return Ok(());
        }
    }
    let _ = std::fs::remove_file(&request);
    bail!("the running flockswitch did not answer; quit it and try again")
}

/// Leaves a request file for the running tray app and waits for it to quit.
fn ask_running_copy(request: &std::path::Path) -> Result<()> {
    if let Some(dir) = request.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(request, b"")?;
    for _ in 0..150 {
        if !main_instance_running() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = std::fs::remove_file(request);
    bail!("flockswitch did not quit; quit it from the menu (or stop `flockswitch run`) and try again")
}

/// True when another copy (the tray app or `run`) holds the instance lock.
pub fn main_instance_running() -> bool {
    std::net::TcpListener::bind(("127.0.0.1", INSTANCE_PORT)).is_err()
}

/// True while a settings window is open (it holds the port after the instance lock).
pub fn settings_window_open() -> bool {
    std::net::TcpListener::bind(("127.0.0.1", INSTANCE_PORT + 1)).is_err()
}

/// Holds a loopback port for the life of the process; a second copy fails to bind.
fn single_instance() -> Option<std::net::TcpListener> {
    std::net::TcpListener::bind(("127.0.0.1", INSTANCE_PORT)).ok()
}

/// Removes everything flockswitch created outside its own program file: the login
/// item and the settings folder (config, remembered devices, log). The trigger
/// button is already back to normal because the engine releases it when it stops.
pub fn uninstall() {
    if let Some(al) = tray::autostart() {
        if al.is_enabled().unwrap_or(false) {
            if let Err(e) = al.disable() {
                log::warn!("could not remove start at login: {e}");
            }
        }
    }
    if let Some(dir) = Config::path().parent() {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
        let _ = std::fs::remove_dir(dir);
    }
    log::info!("uninstalled settings and start-at-login");
}

pub fn log_path() -> PathBuf {
    Config::path().with_file_name("flockswitch.log")
}

/// flockswitch.log → flockswitch.previous.log.
fn previous_log(path: &std::path::Path) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!("{stem}.previous.log"))
}

/// The tray app has no terminal, so it logs to a file next to the config.
fn init_logging(file: Option<PathBuf>) {
    let mut builder = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    builder.format_timestamp_secs();
    if let Some(path) = file {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Start a fresh log each launch so it never grows without bound. The previous
        // run's is kept beside it (after an update, that is the one that installed it).
        let _ = std::fs::rename(&path, previous_log(&path));
        if let Ok(file) = std::fs::File::create(&path) {
            builder.target(env_logger::Target::Pipe(Box::new(file)));
        }
    }
    builder.init();
}
