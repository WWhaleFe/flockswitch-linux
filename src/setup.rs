//! The setup check, against the real system: gathers what setup_logic.rs needs (files,
//! the session bus, `fc-list`, `dpkg-query`/`rpm -q`/`pacman -Q`, `brew --version`; nothing
//! that needs root or the network), runs a fix the user approved, and in the tray app
//! keeps the menu's count up to date and sends at most one notification per start.
//!
//! An approved fix runs its administrator steps through one `pkexec /bin/sh -c '…'`, so
//! the desktop's own password dialog appears, then its user steps with `/bin/sh -c`;
//! several fixes approved together ("Install all") share that one script and password.
//! Every line of output goes to the log. Where pkexec is missing, the dialog was
//! cancelled or there is no agent to show it, the settings window shows the same
//! commands with sudo to run in a terminal.
//!
//! `flockswitch setup` is the same check in a terminal (`cli`): the list, the commands,
//! one question, then one `sudo /bin/sh -c '…'` for everything.

use crate::config::Config;
use crate::i18n::Texts;
use crate::setup_logic::{self as logic, AdminTool, Brew, Facts, Item, ItemId, Pkg, Udev, Why};
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

pub use crate::setup_logic::Outcome;

/// Lines kept for the settings window from a failed fix.
const TAIL: usize = 8;

/// Runs the fixes of `items` (one, or several approved together: "Install all",
/// `flockswitch setup`): every administrator step in one script through one `tool` (one
/// password), then each item's user steps, for the items whose administrator steps
/// went through. `echo` also prints what the commands print (in a terminal). Only ever
/// called after the user approved.
pub fn run_all(items: &[&Item], tool: AdminTool, echo: bool) -> Vec<(ItemId, Outcome)> {
    let items: Vec<&Item> = items.iter().copied().filter(|i| i.fix.is_some()).collect();
    let label = items.iter().map(|i| i.id.key()).collect::<Vec<_>>().join("+");
    log::info!("setup {label}: approved, running");
    let admin: Vec<&Item> = items.iter().copied().filter(|i| i.needs_admin()).collect();
    let mut admin_outcome: Vec<(ItemId, Outcome)> = Vec::new();
    if !admin.is_empty() {
        // One item keeps its own script (stops at its first failing step); several get
        // the combined one, which says which of them failed.
        let script = match admin.as_slice() {
            [one] => one.fix.as_ref().map(logic::Fix::admin_script).unwrap_or_default(),
            many => logic::combined_admin_script(many),
        };
        let failed_all = |outcome: Outcome| admin.iter().map(|i| (i.id, outcome.clone())).collect::<Vec<_>>();
        match find_program(tool.program()) {
            None => {
                log::info!("setup {label}: {} not found; the commands are shown to run by hand", tool.program());
                admin_outcome = failed_all(Outcome::NoAdmin);
            }
            Some(program) => {
                log::info!("setup {label}: administrator script ({}):\n{script}", tool.program());
                let mut cmd = Command::new(program);
                cmd.arg("/bin/sh").arg("-c").arg(&script);
                let (status, lines) = stream(cmd, &label, echo);
                let code = status.and_then(|s| s.code());
                if !lines.iter().any(|l| l.trim() == logic::ADMIN_MARKER) {
                    log::info!("setup {label}: not authorized (exit {code:?}); the commands are shown to run by hand");
                    admin_outcome = failed_all(Outcome::NoAdmin);
                } else {
                    let ok = status.is_some_and(|s| s.success());
                    let failed: Vec<String> = match admin.as_slice() {
                        [one] if !ok => vec![one.id.key().to_string()],
                        [_] => vec![],
                        // No list printed (the script was cut short): all of them, unless it succeeded.
                        _ => logic::failed_keys(&lines).unwrap_or_else(|| if ok { vec![] } else { admin.iter().map(|i| i.id.key().to_string()).collect() }),
                    };
                    for item in &admin {
                        if failed.iter().any(|k| k == item.id.key()) {
                            admin_outcome.push((item.id, Outcome::Failed { code, tail: tail(&lines) }));
                        }
                    }
                }
            }
        }
    }
    let mut outcomes = Vec::new();
    for item in items {
        let key = item.id.key();
        if let Some((_, o)) = admin_outcome.iter().find(|(id, _)| *id == item.id) {
            outcomes.push((item.id, o.clone()));
            continue;
        }
        let Some(fix) = &item.fix else { continue };
        if !fix.user.is_empty() {
            let mut cmd = Command::new("/bin/sh");
            cmd.arg("-c").arg(fix.user_script());
            let (status, lines) = stream(cmd, key, echo);
            if !status.is_some_and(|s| s.success()) {
                outcomes.push((item.id, Outcome::Failed { code: status.and_then(|s| s.code()), tail: tail(&lines) }));
                continue;
            }
        }
        log::info!("setup {key}: done");
        outcomes.push((item.id, Outcome::Done));
    }
    outcomes
}

