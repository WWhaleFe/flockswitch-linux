//! The setup check, against the real system: gathers what setup_logic.rs needs (files,
//! the session bus, `fc-list`, `dpkg-query`/`rpm -q`/`pacman -Q`, `brew --version`; nothing
//! that needs root or the network), runs a fix the user approved, and in the tray app
//! keeps the menu's count up to date and sends at most one notification per start.
//!
//! An approved fix runs its administrator steps through one `pkexec /bin/sh -c '…'`, so
//! the desktop's own password dialog appears, then its user steps with `/bin/sh -c`.
//! Every line of output goes to the log. Where pkexec is missing, the dialog was
//! cancelled or there is no agent to show it, the settings window shows the same
//! commands with sudo to run in a terminal.

use crate::setup_logic::{self as logic, Brew, Facts, Item, Pkg, Udev};
use crate::shared::Shared;
use crate::update::{InstallKind, UpdateState};
use ashpd::zbus;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What the check takes from the running flockswitch (status file) and the settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inputs {
    pub hid_denied: bool,
    /// A Logitech device was open since start (so device access works). Kept once
    /// seen: the devices come and go with every switch.
    pub devices_open: bool,
    pub edge_backend: String,
    pub edge_note: String,
    /// The window and menu are in Korean.
    pub korean: bool,
    /// A newer release found by the update check (offered for a Homebrew install).
    pub newer_version: Option<String>,
}

/// Looks at this computer and says what is missing.
pub fn detect(inputs: &Inputs) -> Vec<Item> {
    let started = Instant::now();
    let facts = gather(inputs);
    let items = logic::evaluate(&facts);
    let missing: Vec<&str> = items.iter().filter(|i| i.state == logic::State::Missing).map(|i| i.id.key()).collect();
    log::info!(
        "setup check: {} ({} ms)",
        if missing.is_empty() { "nothing missing".to_string() } else { format!("missing {}", missing.join(", ")) },
        started.elapsed().as_millis()
    );
    items
}

fn gather(inputs: &Inputs) -> Facts {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let wayland = crate::edge_wayland::session_is_wayland();
    let bus = |name: &'static str| name_has_owner(name);
    let gnome_shell = bus("org.gnome.Shell").unwrap_or_else(|| logic::is_gnome(&desktop));
    let os = std::fs::read_to_string("/etc/os-release").or_else(|_| std::fs::read_to_string("/usr/lib/os-release")).unwrap_or_default();
    let present: Vec<logic::PackageManager> = logic::PACKAGE_MANAGERS.into_iter().filter(|pm| find_program(pm.program()).is_some()).collect();
    let package_manager = logic::package_manager(&os, &present);

    let mut f = Facts {
        wayland,
        gnome_shell,
        edge_backend: inputs.edge_backend.clone(),
        edge_note: inputs.edge_note.clone(),
        package_manager,
        korean: inputs.korean,
        ..Facts::default()
    };
    // The desktop portal matters only where the edges found no input capture on GNOME.
    let code = inputs.edge_note.split_once(':').map_or(inputs.edge_note.as_str(), |(c, _)| c);
    if wayland && gnome_shell && inputs.edge_backend == "none" && matches!(code, "no-support" | "gnome-old") {
        f.gnome = logic::gnome_in_note(&inputs.edge_note).or_else(crate::edge_wayland::gnome_shell_version_command);
        if let Some(pm) = package_manager.filter(|_| code == "no-support") {
            f.packages = logic::PORTAL_PACKAGES.iter().map(|(name, _)| (name.to_string(), installed(pm, name))).collect();
        }
    }
    f.udev = Udev {
        etc: std::fs::read_to_string(logic::UDEV_RULE_PATH).ok(),
        elsewhere: logic::UDEV_RULE_ELSEWHERE.iter().filter_map(|p| std::fs::read_to_string(p).ok()).collect(),
        other_rule: logic::OTHER_UDEV_RULES.iter().any(|p| Path::new(p).exists()),
        hid_denied: inputs.hid_denied,
        devices_open: inputs.devices_open,
    };
    if inputs.korean {
        f.korean_font = output(Command::new("fc-list").args([":lang=ko", "family"]), Duration::from_secs(15))
            .filter(|(ok, _)| *ok)
            .map(|(_, out)| !out.trim().is_empty());
    }
    if gnome_shell {
        f.sni_watcher = bus("org.kde.StatusNotifierWatcher");
        if f.sni_watcher == Some(false) {
            let mut dirs = vec![PathBuf::from("/usr/share/gnome-shell/extensions"), PathBuf::from("/usr/local/share/gnome-shell/extensions")];
            dirs.extend(dirs::data_dir().map(|d| d.join("gnome-shell/extensions")));
            f.appindicator_installed = dirs.iter().any(|d| d.join(logic::APPINDICATOR_UUID).is_dir());
            f.appindicator_enabled = output(Command::new("gsettings").args(["get", "org.gnome.shell", "enabled-extensions"]), Duration::from_secs(5))
                .filter(|(ok, _)| *ok)
                .map(|(_, out)| logic::gsettings_list_contains(&out, logic::APPINDICATOR_UUID));
        }
    }
    f.homebrew = homebrew(inputs.newer_version.clone());
    f
}

