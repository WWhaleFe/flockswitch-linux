//! Tray icon (StatusNotifierItem) with the settings menu.
//! The engine runs on its own thread; the menu edits the shared config and the
//! engine picks the change up on its next loop.

use crate::engine::Engine;
use crate::i18n::{texts, Texts};
use crate::icon;
use crate::shared::{Shared, Status};
use anyhow::Result;
use auto_launch::{AutoLaunch, AutoLaunchBuilder, LinuxLaunchMode};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[derive(Clone)]
enum Action {
    Enabled,
    Settings,
    CheckUpdate,
    DownloadUpdate,
    ShowDownload,
    ReleasePage,
    Report,
    Quit,
}

struct Ui {
    shared: Arc<Shared>,
    tray: Option<TrayIcon>,
    actions: HashMap<MenuId, Action>,
    shown: (Status, u64),
    uninstall_requested: bool,
    shortcuts: Option<crate::hotkey::Shortcuts>,
}

pub fn run(shared: Arc<Shared>) -> Result<()> {
    // Ctrl+C, logout or shutdown: let the loop below hand the button back and exit.
    let s = shared.clone();
    ctrlc::set_handler(move || s.stop.store(true, std::sync::atomic::Ordering::SeqCst))?;
    let mut engine = Some(spawn_engine(shared.clone()));
    spawn_update_check(shared.clone());
    crate::edge::spawn(shared.clone());

    let mut ui = Ui {
        shared: shared.clone(),
        tray: None,
        actions: HashMap::new(),
        shown: (Status::default(), u64::MAX),
        uninstall_requested: false,
        shortcuts: None,
    };
    // The icon reaches the panel over D-Bus and needs no event loop here. Without a
    // session bus there is no icon: open the settings window so starting flockswitch
    // still shows something (the engine keeps running either way).
    if let Err(e) = ui.create_tray() {
        log::warn!("cannot create the tray icon: {e:#}");
        open_settings_window();
    }
    if crate::hotkey::unsupported_session() {
        log::info!("keyboard shortcuts: not available on Wayland; use the desktop's own shortcuts");
    } else {
        match crate::hotkey::Shortcuts::new(shared.clone()) {
            Ok(s) => ui.shortcuts = Some(s),
            Err(e) => log::warn!("keyboard shortcuts unavailable: {e:#}"),
        }
    }

    loop {
        // Follow the shortcut settings (and let go while the settings window records one).
        if let Some(s) = ui.shortcuts.as_mut() {
            if let Some(failed) = s.sync(&shared.config()) {
                shared.set_status(|st| st.hotkeys_failed = failed);
            }
        }

        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if let Some(action) = ui.actions.get(&ev.id).cloned() {
                if ui.handle(action) {
                    shared.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
        // Uninstall asked for from the settings window; quit from `flockswitch quit`
        // or a newer copy taking over.
        let request = crate::shared::uninstall_request_path();
        if request.exists() {
            let _ = std::fs::remove_file(&request);
            ui.uninstall_requested = true;
            shared.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let request = crate::shared::quit_request_path();
        if request.exists() {
            let _ = std::fs::remove_file(&request);
            shared.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if shared.stopped() {
            if let Some(h) = engine.take() {
                join_briefly(h); // lets the engine hand the button back first
            }
            if ui.uninstall_requested {
                crate::uninstall();
            }
            ui.tray = None;
            return Ok(());
        }

        // Redraw when the engine found/lost devices or switched.
        let now = (shared.status(), shared.version());
        if ui.tray.is_some() && now != ui.shown {
            ui.refresh();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Waits for the engine to finish, but not forever: it may be in the middle of a slow
/// device request, and the UI must not hang at quit or logout.
fn join_briefly(h: JoinHandle<()>) {
    // Long enough for one slow device request plus handing the button back.
    let deadline = Instant::now() + Duration::from_secs(6);
    while !h.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if h.is_finished() {
        let _ = h.join();
    }
}

fn spawn_engine(shared: Arc<Shared>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while !shared.stopped() {
            // A panic must not leave the app looking alive with a dead engine: catch it,
            // let Engine's Drop hand the button back, and start again.
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Engine::new(shared.clone()).and_then(|mut e| e.run())
            }));
            let failure = match run {
                Ok(Ok(())) => break,
                Ok(Err(e)) => format!("{e:#}"),
                Err(_) => "internal error (panic)".to_string(),
            };
            log::error!("engine stopped: {failure}; restarting in 5 s");
            for _ in 0..50 {
                if shared.stopped() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    })
}

pub fn autostart() -> Option<AutoLaunch> {
    let exe = launch_path()?;
    AutoLaunchBuilder::new()
        .set_app_name("flockswitch")
        .set_app_path(&exe.to_string_lossy())
        // ~/.config/autostart/flockswitch.desktop, which every desktop reads.
        .set_linux_launch_mode(LinuxLaunchMode::XdgAutostart)
        .build()
        .ok()
}

/// The program path to start flockswitch by later (login, desktop shortcuts). Homebrew
/// runs it from a folder named after the version, which the next upgrade deletes, so
/// this is the `<prefix>/bin/flockswitch` link instead when there is one.
pub fn launch_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    match brew_link(&exe.to_string_lossy()) {
        Some(link) if link.exists() => Some(link),
        _ => Some(exe),
    }
}

/// `<prefix>/Cellar/flockswitch/<version>/bin/flockswitch` → `<prefix>/bin/flockswitch`.
fn brew_link(exe: &str) -> Option<PathBuf> {
    let i = exe.find("/Cellar/flockswitch/")?;
    Some(PathBuf::from(format!("{}/bin/flockswitch", &exe[..i])))
}

/// White arrows on a blue tile (grey when paused), visible on light and dark panels.
fn tray_icon(enabled: bool) -> Result<Icon> {
    let tile = if enabled { [0x25, 0x63, 0xEB, 255] } else { [0x6B, 0x72, 0x80, 255] };
    Ok(Icon::from_rgba(icon::arrows(48, [255, 255, 255, 255], Some(tile)), 48, 48)?)
}

impl Ui {
    fn create_tray(&mut self) -> Result<()> {
        let menu = self.build_menu();
        let cfg = self.shared.config();
        let builder = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("flockswitch");
        let builder = builder.with_icon(tray_icon(cfg.enabled)?);
        let tray = builder.build()?;
        self.tray = Some(tray);
        self.shown = (self.shared.status(), self.shared.version());
        Ok(())
    }

    fn refresh(&mut self) {
        let menu = self.build_menu();
        let enabled = self.shared.config().enabled;
        if let Some(tray) = &self.tray {
            tray.set_menu(Some(Box::new(menu)));
            if let Ok(icon) = tray_icon(enabled) {
                let _ = tray.set_icon(Some(icon));
            }
        }
        self.shown = (self.shared.status(), self.shared.version());
    }

    fn check(&mut self, text: impl AsRef<str>, checked: bool, enabled: bool, action: Action) -> CheckMenuItem {
        let item = CheckMenuItem::new(text, enabled, checked, None);
        self.actions.insert(item.id().clone(), action);
        item
    }

    fn item(&mut self, text: impl AsRef<str>, action: Action) -> MenuItem {
        let item = MenuItem::new(text, true, None);
        self.actions.insert(item.id().clone(), action);
        item
    }

    fn build_menu(&mut self) -> Menu {
        self.actions.clear();
        let t: &Texts = texts();
        let cfg = self.shared.config();
        let st = self.shared.status();
        let menu = Menu::new();

        let status = if !cfg.enabled {
            format!("○ {}", t.paused)
        } else if let Some(trigger) = &st.trigger {
            format!("● {} — {trigger}", t.running)
        } else {
            format!("◌ {}", t.searching)
        };
        let _ = menu.append(&MenuItem::new(status, false, None));
        if let Some(last) = &st.last_switch {
            let _ = menu.append(&MenuItem::new(format!("{}: {} {last}", t.last_switch, t.channel), false, None));
        }
        let _ = menu.append(&PredefinedMenuItem::separator());

        let enabled = self.check(t.enabled, cfg.enabled, true, Action::Enabled);
        let _ = menu.append(&enabled);
        // Everything else lives in the settings window, which stays open while you
        // change things (a native menu closes after every click).
        let settings = self.item(t.settings, Action::Settings);
        let _ = menu.append(&settings);
        let report = self.item(t.menu_report, Action::Report);
        let _ = menu.append(&report);
        let _ = menu.append(&PredefinedMenuItem::separator());

        // Version and updates.
        use crate::update::UpdateState as U;
        let _ = menu.append(&MenuItem::new(format!("flockswitch {}", env!("CARGO_PKG_VERSION")), false, None));
        let state_line = match &st.update {
            U::Idle => None,
            U::Checking => Some((t.update_checking.to_string(), false)),
            U::UpToDate => Some((format!("✓ {}", t.update_up_to_date), false)),
            U::Available(r) | U::Downloading(r) | U::DownloadFailed(r) => {
                Some((format!("● {} {}", t.update_available, r.version), false))
            }
            U::Downloaded(_) => Some((format!("✓ {}", t.update_downloaded), false)),
            U::Failed => Some((t.menu_check_failed.to_string(), true)),
        };
        if let Some((text, retry)) = state_line {
            let item = if retry { self.item(text, Action::CheckUpdate) } else { MenuItem::new(text, false, None) };
            let _ = menu.append(&item);
        }
        let checking = matches!(st.update, U::Checking);
        let check = MenuItem::new(t.menu_check_update, !checking, None);
        self.actions.insert(check.id().clone(), Action::CheckUpdate);
        let _ = menu.append(&check);
        match &st.update {
            U::Available(_) | U::DownloadFailed(_) => {
                let _ = menu.append(&self.item(t.menu_download, Action::DownloadUpdate));
            }
            U::Downloading(_) => {
                let _ = menu.append(&MenuItem::new(t.update_downloading, false, None));
            }
            U::Downloaded(_) => {
                let _ = menu.append(&self.item(t.menu_show_download, Action::ShowDownload));
            }
            _ => {}
        }
        let _ = menu.append(&self.item(t.menu_release_page, Action::ReleasePage));
        let _ = menu.append(&PredefinedMenuItem::separator());
        let quit = self.item(t.quit, Action::Quit);
        let _ = menu.append(&quit);
        menu
    }

    /// Applies a menu action. Returns true when the app should quit.
    fn handle(&mut self, action: Action) -> bool {
        match action {
            Action::Quit => return true,
            Action::Enabled => {
                if let Err(e) = self.shared.update_config(|c| c.enabled = !c.enabled) {
                    log::info!("setting not changed: {e:#}");
                }
            }
            Action::Settings => open_settings_window(),
            Action::CheckUpdate => {
                let shared = self.shared.clone();
                std::thread::spawn(move || {
                    crate::update::check_into(&shared);
                });
            }
            Action::DownloadUpdate => match self.shared.status().update {
                crate::update::UpdateState::Available(r) | crate::update::UpdateState::DownloadFailed(r) => {
                    crate::update::download_into(self.shared.clone(), r)
                }
                _ => {}
            },
            Action::ShowDownload => {
                if let crate::update::UpdateState::Downloaded(path) = self.shared.status().update {
                    crate::update::reveal(&path);
                }
            }
            Action::ReleasePage => {
                let page = match self.shared.status().update {
                    crate::update::UpdateState::Available(r) => r.page,
                    _ => crate::update::RELEASES_PAGE.to_string(),
                };
                crate::update::open_url(&page);
            }
            Action::Report => {
                let st = self.shared.status();
                let body = crate::update::report_body(&self.shared.config(), &st.devices, st.trigger.as_deref());
                crate::update::open_url(&crate::update::report_mailto(texts().report_subject, &body));
            }
        }
        self.refresh();
        false
    }
}

/// Looks for a new version shortly after start and then once a day, while the
/// setting is on. The result shows in the menu.
fn spawn_update_check(shared: Arc<Shared>) {
    std::thread::spawn(move || {
        let mut wait = Duration::from_secs(20);
        loop {
            let until = Instant::now() + wait;
            while Instant::now() < until {
                if shared.stopped() {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            wait = crate::update::CHECK_INTERVAL;
            // Leave a download in progress or done alone.
            let busy = matches!(
                shared.status().update,
                crate::update::UpdateState::Downloading(_) | crate::update::UpdateState::Downloaded(_)
            );
            if shared.config().check_updates && !busy {
                crate::update::check_into(&shared);
            }
        }
    });
}

/// Starts the settings window as a separate process of this same program.
pub fn open_settings_window() {
    match std::env::current_exe() {
        Ok(exe) => {
            if let Err(e) = std::process::Command::new(exe).arg("settings").spawn() {
                log::warn!("cannot open the settings window: {e}");
            }
        }
        Err(e) => log::warn!("cannot open the settings window: {e}"),
    }
}


#[cfg(test)]
mod tests {
    use super::brew_link;
    use std::path::PathBuf;

    #[test]
    fn homebrew_path_without_the_version() {
        assert_eq!(
            brew_link("/home/linuxbrew/.linuxbrew/Cellar/flockswitch/0.1.1/bin/flockswitch"),
            Some(PathBuf::from("/home/linuxbrew/.linuxbrew/bin/flockswitch"))
        );
        assert_eq!(brew_link("/home/me/.local/bin/flockswitch"), None);
    }
}
