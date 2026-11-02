//! Keyboard shortcuts that send the group to a channel. They are grabbed from the X
//! server, so they work from any keyboard, need no extra permission, and flockswitch
//! sees only its own shortcuts. Wayland lets no app grab keys for the whole desktop:
//! there the desktop's own shortcut settings run `flockswitch switch <N>` instead.

use crate::config::Config;
use crate::shared::Shared;
use anyhow::{anyhow, Result};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Reads a shortcut as written in the settings, e.g. "ctrl+alt+1".
pub fn parse(text: &str) -> Result<HotKey> {
    text.parse::<HotKey>().map_err(|e| anyhow!("shortcut '{text}': {e}"))
}

/// How a shortcut is shown: ⌃⌥1 on macOS, Ctrl+Alt+1 elsewhere.
pub fn display(text: &str) -> String {
    let parts: Vec<&str> = text.split('+').map(str::trim).collect();
    let has = |names: &[&str]| parts.iter().any(|p| names.iter().any(|n| p.eq_ignore_ascii_case(n)));
    let mods = [
        (has(&["ctrl", "control"]), "⌃", "Ctrl"),
        (has(&["alt", "option"]), "⌥", "Alt"),
        (has(&["shift"]), "⇧", "Shift"),
        (has(&["super", "cmd", "command", "meta"]), "⌘", "Super"),
    ];
    let key = parts.last().map(|k| k.to_uppercase()).unwrap_or_default();
    if cfg!(target_os = "macos") {
        let mut out: String = mods.iter().filter(|m| m.0).map(|m| m.1).collect();
        out.push_str(&key);
        out
    } else {
        let mut out: Vec<&str> = mods.iter().filter(|m| m.0).map(|m| m.2).collect();
        out.push(&key);
        out.join("+")
    }
}

/// A Wayland session: global key grabs only see keys typed into X11 windows there, so
/// flockswitch does not register any (the settings window explains the alternative).
pub fn unsupported_session() -> bool {
    let var = |name: &str| std::env::var(name).unwrap_or_default();
    var("XDG_SESSION_TYPE").eq_ignore_ascii_case("wayland") || !var("WAYLAND_DISPLAY").is_empty()
}

/// While the settings window records a new shortcut it keeps this file fresh, and the
/// tray app lets go of its shortcuts, so pressing one records it instead of switching.
fn pause_path() -> std::path::PathBuf {
    Config::path().with_file_name("shortcuts.pause")
}

pub fn hold_pause() {
    let _ = std::fs::write(pause_path(), b"");
}

pub fn end_pause() {
    let _ = std::fs::remove_file(pause_path());
}

/// A pause left behind by a settings window that closed mid-recording runs out.
fn paused() -> bool {
    std::fs::metadata(pause_path())
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().unwrap_or_default() < Duration::from_secs(3))
}

/// The shortcuts registered with the system. Lives on the tray app's main thread.
pub struct Shortcuts {
    manager: GlobalHotKeyManager,
    registered: Vec<HotKey>,
    /// Shortcut id → channel, read by the system's callback.
    channels: Arc<Mutex<HashMap<u32, u8>>>,
    current: Vec<(u8, String)>,
}

impl Shortcuts {
    pub fn new(shared: Arc<Shared>) -> Result<Shortcuts> {
        let manager = GlobalHotKeyManager::new()?;
        let channels: Arc<Mutex<HashMap<u32, u8>>> = Arc::default();
        let map = channels.clone();
        // Called as the key goes down, so the switch does not wait for the menu loop.
        GlobalHotKeyEvent::set_event_handler(Some(move |ev: GlobalHotKeyEvent| {
            if ev.state == HotKeyState::Pressed {
                if let Some(&channel) = map.lock().unwrap().get(&ev.id) {
                    shared.request_switch(channel);
                }
            }
        }));
        Ok(Shortcuts { manager, registered: Vec::new(), channels, current: Vec::new() })
    }

    /// Registers what the settings ask for. Returns the channels whose shortcut could
    /// not be registered (usually taken by another app) when anything changed.
    pub fn sync(&mut self, cfg: &Config) -> Option<Vec<u8>> {
        let wanted: Vec<(u8, String)> = if cfg.enabled && cfg.hotkeys.on && !paused() {
            (1..=3).map(|n| (n, cfg.hotkeys.key(n).to_string())).filter(|(_, k)| !k.is_empty()).collect()
        } else {
            Vec::new()
        };
        if wanted == self.current {
            return None;
        }
        let _ = self.manager.unregister_all(&self.registered);
        self.registered.clear();
        let mut map = self.channels.lock().unwrap();
        map.clear();
        let mut failed = Vec::new();
        for (n, text) in &wanted {
            match parse(text).and_then(|hk| self.manager.register(hk).map(|_| hk).map_err(Into::into)) {
                Ok(hk) => {
                    map.insert(hk.id(), *n);
                    self.registered.push(hk);
                }
                Err(e) => {
                    log::warn!("channel {n} shortcut not registered: {e:#}");
                    failed.push(*n);
                }
            }
        }
        if !wanted.is_empty() {
            let list: Vec<String> = wanted.iter().map(|(n, k)| format!("{} → {n}", display(k))).collect();
            log::info!("shortcuts: {}", list.join(", "));
        }
        self.current = wanted;
        Some(failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_shows() {
        assert!(parse("ctrl+alt+1").is_ok());
        assert!(parse("ctrl+alt+f5").is_ok());
        assert!(parse("ctrl+alt+").is_err());
        let shown = display("ctrl+alt+shift+k");
        if cfg!(target_os = "macos") {
            assert_eq!(shown, "⌃⌥⇧K");
        } else {
            assert_eq!(shown, "Ctrl+Alt+Shift+K");
        }
    }
}