/// A line the scripts print for flockswitch itself, not for the user.
fn is_marker(line: &str) -> bool {
    line.trim_start().starts_with("[flockswitch] ")
}

fn tail(lines: &[String]) -> Vec<String> {
    let useful: Vec<&String> = lines.iter().filter(|l| !l.trim().is_empty() && !is_marker(l)).collect();
    useful[useful.len().saturating_sub(TAIL)..].iter().map(|l| l.to_string()).collect()
}

/// Runs `cmd`, writing each line it prints (stdout and stderr) to the log as it comes,
/// and with `echo` to the terminal as well.
fn stream(mut cmd: Command, label: &str, echo: bool) -> (Option<ExitStatus>, Vec<String>) {
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
        if echo && !is_marker(&line) {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "    {line}");
            let _ = out.flush();
        }
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

// ---- What the settings window and the terminal say about an item ---------------------

pub fn title(t: &Texts, item: &Item) -> String {
    match item.id {
        ItemId::UdevRule => t.setup_udev.into(),
        ItemId::Portal => t.setup_portal.into(),
        ItemId::KoreanFont => t.setup_font.into(),
        ItemId::TrayIcon => t.setup_tray.into(),
        ItemId::MenuEntry => t.setup_menu.into(),
        ItemId::BrewUpdate => {
            let version = match &item.why {
                Some(Why::BrewUpdate { version } | Why::BrewNotFound { version }) => version.as_str(),
                _ => "",
            };
            t.setup_brew.replace("{version}", version)
        }
    }
}

pub fn why(t: &Texts, item: &Item) -> String {
    let Some(why) = &item.why else { return String::new() };
    match why {
        Why::UdevMissing => t.setup_udev_missing.into(),
        Why::UdevOutdated => t.setup_udev_outdated.into(),
        Why::UdevDenied => t.setup_udev_denied.into(),
        Why::PortalPackages { gnome, packages } => {
            t.setup_portal_packages.replace("{version}", &gnome.to_string()).replace("{packages}", &packages.join(", "))
        }
        Why::PortalRelogin { .. } => t.setup_portal_relogin.into(),
        Why::PortalX11 { gnome } => t.setup_portal_x11.replace("{version}", &gnome.to_string()),
        Why::FontMissing => t.setup_font_missing.into(),
        Why::TrayInstall => t.setup_tray_install.into(),
        Why::TrayEnable => t.setup_tray_enable.into(),
        Why::TrayRelogin => t.setup_tray_relogin.into(),
        Why::MenuMissing => t.setup_menu_missing.into(),
        Why::BrewUpdate { .. } => t.setup_brew_update.into(),
        Why::BrewNotFound { .. } => t.setup_brew_not_found.into(),
    }
}

// ---- `flockswitch setup` in a terminal --------------------------------------------------

pub const CLI_USAGE: &str = "\
flockswitch setup — check what this computer still needs and install it with one password