/// Whether `package` is installed, and its version, from the package database.
fn installed(pm: logic::PackageManager, package: &str) -> Pkg {
    let args = pm.query().command(package);
    let Some((program, rest)) = args.split_first() else { return Pkg::Unknown };
    match output(Command::new(program).args(rest), Duration::from_secs(10)) {
        Some((ok, out)) => pm.query().parse(ok, &out),
        None => Pkg::Unknown,
    }
}

fn homebrew(newer: Option<String>) -> Option<Brew> {
    if crate::update::install_kind() != InstallKind::Homebrew {
        return None;
    }
    let brew = brew_program_candidates().into_iter().find(|p| is_executable(p));
    let prefix = brew.as_deref().and_then(|b| b.parent()?.parent()).map(Path::to_path_buf).or_else(brew_prefix);
    let desktop_source = prefix.map(|p| p.join("opt/flockswitch/share/applications/flockswitch.desktop")).filter(|p| p.is_file());
    let data = dirs::data_dir().unwrap_or_else(|| PathBuf::from(".local/share"));
    let apps_dir = data.join("applications");
    // The user's own entry (a link that still points somewhere), or a system one.
    let system_dirs = std::env::var("XDG_DATA_DIRS").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let menu_entry = apps_dir.join("flockswitch.desktop").exists()
        || system_dirs.split(':').filter(|d| !d.is_empty()).any(|d| Path::new(d).join("applications/flockswitch.desktop").exists());
    let needs_trust = match (&newer, &brew) {
        (Some(_), Some(b)) => logic::brew_needs_trust(output(Command::new(b).arg("--version"), Duration::from_secs(20)).and_then(|(_, out)| logic::brew_major(&out))),
        _ => false,
    };
    Some(Brew { brew, desktop_source, apps_dir, menu_entry, newer, needs_trust })
}

/// The running program's path, as started and with links resolved.
fn exe_paths() -> Vec<String> {
    let raw = std::env::current_exe().ok();
    let canonical = raw.as_ref().and_then(|p| std::fs::canonicalize(p).ok());
    [canonical, raw].into_iter().flatten().map(|p| p.to_string_lossy().trim_end_matches(" (deleted)").to_string()).collect()
}

fn brew_program_candidates() -> Vec<PathBuf> {
    let exes = exe_paths();
    let exes: Vec<&str> = exes.iter().map(String::as_str).collect();
    let prefix = std::env::var("HOMEBREW_PREFIX").ok();
    logic::brew_candidates(&exes, prefix.as_deref(), dirs::home_dir().as_deref())
}

/// The Homebrew prefix of the running program.
fn brew_prefix() -> Option<PathBuf> {
    exe_paths().iter().find_map(|e| logic::brew_prefix_from_exe(e))
}

/// `<prefix>/bin/flockswitch`, which follows `brew upgrade`.
pub fn brew_program() -> Option<PathBuf> {
    brew_prefix().map(|p| p.join("bin/flockswitch")).filter(|p| is_executable(p))
}

/// The version Homebrew has installed now (`<prefix>/opt/flockswitch` → its Cellar folder).
pub fn brew_installed_version() -> Option<String> {
    let opt = brew_prefix()?.join("opt/flockswitch");
    logic::cellar_version(&std::fs::canonicalize(opt).ok()?)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// A program in the usual system folders (or on PATH).
pub fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin", "/usr/local/bin"]
        .into_iter()
        .map(String::from)
        .chain(path.split(':').filter(|d| d.starts_with('/')).map(String::from))
        .map(|d| Path::new(&d).join(name))
        .find(|p| is_executable(p))
}

/// Runs a quick command and returns (succeeded, stdout); None when it could not run or
/// took longer than `limit`.
fn output(cmd: &mut Command, limit: Duration) -> Option<(bool, String)> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait().ok()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Some((status.success(), reader.join().ok()?))
}