USAGE:
    flockswitch setup            list what is missing and why, show the commands, ask,
                                 then install it all: sudo asks for your password once
                                 (pkexec, the desktop's dialog, where there is no terminal)
    flockswitch setup --yes      install without asking
    flockswitch setup --dry-run  only list what is missing and the commands
    flockswitch setup --check    only the list; exit 0 when nothing is left to install

It looks at the udev rule that lets flockswitch open Logitech devices, a font with Hangul
(when flockswitch is in Korean), the tray icon extension (stock GNOME), the desktop portal
for screen-edge switching (GNOME on Wayland) and the app menu entry (Homebrew). Run it as
yourself, not with sudo. What runs is written to ~/.config/flockswitch/setup.log.

EXIT STATUS:
    0  nothing (more) to install; notes such as \"log out and in\" may remain
    1  something is still missing (--check, --dry-run, not approved, or it cannot be
       installed here)
    2  wrong arguments, or run as root
    3  a step failed
    4  no administrator rights (wrong password, cancelled, or no sudo or pkexec)";

/// `flockswitch setup [--yes | --dry-run | --check]`: the setup check in a terminal.
/// Returns the exit status (`setup_logic::exit`).
pub fn cli(args: &[String]) -> i32 {
    use logic::exit;
    let opts = match logic::parse_cli(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}\n\n{CLI_USAGE}");
            return exit::USAGE;
        }
    };
    if opts.help {
        println!("{CLI_USAGE}");
        return exit::OK;
    }
    // The settings' language, read without creating the file (the tray app's first
    // start is told by its absence), else the system's.
    let configured = std::fs::read_to_string(Config::path()).ok().and_then(|t| Config::parse(&t).ok()).map(|c| c.language);
    let korean = logic::korean(configured.as_deref(), sys_locale::get_locale().as_deref());
    crate::i18n::set_language(if korean { "ko" } else { "en" });
    if is_root() {
        eprintln!("{}", crate::i18n::texts().cli_root);
        return exit::USAGE;
    }
    let running = crate::main_instance_running();
    let items = detect(&cli_inputs(korean, running));
    // Korean without a font to draw it in: English, as in the settings window.
    let t = readable_texts(&items);
    // A Homebrew update is not setup: it has its own way (the settings window, brew).
    let missing: Vec<&Item> = items.iter().filter(|i| i.state == logic::State::Missing && i.id != ItemId::BrewUpdate).collect();
    let todo = logic::installable(missing.iter().copied());
    let manual = missing.iter().any(|i| i.fix.is_none() && i.manual.is_some());
    let found = if todo.is_empty() && !manual { exit::OK } else { exit::MISSING };
    if missing.is_empty() {
        println!("{}", t.cli_nothing);
        return exit::OK;
    }
    println!("{}\n", t.cli_found.replace("{n}", &missing.len().to_string()));
    for (n, item) in missing.iter().enumerate() {
        println!("{:>2}. {}", n + 1, title(t, item));
        if opts.check {
            continue;
        }
        let reason = if item.why == Some(Why::FontMissing) { t.cli_font_missing.to_string() } else { why(t, item) };
        println!("    {reason}");
        if let (None, Some(manual)) = (&item.fix, &item.manual) {
            println!("    {}", t.setup_no_package_manager);
            for line in manual.lines() {
                println!("      {line}");
            }
        }
    }
    if opts.check || todo.is_empty() {
        return found;
    }
    let tty = Tty::open();
    let needs_admin = todo.iter().any(|i| i.needs_admin());
    let tool = logic::admin_tool(tty.is_some(), find_program("sudo").is_some(), find_program("pkexec").is_some());
    println!();
    println!(
        "{}",
        match (needs_admin, tool) {
            (false, _) => t.cli_together_user,
            (true, Some(AdminTool::Pkexec)) => t.cli_together_pkexec,
            (true, _) => t.cli_together_sudo,
        }
    );
    print_commands(t, &todo);
    println!();
    if opts.dry_run {
        return exit::MISSING;
    }
    if !opts.yes {
        let approved = match &tty {
            Some(tty) => tty.ask(t.cli_ask),
            None => {
                println!("{}", t.cli_no_tty);
                return exit::MISSING;
            }
        };
        log::info!("setup: {} in the terminal", if approved { "approved" } else { "not approved" });
        if !approved {
            println!("{}", t.cli_declined);
            return exit::MISSING;
        }
    }
    let tool = match tool {
        Some(tool) => tool,
        None if needs_admin => {
            log::info!("setup: neither sudo (with a terminal) nor pkexec");
            println!("{}", t.cli_no_admin);
            return exit::NO_ADMIN;
        }
        // Nothing needs it.
        None => AdminTool::Pkexec,
    };
    println!("{}", t.cli_running);
    let outcomes = run_all(&todo, tool, true);
    println!();
    for (id, outcome) in &outcomes {
        let Some(item) = todo.iter().find(|i| i.id == *id) else { continue };
        match outcome {
            Outcome::Done => println!("  ✓ {}", title(t, item)),
            Outcome::NoAdmin => println!("  ✗ {}: {}", title(t, item), t.cli_no_admin_item),
            Outcome::Failed { code, .. } => {
                let code = code.map_or_else(|| "?".to_string(), |c| c.to_string());
                println!("  ✗ {}: {}", title(t, item), t.cli_failed.replace("{code}", &code));
            }
        }
    }
    let done: Vec<ItemId> = outcomes.iter().filter(|(_, o)| *o == Outcome::Done).map(|(id, _)| *id).collect();
    if running {
        // The running flockswitch looks again: for the devices once udev applied the
        // rule, and at what is still missing (its menu's count).
        if done.contains(&ItemId::UdevRule) {
            std::thread::sleep(Duration::from_secs(2));
            crate::shared::request_rescan();
        }
        crate::shared::request_setup_recheck();
    }
    let outcomes: Vec<Outcome> = outcomes.into_iter().map(|(_, o)| o).collect();
    if outcomes.contains(&Outcome::NoAdmin) {
        println!("{}", t.cli_no_admin);
    }
    if outcomes.iter().any(|o| *o != Outcome::Done) {
        println!("{}", t.cli_log.replace("{path}", &log_path().display().to_string()));
    }
    let left = logic::left_to_do(&items, &done);
    if !left.is_empty() {
        println!("\n{}", t.cli_left);
        for l in &left {
            let text = match l {
                logic::Left::Replug => t.cli_left_replug.to_string(),
                logic::Left::Relogin => t.cli_left_relogin.to_string(),
                logic::Left::MaybeRelogin => t.cli_left_maybe_relogin.to_string(),
                logic::Left::X11Session(v) => t.cli_left_x11.replace("{version}", &v.to_string()),
            };
            println!("  • {text}");
        }
    }
    let code = logic::exit_after(&outcomes, manual);
    log::info!("setup: exit {code}");
    if code == exit::OK && left.is_empty() {
        println!("\n{}", t.cli_ready);
    }
    code
}