/// Whether `name` has an owner on the session bus (None: no session bus).
fn name_has_owner(name: &'static str) -> Option<bool> {
    async_io::block_on(crate::edge_wayland::within(Duration::from_secs(5), async move {
        let conn = zbus::Connection::session().await.ok()?;
        let proxy = zbus::Proxy::new(&conn, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus").await.ok()?;
        proxy.call::<_, _, bool>("NameHasOwner", &(name,)).await.ok()
    }))
    .flatten()
}

// ---- Running an approved fix -------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// The administrator steps did not run: no pkexec, the password dialog was
    /// cancelled, or there is no agent to show one.
    NoAdmin,
    Failed { code: Option<i32>, tail: Vec<String> },
}

/// Lines kept for the settings window from a failed fix.
const TAIL: usize = 8;

/// Runs the fix of `item`. Only ever called after the user pressed Run.
pub fn run(item: &Item) -> Outcome {
    let Some(fix) = &item.fix else { return Outcome::Done };
    let label = item.id.key();
    log::info!("setup {label}: approved, running");
    if fix.needs_admin() {
        let Some(pkexec) = find_program("pkexec") else {
            log::info!("setup {label}: pkexec not found; the commands are shown to run with sudo");
            return Outcome::NoAdmin;
        };
        let mut cmd = Command::new(pkexec);
        cmd.arg("/bin/sh").arg("-c").arg(fix.admin_script());
        let (status, lines) = stream(cmd, label);
        if !lines.iter().any(|l| l.trim() == logic::ADMIN_MARKER) {
            log::info!("setup {label}: not authorized (exit {:?}); the commands are shown to run with sudo", status.and_then(|s| s.code()));
            return Outcome::NoAdmin;
        }
        if !status.is_some_and(|s| s.success()) {
            return Outcome::Failed { code: status.and_then(|s| s.code()), tail: tail(&lines) };
        }
    }
    if !fix.user.is_empty() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(fix.user_script());
        let (status, lines) = stream(cmd, label);
        if !status.is_some_and(|s| s.success()) {
            return Outcome::Failed { code: status.and_then(|s| s.code()), tail: tail(&lines) };
        }
    }
    log::info!("setup {label}: done");
    Outcome::Done
}

fn tail(lines: &[String]) -> Vec<String> {
    let useful: Vec<&String> = lines.iter().filter(|l| !l.trim().is_empty() && l.trim() != logic::ADMIN_MARKER).collect();
    useful[useful.len().saturating_sub(TAIL)..].iter().map(|l| l.to_string()).collect()
}

/// Runs `cmd`, writing each line it prints (stdout and stderr) to the log as it comes.
fn stream(mut cmd: Command, label: &str) -> (Option<ExitStatus>, Vec<String>) {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("setup {label}: cannot start: {e}");
            return (None, vec![e.to_string()]);
        }
    };
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let pipes: Vec<Box<dyn Read + Send>> = [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .collect();
    let readers: Vec<_> = pipes
        .into_iter()
        .map(|pipe| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    let _ = tx.send(line);
                }
            })
        })
        .collect();
    drop(tx);
    let mut lines = Vec::new();
    for line in rx {
        log::info!("setup {label}: {line}");
        lines.push(line);
        if lines.len() > 400 {
            lines.drain(..200);
        }
    }
    for r in readers {
        let _ = r.join();
    }
    let status = child.wait().ok();
    log::info!("setup {label}: exit {:?}", status.and_then(|s| s.code()));
    (status, lines)
}

// ---- The tray app: the menu's count and one notification ----------------------------

/// Korean is chosen but no font can show it.
pub fn korean_unreadable(items: &[Item]) -> bool {
    items.iter().any(|i| i.id == logic::ItemId::KoreanFont && i.state == logic::State::Missing)
}

/// The texts to use: English while Korean has no font to show it in.
pub fn readable_texts(items: &[Item]) -> &'static crate::i18n::Texts {
    if korean_unreadable(items) { crate::i18n::english() } else { crate::i18n::texts() }
}

fn tray_inputs(shared: &Shared, devices_seen: &mut bool) -> Inputs {
    let st = shared.status();
    *devices_seen |= st.devices.iter().any(|d| d.online);
    let newer = match st.update {
        UpdateState::Available(r) | UpdateState::DownloadFailed(r) | UpdateState::InstallFailed(r) => Some(r.version),
        _ => None,
    };
    Inputs {
        hid_denied: st.hid_denied,
        devices_open: *devices_seen,
        edge_backend: st.edge_backend,
        edge_note: st.edge_note,
        korean: crate::i18n::is_korean(),
        newer_version: newer,
    }
}