/// Where `flockswitch setup` writes its log: beside the tray app's flockswitch.log,
/// which belongs to the running app (started afresh at each of its starts).
pub fn log_path() -> PathBuf {
    crate::log_path().with_file_name("setup.log")
}

/// What the check takes in a terminal: what the running flockswitch found (device
/// access, the edges), or else, briefly, what the edges would find now.
fn cli_inputs(korean: bool, running: bool) -> Inputs {
    let mut inputs = Inputs { korean, ..Inputs::default() };
    if running {
        let st = crate::shared::load_status_file();
        inputs.hid_denied = st.hid_denied;
        inputs.devices_open = !st.online.is_empty();
        inputs.edge_backend = st.edge_backend;
        inputs.edge_note = st.edge_note;
    } else if crate::edge_wayland::session_is_wayland() {
        (inputs.edge_backend, inputs.edge_note) = crate::edge_wayland::probe_now(Duration::from_secs(6));
    }
    inputs
}

/// The steps of `items`, grouped by who runs them, as they are typed in a terminal.
fn print_commands(t: &Texts, items: &[&Item]) {
    fn admin(f: &logic::Fix) -> &[logic::Step] {
        &f.admin
    }
    fn user(f: &logic::Fix) -> &[logic::Step] {
        &f.user
    }
    for (heading, steps_of) in [(t.cli_as_admin, admin as fn(&logic::Fix) -> &[logic::Step]), (t.cli_as_user, user)] {
        let groups: Vec<(&Item, &[logic::Step])> =
            items.iter().filter_map(|i| Some((*i, steps_of(i.fix.as_ref()?)))).filter(|(_, steps)| !steps.is_empty()).collect();
        if groups.is_empty() {
            continue;
        }
        println!("  {heading}");
        for (item, steps) in groups {
            println!("    # {}", title(t, item));
            for step in steps {
                for line in step.text(false).lines() {
                    println!("    {line}");
                }
            }
        }
    }
}

/// Run by root (sudo flockswitch setup): the user steps would land in root's session.
fn is_root() -> bool {
    extern "C" {
        fn geteuid() -> u32;
    }
    // Safety: geteuid takes nothing and cannot fail.
    unsafe { geteuid() == 0 }
}

/// The terminal flockswitch runs in, if any: also under `curl … | sh`, where standard
/// input is the script.
struct Tty(std::fs::File);

impl Tty {
    fn open() -> Option<Tty> {
        std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty").ok().map(Tty)
    }

    /// Asks a yes-or-no question there; Enter is yes, the end of input no.
    fn ask(&self, question: &str) -> bool {
        use std::io::Write;
        let mut reader = BufReader::new(&self.0);
        for _ in 0..3 {
            let _ = (&self.0).write_all(question.as_bytes());
            let _ = (&self.0).flush();
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    let _ = (&self.0).write_all(b"\n");
                    return false;
                }
                Ok(_) => {}
            }
            if let Some(answer) = logic::answer(&line) {
                return answer;
            }
        }
        false
    }
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
    let korean = crate::i18n::is_korean();
    shared.read_status(|st| {
        *devices_seen |= st.devices.iter().any(|d| d.online);
        let newer = match &st.update {
            UpdateState::Available(r) | UpdateState::DownloadFailed(r) | UpdateState::InstallFailed(r) => Some(r.version.clone()),
            _ => None,
        };
        Inputs {
            hid_denied: st.hid_denied,
            devices_open: *devices_seen,
            edge_backend: st.edge_backend.clone(),
            edge_note: st.edge_note.clone(),
            korean,
            newer_version: newer,
        }
    })
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
            let check_updates = shared.config().check_updates;
            let (edges_known, update_known) = shared.read_status(|st| {
                (!st.edge_backend.is_empty(), !check_updates || !matches!(st.update, UpdateState::Idle | UpdateState::Checking))
            });
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
                // The first start on this computer (no settings file before): the
                // settings window opens on the setup check instead of the
                // notification, once.
                let mut opened = false;
                // The settings window, if already opened by hand, has cleared it.
                shared.reload_if_edited();
                if first && shared.config().setup.first_check {
                    if let Err(e) = shared.update_config(|c| c.setup.first_check = false) {
                        log::warn!("setup check: cannot note the first check in the settings: {e:#}");
                    }
                    if pending > 0 {
                        log::info!("setup check: first start, {pending} to set up: opening the settings window");
                        crate::tray::open_settings_page("setup");
                        opened = true;
                    }
                }
                // At the first check one notification says it all; afterwards only a
                // version not told about yet.
                let fresh_update = update.filter(|v| told_version.as_ref() != Some(v));
                let mut lines = Vec::new();
                if first && pending > 0 && !opened {
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