/// In the tray app: checks once what the session has settled on (the edges' probe and
/// the first update check, at most 45 s after start), then whenever what it depends on
/// changes or the settings window asks (a fix ran, an item was set aside). Keeps the
/// count for the menu, and sends one notification at the first check when something is
/// missing; later only for a new Homebrew version, with "Install updates automatically"
/// on (a Homebrew copy is never updated without approval).
pub fn spawn_watch(shared: Arc<Shared>) {
    let started = std::thread::Builder::new().name("setup".into()).spawn(move || {
        let start = Instant::now();
        while !shared.stopped() && start.elapsed() < Duration::from_secs(45) {
            let (st, cfg) = (shared.status(), shared.config());
            let edges_known = !st.edge_backend.is_empty();
            let update_known = !cfg.check_updates || !matches!(st.update, UpdateState::Idle | UpdateState::Checking);
            if start.elapsed() >= Duration::from_secs(5) && edges_known && update_known {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        let mut last = None;
        let mut first = true;
        let mut told_version: Option<String> = None;
        let mut devices_seen = false;
        while !shared.stopped() {
            let cfg = shared.config();
            let now = (tray_inputs(&shared, &mut devices_seen), cfg.setup.snoozed.clone());
            if crate::shared::take_setup_recheck() || last.as_ref() != Some(&now) {
                let items = detect(&now.0);
                let pending = logic::pending(&items, &now.1);
                let update = logic::offered_update(&items, &now.1).filter(|_| cfg.auto_update);
                let english = korean_unreadable(&items);
                shared.set_status(|s| {
                    s.setup_pending = pending;
                    s.setup_english = english;
                });
                let t = readable_texts(&items);
                // At the first check one notification says it all; afterwards only a
                // version not told about yet.
                let fresh_update = update.filter(|v| told_version.as_ref() != Some(v));
                let mut lines = Vec::new();
                if first && pending > 0 {
                    lines.push(t.notify_setup.replace("{n}", &pending.to_string()));
                }
                if let Some(v) = &fresh_update {
                    lines.push(t.notify_update.replace("{version}", v));
                }
                if !lines.is_empty() {
                    notify(&lines.join("\n"), t.notify_open);
                }
                if fresh_update.is_some() {
                    told_version = fresh_update;
                }
                first = false;
                last = Some(now);
            }
            for _ in 0..20 {
                if shared.stopped() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    });
    if let Err(e) = started {
        log::warn!("setup check: cannot start its thread: {e}");
    }
}

/// A desktop notification (best effort). Clicking it, or its button, opens the
/// settings window on the General tab.
fn notify(body: &str, open: &str) {
    let (body, open) = (body.to_string(), open.to_string());
    std::thread::spawn(move || {
        let result = async_io::block_on(async {
            use futures_util::StreamExt;
            let conn = zbus::Connection::session().await?;
            let proxy = zbus::Proxy::new(&conn, "org.freedesktop.Notifications", "/org/freedesktop/Notifications", "org.freedesktop.Notifications").await?;
            let mut invoked = std::pin::pin!(proxy.receive_signal("ActionInvoked").await?);
            let mut closed = std::pin::pin!(proxy.receive_signal("NotificationClosed").await?);
            let mut hints = std::collections::HashMap::new();
            hints.insert("desktop-entry", zbus::zvariant::Value::from("flockswitch"));
            let actions = vec!["default", open.as_str(), "open", open.as_str()];
            let id: u32 = proxy.call("Notify", &("flockswitch", 0u32, "", "flockswitch", body.as_str(), actions, hints, -1i32)).await?;
            log::info!("notification {id}: {}", body.replace('\n', " / "));
            let wait = async {
                loop {
                    match futures_util::future::select(invoked.next(), closed.next()).await {
                        futures_util::future::Either::Left((Some(msg), _)) => {
                            if msg.body().deserialize::<(u32, String)>().is_ok_and(|(n, _)| n == id) {
                                return true;
                            }
                        }
                        futures_util::future::Either::Right((Some(msg), _)) => {
                            if msg.body().deserialize::<(u32, u32)>().is_ok_and(|(n, _)| n == id) {
                                return false;
                            }
                        }
                        _ => return false,
                    }
                }
            };
            Ok::<bool, zbus::Error>(crate::edge_wayland::within(Duration::from_secs(6 * 3600), wait).await.unwrap_or(false))
        });
        match result {
            Ok(true) => crate::tray::open_settings_page("setup"),
            Ok(false) => {}
            Err(e) => log::info!("cannot show a notification: {e}"),
        }
    });
}
