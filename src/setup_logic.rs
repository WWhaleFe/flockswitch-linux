//! The setup check, as plain logic: from what was found on this computer (`Facts`),
//! which prerequisites are missing, why, and the exact commands that fix each one.
//! Nothing here runs anything; setup.rs gathers the facts and runs a fix once the user
//! approved it. Only `std` is used, so the tests run on any computer
//! (`rustc --edition 2021 --test src/setup_logic.rs`).
//!
//! A fix has administrator steps, run together through one `pkexec /bin/sh -c '…'`
//! (the desktop's own password dialog), and steps run as the user. Every command is
//! built from constants here or from paths this program found itself (quoted); nothing
//! the user typed goes into a script.

use std::path::{Path, PathBuf};

/// The udev rule this version brings (install.sh and the Homebrew formula install the
/// same file).
pub const UDEV_RULE: &str = include_str!("../packaging/linux/70-flockswitch.rules");
pub const UDEV_RULE_PATH: &str = "/etc/udev/rules.d/70-flockswitch.rules";
/// Where a distribution package would put the same rule (/etc overrides these).
pub const UDEV_RULE_ELSEWHERE: [&str; 2] = ["/usr/lib/udev/rules.d/70-flockswitch.rules", "/lib/udev/rules.d/70-flockswitch.rules"];
/// Solaar's rule gives the same access.
pub const OTHER_UDEV_RULES: [&str; 3] = [
    "/etc/udev/rules.d/42-logitech-unify-permissions.rules",
    "/usr/lib/udev/rules.d/42-logitech-unify-permissions.rules",
    "/lib/udev/rules.d/42-logitech-unify-permissions.rules",
];
/// The GNOME Shell extension that shows tray icons (StatusNotifierItem).
pub const APPINDICATOR_UUID: &str = "appindicatorsupport@rgcjonas.gmail.com";
pub const APPINDICATOR_PACKAGE: &str = "gnome-shell-extension-appindicator";
pub const BREW_FORMULA: &str = "wwhalefe/tap/flockswitch";
/// The desktop portal and its GNOME backend, with the first versions that have
/// InputCapture.
pub const PORTAL_PACKAGES: [(&str, &[u64]); 2] = [("xdg-desktop-portal", &[1, 17]), ("xdg-desktop-portal-gnome", &[45])];
/// The first GNOME with input capture.
pub const GNOME_INPUT_CAPTURE: u32 = 45;
/// Printed first by the administrator script: when it is missing from the output,
/// pkexec never ran the script (the password dialog was cancelled, or there is no
/// agent to show one).
pub const ADMIN_MARKER: &str = "[flockswitch] running as administrator";
const HEREDOC_END: &str = "FLOCKSWITCH_EOF";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemId {
    UdevRule,
    Portal,
    KoreanFont,
    TrayIcon,
    MenuEntry,
    BrewUpdate,
}

impl ItemId {
    /// The name in the settings file (snoozed items) and the log.
    pub fn key(self) -> &'static str {
        match self {
            ItemId::UdevRule => "udev-rule",
            ItemId::Portal => "portal",
            ItemId::KoreanFont => "korean-font",
            ItemId::TrayIcon => "tray-icon",
            ItemId::MenuEntry => "menu-entry",
            ItemId::BrewUpdate => "brew-update",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    Apt,
    Dnf,
    Zypper,
    Pacman,
}

/// In the order they are tried when /etc/os-release names none of them.
pub const PACKAGE_MANAGERS: [PackageManager; 4] = [PackageManager::Apt, PackageManager::Dnf, PackageManager::Zypper, PackageManager::Pacman];

impl PackageManager {
    /// The program that installs packages.
    pub fn program(self) -> &'static str {
        match self {
            PackageManager::Apt => "apt-get",
            PackageManager::Dnf => "dnf",
            PackageManager::Zypper => "zypper",
            PackageManager::Pacman => "pacman",
        }
    }

    /// How an installed package and its version are asked for (no root needed).
    pub fn query(self) -> Query {
        match self {
            PackageManager::Apt => Query::Dpkg,
            PackageManager::Dnf | PackageManager::Zypper => Query::Rpm,
            PackageManager::Pacman => Query::Pacman,
        }
    }

    /// The package with a Korean (CJK) sans-serif font.
    pub fn font_package(self) -> &'static str {
        match self {
            PackageManager::Apt => "fonts-noto-cjk",
            PackageManager::Dnf => "google-noto-sans-cjk-fonts",
            PackageManager::Zypper => "noto-sans-cjk-fonts",
            PackageManager::Pacman => "noto-fonts-cjk",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Query {
    /// `dpkg-query -W -f '${db:Status-Abbrev} ${Version}' <package>`
    Dpkg,
    /// `rpm -q --qf '%{VERSION}\n' <package>`
    Rpm,
    /// `pacman -Q <package>`
    Pacman,
}

impl Query {
    /// The command line (program and arguments) that asks about `package`.
    pub fn command(self, package: &str) -> Vec<String> {
        let args: &[&str] = match self {
            Query::Dpkg => &["dpkg-query", "-W", "-f", "${db:Status-Abbrev} ${Version}"],
            Query::Rpm => &["rpm", "-q", "--qf", "%{VERSION}\\n"],
            Query::Pacman => &["pacman", "-Q"],
        };
        args.iter().map(|a| a.to_string()).chain([package.to_string()]).collect()
    }

    /// Reads the answer: `ok` is whether the command succeeded.
    pub fn parse(self, ok: bool, stdout: &str) -> Pkg {
        if !ok {
            return Pkg::Missing;
        }
        let text = stdout.trim();
        match self {
            Query::Dpkg => {
                let mut words = text.split_whitespace();
                let status = words.next().unwrap_or("");
                // Second letter: i installed, W/t installed with triggers pending.
                let installed = status.chars().nth(1).is_some_and(|c| matches!(c, 'i' | 'W' | 't'));
                match words.next() {
                    Some(version) if installed => Pkg::Installed(version.to_string()),
                    _ => Pkg::Missing,
                }
            }
            Query::Rpm => match text.lines().next().map(str::trim) {
                Some(v) if !v.is_empty() && !v.contains(' ') => Pkg::Installed(v.to_string()),
                _ => Pkg::Missing,
            },
            Query::Pacman => match text.split_whitespace().nth(1) {
                Some(v) => Pkg::Installed(v.to_string()),
                None => Pkg::Missing,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pkg {
    Installed(String),
    Missing,
    /// The package database could not be asked.
    Unknown,
}

/// The package manager of this distribution: the one /etc/os-release points to (ID,
/// then ID_LIKE), if it is there, else the first that is (`present`).
pub fn package_manager(os_release: &str, present: &[PackageManager]) -> Option<PackageManager> {
    let value = |key: &str| {
        os_release
            .lines()
            .find_map(|l| l.trim().strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches(|c| c == '"' || c == '\'').to_ascii_lowercase())
            .unwrap_or_default()
    };
    let (id, like) = (value("ID"), value("ID_LIKE"));
    let by_name = |name: &str| -> Option<PackageManager> {
        let pm = match name {
            "debian" | "ubuntu" | "linuxmint" | "pop" | "elementary" | "raspbian" | "zorin" | "kali" | "neon" => PackageManager::Apt,
            "fedora" | "rhel" | "centos" | "rocky" | "almalinux" | "nobara" | "ultramarine" => PackageManager::Dnf,
            "suse" | "sles" | "opensuse" => PackageManager::Zypper,
            n if n.starts_with("opensuse") => PackageManager::Zypper,
            "arch" | "manjaro" | "endeavouros" | "cachyos" | "garuda" | "artix" => PackageManager::Pacman,
            _ => return None,
        };
        Some(pm)
    };
    std::iter::once(id.as_str())
        .chain(like.split_whitespace())
        .filter_map(by_name)
        .find(|pm| present.contains(pm))
        .or_else(|| PACKAGE_MANAGERS.into_iter().find(|pm| present.contains(pm)))
}

/// "1:1.18.4-1ubuntu2" ≥ [1, 18]: the epoch is skipped, then the leading numbers count.
pub fn version_at_least(version: &str, min: &[u64]) -> bool {
    let v = version.split_once(':').map_or(version, |(_, rest)| rest);
    let numbers: Vec<u64> = v
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<String>()
        .split('.')
        .filter(|p| !p.is_empty())
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    for (i, want) in min.iter().enumerate() {
        let have = numbers.get(i).copied().unwrap_or(0);
        if have != *want {
            return have > *want;
        }
    }
    true
}

/// The GNOME version in what the edge watcher wrote to the status file:
/// "gnome-old:43", or "no-support:GNOME 46 · InputCapture: …".
pub fn gnome_in_note(note: &str) -> Option<u32> {
    if let Some(v) = note.strip_prefix("gnome-old:") {
        return v.trim().parse().ok();
    }
    let at = note.find("GNOME ")?;
    note[at + 6..].chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

/// XDG_CURRENT_DESKTOP names GNOME ("GNOME", "ubuntu:GNOME", "GNOME-Classic:GNOME"),
/// and not a desktop that only builds on it (Budgie, Unity, Pantheon).
pub fn is_gnome(desktop: &str) -> bool {
    let d = desktop.to_ascii_lowercase();
    let parts: Vec<&str> = d.split(':').map(str::trim).collect();
    parts.iter().any(|p| *p == "gnome" || p.starts_with("gnome-")) && !parts.iter().any(|p| matches!(*p, "budgie" | "unity" | "pantheon" | "cinnamon" | "x-cinnamon"))
}

/// `gsettings get org.gnome.shell enabled-extensions` ("['a@b', 'c']" or "@as []")
/// lists `uuid`.
pub fn gsettings_list_contains(output: &str, uuid: &str) -> bool {
    output.contains(&format!("'{uuid}'")) || output.contains(&format!("\"{uuid}\""))
}

/// A word for the shell, quoted only when it needs to be.
pub fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn quoted(path: &Path) -> String {
    shell_quote(&path.to_string_lossy())
}

// ---- Homebrew --------------------------------------------------------------------

/// The Homebrew prefix the running program belongs to:
/// `<prefix>/Cellar/flockswitch/<version>/bin/flockswitch`,
/// `<prefix>/opt/flockswitch/bin/flockswitch` or a linuxbrew `<prefix>/bin/flockswitch`.
pub fn brew_prefix_from_exe(exe: &str) -> Option<PathBuf> {
    if let Some(i) = exe.find("/Cellar/") {
        return Some(PathBuf::from(&exe[..i]));
    }
    if let Some(i) = exe.find("/opt/flockswitch/") {
        return Some(PathBuf::from(&exe[..i]));
    }
    let prefix = exe.strip_suffix("/bin/flockswitch")?;
    prefix.contains("linuxbrew").then(|| PathBuf::from(prefix))
}

/// Where `brew` may be, in order: beside the running program's prefix, under
/// $HOMEBREW_PREFIX, the default /home/linuxbrew/.linuxbrew and ~/.linuxbrew. PATH is
/// never assumed (a program started from the app menu often has another).
pub fn brew_candidates(exes: &[&str], homebrew_prefix: Option<&str>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = exes.iter().filter_map(|e| brew_prefix_from_exe(e)).map(|p| p.join("bin/brew")).collect();
    if let Some(p) = homebrew_prefix.map(str::trim).filter(|p| p.starts_with('/')) {
        out.push(Path::new(p).join("bin/brew"));
    }
    out.push(PathBuf::from("/home/linuxbrew/.linuxbrew/bin/brew"));
    if let Some(h) = home {
        out.push(h.join(".linuxbrew/bin/brew"));
    }
    let mut seen = Vec::new();
    out.retain(|p| {
        let new = !seen.contains(p);
        seen.push(p.clone());
        new
    });
    out
}

/// "Homebrew 6.0.1" (the first line of `brew --version`) → 6.
pub fn brew_major(version_output: &str) -> Option<u64> {
    let line = version_output.lines().find(|l| l.trim_start().starts_with("Homebrew "))?;
    let rest = line.trim_start().strip_prefix("Homebrew ")?.trim_start_matches(|c: char| !c.is_ascii_digit());
    rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

/// Homebrew 6 keeps formulae of a tap it was not told to trust from loading.
pub fn brew_needs_trust(major: Option<u64>) -> bool {
    major.is_some_and(|m| m >= 6)
}

/// The version `<prefix>/opt/flockswitch` points to (`…/Cellar/flockswitch/0.11.1_1`
/// → "0.11.1"; the `_1` is the formula's revision).
pub fn cellar_version(target: &Path) -> Option<String> {
    let name = target.file_name()?.to_string_lossy().into_owned();
    let version = match name.rsplit_once('_') {
        Some((v, rev)) if !rev.is_empty() && rev.chars().all(|c| c.is_ascii_digit()) => v.to_string(),
        _ => name,
    };
    version.chars().next().is_some_and(|c| c.is_ascii_digit()).then_some(version)
}

// ---- What was found ----------------------------------------------------------------

/// Everything the check looks at. setup.rs fills it in; tests make it up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// A Wayland session (XDG_SESSION_TYPE or WAYLAND_DISPLAY).
    pub wayland: bool,
    /// GNOME Shell runs (org.gnome.Shell on the session bus, or XDG_CURRENT_DESKTOP).
    pub gnome_shell: bool,
    /// Its major version, when known.
    pub gnome: Option<u32>,
    /// What the running flockswitch wrote about the screen edges (status file); empty
    /// while it is not running.
    pub edge_backend: String,
    pub edge_note: String,
    pub package_manager: Option<PackageManager>,
    /// The packages asked about (the desktop portal's), and what was found.
    pub packages: Vec<(String, Pkg)>,
    pub udev: Udev,
    /// The window is in Korean.
    pub korean: bool,
    /// A font with Hangul is installed (None: could not tell).
    pub korean_font: Option<bool>,
    /// org.kde.StatusNotifierWatcher is on the session bus (None: no bus, or not asked).
    pub sni_watcher: Option<bool>,
    /// The AppIndicator extension's files are there, and whether it is switched on.
    pub appindicator_installed: bool,
    pub appindicator_enabled: Option<bool>,
    /// Installed with Homebrew.
    pub homebrew: Option<Brew>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Udev {
    /// /etc/udev/rules.d/70-flockswitch.rules, when it is there.
    pub etc: Option<String>,
    /// The same file name under /usr/lib or /lib (a distribution package).
    pub elsewhere: Vec<String>,
    /// Another rule that gives the same access (Solaar's).
    pub other_rule: bool,
    /// The running flockswitch could not open a Logitech device for lack of permission.
    pub hid_denied: bool,
    /// The running flockswitch has devices open.
    pub devices_open: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Brew {
    /// The brew program, if found.
    pub brew: Option<PathBuf>,
    /// `<prefix>/opt/flockswitch/share/applications/flockswitch.desktop`, if there.
    pub desktop_source: Option<PathBuf>,
    /// The user's applications folder (~/.local/share/applications).
    pub apps_dir: PathBuf,
    /// An app menu entry for flockswitch exists (the user's or the system's).
    pub menu_entry: bool,
    /// A newer release than this one, found by the update check.
    pub newer: Option<String>,
    /// `brew --version` is 6 or newer.
    pub needs_trust: bool,
}

// ---- What is missing ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Ok,
    Missing,
    /// Could not tell (nothing is offered).
    Unknown,
    /// Does not apply to this computer.
    NotApplicable,
}

/// Why an item is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    UdevMissing,
    UdevOutdated,
    UdevDenied,
    /// GNOME 45+ on Wayland without input capture: these packages are missing or old.
    PortalPackages { gnome: u32, packages: Vec<String> },
    /// The packages are new enough; the portal starts with the next login.
    PortalRelogin { gnome: u32 },
    /// GNOME 44 or older: an X11 session is the way, nothing to install.
    PortalX11 { gnome: u32 },
    FontMissing,
    /// No tray on stock GNOME: the extension is not installed.
    TrayInstall,
    /// It is installed but not switched on.
    TrayEnable,
    /// It is installed and on, but not running yet (needs a new login).
    TrayRelogin,
    MenuMissing,
    BrewUpdate { version: String },
    /// A newer version, but brew itself was not found.
    BrewNotFound { version: String },
}

/// What to tell the user once a fix went through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    /// Replug the receiver or reconnect Bluetooth (flockswitch looks again by itself).
    Replug,
    /// Log out and in again.
    Relogin,
    /// Probably works now; if not, log out and in again.
    MaybeRelogin,
    /// The window loads the new font.
    Fonts,
    /// The app menu shows flockswitch.
    Menu,
    /// flockswitch restarts into the new version.
    Restart,
}

/// One command line of a fix, with the text fed to it (a here-document), if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub command: String,
    pub input: Option<String>,
}

impl Step {
    fn new(command: impl Into<String>) -> Step {
        Step { command: command.into(), input: None }
    }

    fn with_input(command: impl Into<String>, input: &str) -> Step {
        Step { command: command.into(), input: Some(input.to_string()) }
    }

    fn text(&self, sudo: bool) -> String {
        let cmd = if sudo { format!("sudo {}", self.command) } else { self.command.clone() };
        match &self.input {
            Some(body) => format!("{cmd} <<'{HEREDOC_END}'\n{}\n{HEREDOC_END}", body.trim_end_matches('\n')),
            None => cmd,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    /// Run as root, together, through one pkexec.
    pub admin: Vec<Step>,
    /// Run as the user, after the administrator steps.
    pub user: Vec<Step>,
    pub after: After,
}

impl Fix {
    pub fn needs_admin(&self) -> bool {
        !self.admin.is_empty()
    }

    /// The script pkexec runs (`pkexec /bin/sh -c <this>`).
    pub fn admin_script(&self) -> String {
        let steps: Vec<String> = self.admin.iter().map(|s| s.text(false)).collect();
        format!("set -e\necho '{ADMIN_MARKER}'\nexport PATH=/usr/sbin:/usr/bin:/sbin:/bin\n{}", steps.join("\n"))
    }

    /// The script run as the user (`/bin/sh -c <this>`).
    pub fn user_script(&self) -> String {
        let steps: Vec<String> = self.user.iter().map(|s| s.text(false)).collect();
        format!("set -e\n{}", steps.join("\n"))
    }

    /// The same, to paste into a terminal: the administrator steps with sudo.
    pub fn terminal(&self) -> String {
        self.admin.iter().map(|s| s.text(true)).chain(self.user.iter().map(|s| s.text(false))).collect::<Vec<_>>().join("\n")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: ItemId,
    pub state: State,
    pub why: Option<Why>,
    /// What runs once approved; None where there is nothing to install (explained only).
    pub fix: Option<Fix>,
    /// Commands to run by hand where there is no fix (copyable), if any.
    pub manual: Option<String>,
}

impl Item {
    fn with(id: ItemId, state: State) -> Item {
        Item { id, state, why: None, fix: None, manual: None }
    }

    fn missing(id: ItemId, why: Why, fix: Option<Fix>) -> Item {
        Item { id, state: State::Missing, why: Some(why), fix, manual: None }
    }

    /// Something can be done about it here.
    pub fn actionable(&self) -> bool {
        self.state == State::Missing && self.fix.is_some()
    }

    pub fn needs_admin(&self) -> bool {
        self.fix.as_ref().is_some_and(Fix::needs_admin)
    }

    /// The name it is snoozed under. An update is snoozed for its version only, so the
    /// next one is offered again.
    pub fn snooze_key(&self) -> String {
        match &self.why {
            Some(Why::BrewUpdate { version } | Why::BrewNotFound { version }) => format!("{}:{version}", self.id.key()),
            _ => self.id.key().to_string(),
        }
    }
}

/// Every item, in the order the settings window lists them.
pub fn evaluate(f: &Facts) -> Vec<Item> {
    vec![brew_update(f), udev(f), tray_icon(f), korean_font(f), portal(f), menu_entry(f)]
}

/// The lines that install (or, where the manager tells the two apart, update) packages.
pub fn package_steps(pm: PackageManager, install: &[&str], upgrade: &[&str]) -> Vec<Step> {
    let all: Vec<&str> = install.iter().chain(upgrade).copied().collect();
    let list = |p: &[&str]| p.join(" ");
    let mut steps = Vec::new();
    match pm {
        PackageManager::Apt => {
            // A list that cannot be refreshed (one broken extra repository) must not
            // stop the install; `install` updates a package that is already there.
            steps.push(Step::new("apt-get update || true"));
            steps.push(Step::new(format!("apt-get install -y {}", list(&all))));
        }
        PackageManager::Dnf => {
            if !install.is_empty() {
                steps.push(Step::new(format!("dnf install -y {}", list(install))));
            }
            if !upgrade.is_empty() {
                steps.push(Step::new(format!("dnf upgrade -y {}", list(upgrade))));
            }
        }
        PackageManager::Zypper => {
            if !install.is_empty() {
                steps.push(Step::new(format!("zypper --non-interactive install {}", list(install))));
            }
            if !upgrade.is_empty() {
                steps.push(Step::new(format!("zypper --non-interactive update {}", list(upgrade))));
            }
        }
        PackageManager::Pacman => steps.push(Step::new(format!("pacman -S --needed --noconfirm {}", list(&all)))),
    }
    steps
}

/// The udev rule: written whole (here-document), then udev reads it and applies it to
/// the devices already there.
pub fn udev_fix() -> Fix {
    Fix {
        admin: vec![
            Step::new("mkdir -p /etc/udev/rules.d"),
            Step::with_input(format!("tee {UDEV_RULE_PATH} > /dev/null"), UDEV_RULE),
            Step::new(format!("chmod 644 {UDEV_RULE_PATH}")),
            Step::new("udevadm control --reload-rules"),
            Step::new("udevadm trigger --subsystem-match=hidraw"),
        ],
        user: vec![],
        after: After::Replug,
    }
}

fn same_rule(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.lines().map(str::trim_end).collect::<Vec<_>>().join("\n").trim().to_string();
    norm(a) == norm(b)
}

fn udev(f: &Facts) -> Item {
    let u = &f.udev;
    // /etc overrides a same-named rule under /usr/lib.
    let effective = u.etc.as_deref().or(u.elsewhere.first().map(String::as_str));
    let why = if u.hid_denied {
        Some(Why::UdevDenied)
    } else {
        match effective {
            Some(rule) if same_rule(rule, UDEV_RULE) => None,
            Some(_) => Some(Why::UdevOutdated),
            None if u.other_rule || u.devices_open => None,
            None => Some(Why::UdevMissing),
        }
    };
    match why {
        Some(why) => Item::missing(ItemId::UdevRule, why, Some(udev_fix())),
        None => Item::with(ItemId::UdevRule, State::Ok),
    }
}

/// An item whose fix is installing packages, or naming them where the package manager
/// is not known.
fn with_packages(id: ItemId, why: Why, pm: Option<PackageManager>, install: &[&str], upgrade: &[&str], user: Vec<Step>, after: After) -> Item {
    match pm {
        Some(pm) => Item::missing(id, why, Some(Fix { admin: package_steps(pm, install, upgrade), user, after })),
        None => {
            let mut item = Item::missing(id, why, None);
            let names: Vec<&str> = install.iter().chain(upgrade).copied().collect();
            item.manual = Some(names.join(" "));
            item
        }
    }
}

/// Only for screen-edge switching on GNOME's Wayland session, where flockswitch found
/// no input capture, and only where installing can help (GNOME 45 or newer).
fn portal(f: &Facts) -> Item {
    let id = ItemId::Portal;
    if !f.wayland || !f.gnome_shell {
        return Item::with(id, State::NotApplicable);
    }
    match f.edge_backend.as_str() {
        "" => return Item::with(id, State::Unknown),
        "none" => {}
        _ => return Item::with(id, State::Ok),
    }
    let code = f.edge_note.split_once(':').map_or(f.edge_note.as_str(), |(c, _)| c);
    let gnome = f.gnome.or_else(|| gnome_in_note(&f.edge_note));
    match (code, gnome) {
        ("gnome-old", Some(v)) => Item::missing(id, Why::PortalX11 { gnome: v }, None),
        ("no-support", Some(v)) if v < GNOME_INPUT_CAPTURE => Item::missing(id, Why::PortalX11 { gnome: v }, None),
        ("no-support", Some(v)) => {
            let (mut install, mut upgrade) = (Vec::new(), Vec::new());
            for (name, min) in PORTAL_PACKAGES {
                match f.packages.iter().find(|(n, _)| n == name).map(|(_, s)| s) {
                    Some(Pkg::Installed(version)) if version_at_least(version, min) => {}
                    Some(Pkg::Installed(_)) => upgrade.push(name),
                    // Not asked, or the database could not tell: installing does no harm.
                    _ => install.push(name),
                }
            }
            if install.is_empty() && upgrade.is_empty() {
                return Item::missing(id, Why::PortalRelogin { gnome: v }, None);
            }
            let packages = install.iter().chain(&upgrade).map(|s| s.to_string()).collect();
            with_packages(id, Why::PortalPackages { gnome: v, packages }, f.package_manager, &install, &upgrade, vec![], After::Relogin)
        }
        ("no-support" | "gnome-old", None) => Item::with(id, State::Unknown),
        // No compositor, no display, or a way forced by hand: nothing to install.
        _ => Item::with(id, State::NotApplicable),
    }
}

fn korean_font(f: &Facts) -> Item {
    let id = ItemId::KoreanFont;
    if !f.korean {
        return Item::with(id, State::NotApplicable);
    }
    match f.korean_font {
        Some(true) => Item::with(id, State::Ok),
        None => Item::with(id, State::Unknown),
        Some(false) => {
            let pkg = f.package_manager.map(|pm| pm.font_package());
            let names: Vec<&str> = pkg.into_iter().collect();
            let mut item = with_packages(id, Why::FontMissing, f.package_manager, &names, &[], vec![], After::Fonts);
            if item.fix.is_none() {
                item.manual = Some("Noto Sans CJK (fonts-noto-cjk, google-noto-sans-cjk-fonts, noto-fonts-cjk)".into());
            }
            item
        }
    }
}

/// Stock GNOME shows no tray icons without the AppIndicator extension. Where a panel
/// already hosts them (Ubuntu's own extension, KDE, …) the watcher is on the bus.
fn tray_icon(f: &Facts) -> Item {
    let id = ItemId::TrayIcon;
    if !f.gnome_shell {
        return Item::with(id, State::NotApplicable);
    }
    match f.sni_watcher {
        None => Item::with(id, State::Unknown),
        Some(true) => Item::with(id, State::Ok),
        Some(false) => {
            let enable = Step::new(format!("gnome-extensions enable {APPINDICATOR_UUID}"));
            match (f.appindicator_installed, f.appindicator_enabled) {
                (true, Some(true)) => Item::missing(id, Why::TrayRelogin, None),
                (true, _) => Item::missing(id, Why::TrayEnable, Some(Fix { admin: vec![], user: vec![enable], after: After::MaybeRelogin })),
                (false, _) => {
                    let mut item = with_packages(id, Why::TrayInstall, f.package_manager, &[APPINDICATOR_PACKAGE], &[], vec![enable], After::Relogin);
                    if let Some(m) = item.manual.as_mut() {
                        *m = format!("{m}\ngnome-extensions enable {APPINDICATOR_UUID}");
                    }
                    item
                }
            }
        }
    }
}

/// Homebrew installs no app menu entry where the desktop looks: a link in the user's
/// own applications folder to the one in the formula.
fn menu_entry(f: &Facts) -> Item {
    let id = ItemId::MenuEntry;
    let Some(b) = &f.homebrew else { return Item::with(id, State::NotApplicable) };
    let Some(source) = &b.desktop_source else { return Item::with(id, State::NotApplicable) };
    if b.menu_entry {
        return Item::with(id, State::Ok);
    }
    let link = b.apps_dir.join("flockswitch.desktop");
    let fix = Fix {
        admin: vec![],
        user: vec![Step::new(format!("mkdir -p {}", quoted(&b.apps_dir))), Step::new(format!("ln -sf {} {}", quoted(source), quoted(&link)))],
        after: After::Menu,
    };
    Item::missing(id, Why::MenuMissing, Some(fix))
}

/// The steps that update a Homebrew install.
pub fn brew_steps(brew: &Path, needs_trust: bool) -> Vec<Step> {
    let b = quoted(brew);
    let mut steps = Vec::new();
    if needs_trust {
        steps.push(Step::new(format!("{b} trust --formula {BREW_FORMULA}")));
    }
    steps.push(Step::new(format!("{b} update")));
    steps.push(Step::new(format!("{b} upgrade --formula {BREW_FORMULA}")));
    steps
}

fn brew_update(f: &Facts) -> Item {
    let id = ItemId::BrewUpdate;
    let Some(b) = &f.homebrew else { return Item::with(id, State::NotApplicable) };
    let Some(version) = b.newer.clone() else { return Item::with(id, State::Ok) };
    match &b.brew {
        Some(brew) => {
            let fix = Fix { admin: vec![], user: brew_steps(brew, b.needs_trust), after: After::Restart };
            Item::missing(id, Why::BrewUpdate { version }, Some(fix))
        }
        None => {
            let mut item = Item::missing(id, Why::BrewNotFound { version }, None);
            item.manual = Some(brew_steps(Path::new("brew"), true).iter().map(|s| s.text(false)).collect::<Vec<_>>().join("\n"));
            item
        }
    }
}

// ---- Later ("나중에") ---------------------------------------------------------------

pub fn is_snoozed(item: &Item, snoozed: &[String]) -> bool {
    let key = item.snooze_key();
    snoozed.iter().any(|s| *s == key)
}

/// Adds `item` to the snoozed list (once). Snoozing an update forgets the snooze of an
/// older one.
pub fn snooze(snoozed: &mut Vec<String>, item: &Item) {
    let key = item.snooze_key();
    if item.id == ItemId::BrewUpdate {
        let prefix = format!("{}:", ItemId::BrewUpdate.key());
        snoozed.retain(|s| !s.starts_with(&prefix));
    }
    if !snoozed.contains(&key) {
        snoozed.push(key);
    }
}

/// The missing items to show: not snoozed.
pub fn shown<'a>(items: &'a [Item], snoozed: &[String]) -> Vec<&'a Item> {
    items.iter().filter(|i| i.state == State::Missing && !is_snoozed(i, snoozed)).collect()
}

/// Missing items set aside with "Later".
pub fn snoozed_count(items: &[Item], snoozed: &[String]) -> usize {
    items.iter().filter(|i| i.state == State::Missing && is_snoozed(i, snoozed)).count()
}

/// What the tray menu counts as needing setup: missing, something to install, not
/// snoozed. An update has its own entry.
pub fn pending(items: &[Item], snoozed: &[String]) -> usize {
    shown(items, snoozed).iter().filter(|i| i.actionable() && i.id != ItemId::BrewUpdate).count()
}

/// The new version to offer, unless it was snoozed.
pub fn offered_update(items: &[Item], snoozed: &[String]) -> Option<String> {
    shown(items, snoozed).into_iter().find_map(|i| match &i.why {
        Some(Why::BrewUpdate { version } | Why::BrewNotFound { version }) => Some(version.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(items: &[Item], id: ItemId) -> &Item {
        items.iter().find(|i| i.id == id).unwrap()
    }

    fn gnome_wayland() -> Facts {
        Facts {
            wayland: true,
            gnome_shell: true,
            gnome: Some(46),
            edge_backend: "none".into(),
            edge_note: "no-support:GNOME 46 · InputCapture: no such interface".into(),
            package_manager: Some(PackageManager::Apt),
            udev: Udev { etc: Some(UDEV_RULE.into()), ..Udev::default() },
            sni_watcher: Some(true),
            ..Facts::default()
        }
    }

    #[test]
    fn nothing_to_do_on_a_set_up_computer() {
        let f = Facts { udev: Udev { etc: Some(UDEV_RULE.into()), ..Udev::default() }, ..Facts::default() };
        let items = evaluate(&f);
        assert_eq!(items.len(), 6);
        assert!(shown(&items, &[]).is_empty(), "{items:?}");
        assert_eq!(find(&items, ItemId::UdevRule).state, State::Ok);
        assert_eq!(find(&items, ItemId::Portal).state, State::NotApplicable);
        assert_eq!(find(&items, ItemId::KoreanFont).state, State::NotApplicable);
        assert_eq!(find(&items, ItemId::MenuEntry).state, State::NotApplicable);
    }

    #[test]
    fn udev_rule() {
        let item = |u: Udev| udev(&Facts { udev: u, ..Facts::default() });
        assert_eq!(item(Udev::default()).why, Some(Why::UdevMissing));
        assert!(item(Udev::default()).needs_admin());
        // The same text with other line ends or a trailing newline is the same rule.
        let crlf = UDEV_RULE.replace('\n', "  \n");
        assert_eq!(item(Udev { etc: Some(crlf), ..Udev::default() }).state, State::Ok);
        assert_eq!(item(Udev { etc: Some("# old\n".into()), ..Udev::default() }).why, Some(Why::UdevOutdated));
        // A distribution's copy under /usr/lib counts, unless /etc overrides it.
        assert_eq!(item(Udev { elsewhere: vec![UDEV_RULE.into()], ..Udev::default() }).state, State::Ok);
        assert_eq!(item(Udev { etc: Some("x".into()), elsewhere: vec![UDEV_RULE.into()], ..Udev::default() }).why, Some(Why::UdevOutdated));
        // Solaar's rule, or devices that open: access works without ours.
        assert_eq!(item(Udev { other_rule: true, ..Udev::default() }).state, State::Ok);
        assert_eq!(item(Udev { devices_open: true, ..Udev::default() }).state, State::Ok);
        // Denied wins: installing and applying the rule again is the fix.
        assert_eq!(item(Udev { etc: Some(UDEV_RULE.into()), hid_denied: true, ..Udev::default() }).why, Some(Why::UdevDenied));
    }

    #[test]
    fn udev_commands() {
        let fix = udev_fix();
        let script = fix.admin_script();
        assert!(script.starts_with("set -e\necho '[flockswitch] running as administrator'\n"), "{script}");
        assert!(script.contains("tee /etc/udev/rules.d/70-flockswitch.rules > /dev/null <<'FLOCKSWITCH_EOF'\n# flockswitch"));
        assert!(script.contains("\nFLOCKSWITCH_EOF\nchmod 644 /etc/udev/rules.d/70-flockswitch.rules\nudevadm control --reload-rules\nudevadm trigger --subsystem-match=hidraw"));
        // The rule's own text never ends the here-document early.
        assert!(!UDEV_RULE.contains(HEREDOC_END));
        let terminal = fix.terminal();
        assert!(terminal.starts_with("sudo mkdir -p /etc/udev/rules.d\nsudo tee /etc/udev/rules.d/70-flockswitch.rules > /dev/null <<'FLOCKSWITCH_EOF'\n"));
        assert!(terminal.ends_with("sudo udevadm trigger --subsystem-match=hidraw"));
        assert!(!terminal.contains(ADMIN_MARKER));
        assert!(fix.user.is_empty() && fix.after == After::Replug);
    }

    #[test]
    fn portal_on_gnome_wayland() {
        // Both packages missing.
        let mut f = gnome_wayland();
        f.packages = vec![("xdg-desktop-portal".into(), Pkg::Missing), ("xdg-desktop-portal-gnome".into(), Pkg::Missing)];
        let item = portal(&f);
        assert_eq!(item.why, Some(Why::PortalPackages { gnome: 46, packages: vec!["xdg-desktop-portal".into(), "xdg-desktop-portal-gnome".into()] }));
        let fix = item.fix.unwrap();
        assert_eq!(fix.after, After::Relogin);
        assert_eq!(fix.terminal(), "sudo apt-get update || true\nsudo apt-get install -y xdg-desktop-portal xdg-desktop-portal-gnome");
        // The portal is new enough, its GNOME backend is old (dnf tells install from upgrade).
        f.package_manager = Some(PackageManager::Dnf);
        f.packages = vec![("xdg-desktop-portal".into(), Pkg::Installed("1.18.4".into())), ("xdg-desktop-portal-gnome".into(), Pkg::Installed("44.2".into()))];
        let fix = portal(&f).fix.unwrap();
        assert_eq!(fix.terminal(), "sudo dnf upgrade -y xdg-desktop-portal-gnome");
        // Both fine: the next login starts it; nothing to install.
        f.packages[1].1 = Pkg::Installed("46.2-1".into());
        let item = portal(&f);
        assert_eq!(item.why, Some(Why::PortalRelogin { gnome: 46 }));
        assert!(item.fix.is_none());
        // The database could not tell: offered anyway.
        f.packages = vec![("xdg-desktop-portal".into(), Pkg::Unknown)];
        assert!(portal(&f).actionable());
    }

    #[test]
    fn portal_only_where_it_helps() {
        let mut f = gnome_wayland();
        // GNOME 44 or older: explain the X11 session, offer nothing.
        f.gnome = Some(43);
        f.edge_note = "gnome-old:43".into();
        let item = portal(&f);
        assert_eq!(item.why, Some(Why::PortalX11 { gnome: 43 }));
        assert!(item.fix.is_none() && !item.actionable());
        // The version from the note when GNOME was not asked.
        f.gnome = None;
        assert_eq!(portal(&f).why, Some(Why::PortalX11 { gnome: 43 }));
        f.edge_note = "no-support:GNOME 47 · InputCapture: no answer".into();
        assert!(portal(&f).actionable());
        f.edge_note = "no-support:GNOME ? · InputCapture: no answer".into();
        assert_eq!(portal(&f).state, State::Unknown);
        // Working edges, X11, other desktops, flockswitch not running.
        for (wayland, gnome_shell, backend) in [(true, true, "input-capture"), (false, true, "none"), (true, false, "none"), (true, true, "")] {
            let f = Facts { wayland, gnome_shell, edge_backend: backend.into(), ..gnome_wayland() };
            assert!(!portal(&f).actionable(), "{wayland} {gnome_shell} {backend}");
        }
        // No compositor reached, or a way forced by hand: not a portal problem.
        for note in ["no-wayland:Wayland: refused", "forced:x11", "no-display"] {
            let f = Facts { edge_note: note.into(), ..gnome_wayland() };
            assert_eq!(portal(&f).state, State::NotApplicable, "{note}");
        }
        // No known package manager: the packages are named, to install by hand.
        let f = Facts { package_manager: None, ..gnome_wayland() };
        let item = portal(&f);
        assert!(item.fix.is_none() && item.state == State::Missing);
        assert_eq!(item.manual.as_deref(), Some("xdg-desktop-portal xdg-desktop-portal-gnome"));
    }

    #[test]
    fn korean_font_per_package_manager() {
        let mut f = Facts { korean: true, korean_font: Some(false), ..Facts::default() };
        for (pm, line) in [
            (PackageManager::Apt, "sudo apt-get install -y fonts-noto-cjk"),
            (PackageManager::Dnf, "sudo dnf install -y google-noto-sans-cjk-fonts"),
            (PackageManager::Zypper, "sudo zypper --non-interactive install noto-sans-cjk-fonts"),
            (PackageManager::Pacman, "sudo pacman -S --needed --noconfirm noto-fonts-cjk"),
        ] {
            f.package_manager = Some(pm);
            let fix = korean_font(&f).fix.unwrap();
            assert!(fix.terminal().ends_with(line), "{pm:?}: {}", fix.terminal());
            assert_eq!(fix.after, After::Fonts);
        }
        f.korean_font = Some(true);
        assert_eq!(korean_font(&f).state, State::Ok);
        f.korean_font = None;
        assert_eq!(korean_font(&f).state, State::Unknown);
        // English window: no font needed.
        assert_eq!(korean_font(&Facts { korean_font: Some(false), ..Facts::default() }).state, State::NotApplicable);
    }

    #[test]
    fn tray_icon_on_gnome() {
        let mut f = Facts { gnome_shell: true, sni_watcher: Some(false), package_manager: Some(PackageManager::Dnf), ..Facts::default() };
        let fix = tray_icon(&f).fix.unwrap();
        assert_eq!(fix.terminal(), "sudo dnf install -y gnome-shell-extension-appindicator\ngnome-extensions enable appindicatorsupport@rgcjonas.gmail.com");
        assert_eq!(fix.user_script(), "set -e\ngnome-extensions enable appindicatorsupport@rgcjonas.gmail.com");
        assert_eq!(fix.after, After::Relogin);
        // Installed but off: switched on, no password.
        f.appindicator_installed = true;
        let item = tray_icon(&f);
        assert_eq!(item.why, Some(Why::TrayEnable));
        assert!(!item.needs_admin() && item.actionable());
        // Installed and on, but no watcher yet: a new login.
        f.appindicator_enabled = Some(true);
        assert_eq!(tray_icon(&f).why, Some(Why::TrayRelogin));
        assert!(!tray_icon(&f).actionable());
        // Ubuntu (its own extension), KDE and the rest: a watcher, or not GNOME.
        assert_eq!(tray_icon(&Facts { sni_watcher: Some(true), ..f.clone() }).state, State::Ok);
        assert_eq!(tray_icon(&Facts { gnome_shell: false, ..f.clone() }).state, State::NotApplicable);
        assert_eq!(tray_icon(&Facts { sni_watcher: None, ..f }).state, State::Unknown);
    }

    #[test]
    fn homebrew_menu_entry_and_update() {
        let brew = Brew {
            brew: Some("/home/linuxbrew/.linuxbrew/bin/brew".into()),
            desktop_source: Some("/home/linuxbrew/.linuxbrew/opt/flockswitch/share/applications/flockswitch.desktop".into()),
            apps_dir: "/home/me/.local/share/applications".into(),
            menu_entry: false,
            newer: Some("0.11.1".into()),
            needs_trust: true,
        };
        let f = Facts { homebrew: Some(brew.clone()), udev: Udev { devices_open: true, ..Udev::default() }, ..Facts::default() };
        let items = evaluate(&f);
        let menu = find(&items, ItemId::MenuEntry);
        assert!(!menu.needs_admin());
        assert_eq!(
            menu.fix.as_ref().unwrap().user_script(),
            "set -e\nmkdir -p /home/me/.local/share/applications\nln -sf /home/linuxbrew/.linuxbrew/opt/flockswitch/share/applications/flockswitch.desktop /home/me/.local/share/applications/flockswitch.desktop"
        );
        let update = find(&items, ItemId::BrewUpdate);
        assert_eq!(update.why, Some(Why::BrewUpdate { version: "0.11.1".into() }));
        assert_eq!(
            update.fix.as_ref().unwrap().terminal(),
            "/home/linuxbrew/.linuxbrew/bin/brew trust --formula wwhalefe/tap/flockswitch\n/home/linuxbrew/.linuxbrew/bin/brew update\n/home/linuxbrew/.linuxbrew/bin/brew upgrade --formula wwhalefe/tap/flockswitch"
        );
        assert_eq!(update.fix.as_ref().unwrap().after, After::Restart);
        // Before Homebrew 6 there is no trust step.
        let older = brew_update(&Facts { homebrew: Some(Brew { needs_trust: false, ..brew.clone() }), ..Facts::default() });
        assert!(!older.fix.unwrap().terminal().contains("trust"));
        // Quoted where a path needs it.
        let odd = Brew { apps_dir: "/home/o'neil/apps dir".into(), ..brew.clone() };
        let item = menu_entry(&Facts { homebrew: Some(odd), ..Facts::default() });
        assert!(item.fix.unwrap().user_script().contains("mkdir -p '/home/o'\\''neil/apps dir'"));
        // No brew found: the commands to run by hand.
        let item = brew_update(&Facts { homebrew: Some(Brew { brew: None, ..brew.clone() }), ..Facts::default() });
        assert!(item.fix.is_none());
        assert!(item.manual.unwrap().contains("brew upgrade --formula wwhalefe/tap/flockswitch"));
        // Up to date, entry there, not Homebrew.
        let done = Facts { homebrew: Some(Brew { newer: None, menu_entry: true, ..brew }), ..Facts::default() };
        assert_eq!(find(&evaluate(&done), ItemId::BrewUpdate).state, State::Ok);
        assert_eq!(find(&evaluate(&done), ItemId::MenuEntry).state, State::Ok);
        assert_eq!(find(&evaluate(&Facts::default()), ItemId::BrewUpdate).state, State::NotApplicable);
    }

    #[test]
    fn brew_discovery() {
        assert_eq!(brew_prefix_from_exe("/home/linuxbrew/.linuxbrew/Cellar/flockswitch/0.11.0/bin/flockswitch"), Some("/home/linuxbrew/.linuxbrew".into()));
        assert_eq!(brew_prefix_from_exe("/opt/brew/opt/flockswitch/bin/flockswitch"), Some("/opt/brew".into()));
        assert_eq!(brew_prefix_from_exe("/home/me/.linuxbrew/bin/flockswitch"), Some("/home/me/.linuxbrew".into()));
        assert_eq!(brew_prefix_from_exe("/home/me/.local/bin/flockswitch"), None);
        let c = brew_candidates(
            &["/opt/brew/Cellar/flockswitch/0.11.0/bin/flockswitch", "/opt/brew/bin/flockswitch"],
            Some("/opt/brew"),
            Some(Path::new("/home/me")),
        );
        let want: Vec<PathBuf> = ["/opt/brew/bin/brew", "/home/linuxbrew/.linuxbrew/bin/brew", "/home/me/.linuxbrew/bin/brew"].iter().map(PathBuf::from).collect();
        assert_eq!(c, want);
        // A relative or empty HOMEBREW_PREFIX is ignored.
        assert_eq!(brew_candidates(&[], Some("brew"), None), vec![PathBuf::from("/home/linuxbrew/.linuxbrew/bin/brew")]);
        assert_eq!(brew_major("Homebrew 6.0.1\nHomebrew/homebrew-core (git revision 1)"), Some(6));
        assert_eq!(brew_major("Homebrew >=4.3.0 (shallow or no git repository)"), Some(4));
        assert_eq!(brew_major("bash: brew: not found"), None);
        assert!(brew_needs_trust(Some(6)) && !brew_needs_trust(Some(5)) && !brew_needs_trust(None));
        assert_eq!(cellar_version(Path::new("/home/linuxbrew/.linuxbrew/Cellar/flockswitch/0.11.1_1")), Some("0.11.1".into()));
        assert_eq!(cellar_version(Path::new("/x/Cellar/flockswitch/0.11.1")), Some("0.11.1".into()));
        assert_eq!(cellar_version(Path::new("/x/opt/flockswitch")), None);
    }

    #[test]
    fn package_managers() {
        let all = PACKAGE_MANAGERS;
        let os = |id: &str, like: &str| format!("NAME=x\nID={id}\nID_LIKE=\"{like}\"\n");
        assert_eq!(package_manager(&os("ubuntu", "debian"), &all), Some(PackageManager::Apt));
        assert_eq!(package_manager(&os("fedora", ""), &all), Some(PackageManager::Dnf));
        assert_eq!(package_manager(&os("\"opensuse-tumbleweed\"", "opensuse suse"), &all), Some(PackageManager::Zypper));
        assert_eq!(package_manager(&os("endeavouros", "arch"), &all), Some(PackageManager::Pacman));
        // An unknown ID falls back to ID_LIKE, then to whatever is installed.
        assert_eq!(package_manager(&os("tuxedo", "ubuntu debian"), &all), Some(PackageManager::Apt));
        assert_eq!(package_manager(&os("something", ""), &[PackageManager::Pacman]), Some(PackageManager::Pacman));
        // Named, but not there (a container, say): the one that is.
        assert_eq!(package_manager(&os("fedora", ""), &[PackageManager::Zypper]), Some(PackageManager::Zypper));
        assert_eq!(package_manager("", &[]), None);
    }

    #[test]
    fn package_queries() {
        assert_eq!(Query::Dpkg.parse(true, "ii  1.18.4-1ubuntu2"), Pkg::Installed("1.18.4-1ubuntu2".into()));
        assert_eq!(Query::Dpkg.parse(true, "iW  46.2-1"), Pkg::Installed("46.2-1".into()));
        assert_eq!(Query::Dpkg.parse(true, "rc  1.16.0-1"), Pkg::Missing);
        assert_eq!(Query::Dpkg.parse(true, "un "), Pkg::Missing);
        assert_eq!(Query::Dpkg.parse(false, ""), Pkg::Missing);
        assert_eq!(Query::Rpm.parse(true, "1.20.0\n"), Pkg::Installed("1.20.0".into()));
        assert_eq!(Query::Rpm.parse(false, "package xdg-desktop-portal is not installed\n"), Pkg::Missing);
        assert_eq!(Query::Pacman.parse(true, "xdg-desktop-portal 1.18.4-1\n"), Pkg::Installed("1.18.4-1".into()));
        assert_eq!(Query::Pacman.parse(false, "error: package 'x' was not found"), Pkg::Missing);
        assert_eq!(Query::Rpm.command("xdg-desktop-portal"), ["rpm", "-q", "--qf", "%{VERSION}\\n", "xdg-desktop-portal"]);
        assert!(version_at_least("1:1.18.4-1ubuntu2", &[1, 18]));
        assert!(version_at_least("1.18", &[1, 18]));
        assert!(!version_at_least("1.16.0-1", &[1, 18]));
        assert!(version_at_least("46.2-1", &[45]));
        assert!(!version_at_least("44.2", &[45]));
        assert!(!version_at_least("", &[1]));
    }

    #[test]
    fn package_steps_per_manager() {
        let text = |pm, i: &[&str], u: &[&str]| package_steps(pm, i, u).iter().map(|s| s.text(false)).collect::<Vec<_>>().join("\n");
        assert_eq!(text(PackageManager::Apt, &["a"], &["b"]), "apt-get update || true\napt-get install -y a b");
        assert_eq!(text(PackageManager::Dnf, &["a"], &["b"]), "dnf install -y a\ndnf upgrade -y b");
        assert_eq!(text(PackageManager::Zypper, &[], &["b"]), "zypper --non-interactive update b");
        assert_eq!(text(PackageManager::Pacman, &["a"], &["b"]), "pacman -S --needed --noconfirm a b");
    }

    #[test]
    fn small_helpers() {
        assert_eq!(gnome_in_note("gnome-old:43"), Some(43));
        assert_eq!(gnome_in_note("no-support:GNOME 46 · InputCapture: x"), Some(46));
        assert_eq!(gnome_in_note("no-support:GNOME ? · x"), None);
        assert!(is_gnome("ubuntu:GNOME") && is_gnome("GNOME") && is_gnome("GNOME-Classic:GNOME"));
        assert!(!is_gnome("Budgie:GNOME") && !is_gnome("KDE") && !is_gnome("X-Cinnamon") && !is_gnome(""));
        assert!(gsettings_list_contains("['ding@rastersoft.com', 'appindicatorsupport@rgcjonas.gmail.com']", APPINDICATOR_UUID));
        assert!(!gsettings_list_contains("@as []", APPINDICATOR_UUID));
        assert_eq!(shell_quote("/home/me/.local/share/applications"), "/home/me/.local/share/applications");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn later_is_remembered_per_item() {
        let mut f = Facts { korean: true, korean_font: Some(false), package_manager: Some(PackageManager::Apt), ..Facts::default() };
        f.homebrew = Some(Brew { brew: Some("/b/bin/brew".into()), newer: Some("0.11.1".into()), ..Brew::default() });
        let items = evaluate(&f);
        // udev rule, font, update.
        assert_eq!(shown(&items, &[]).len(), 3);
        assert_eq!(pending(&items, &[]), 2);
        assert_eq!(offered_update(&items, &[]), Some("0.11.1".into()));
        let mut snoozed = Vec::new();
        snooze(&mut snoozed, find(&items, ItemId::KoreanFont));
        snooze(&mut snoozed, find(&items, ItemId::KoreanFont));
        snooze(&mut snoozed, find(&items, ItemId::BrewUpdate));
        assert_eq!(snoozed, ["korean-font", "brew-update:0.11.1"]);
        assert_eq!(shown(&items, &snoozed).len(), 1);
        assert_eq!(snoozed_count(&items, &snoozed), 2);
        assert_eq!(pending(&items, &snoozed), 1);
        assert_eq!(offered_update(&items, &snoozed), None);
        // A newer version is offered again; snoozing it drops the old version's entry.
        f.homebrew.as_mut().unwrap().newer = Some("0.11.2".into());
        let items = evaluate(&f);
        assert_eq!(offered_update(&items, &snoozed), Some("0.11.2".into()));
        snooze(&mut snoozed, find(&items, ItemId::BrewUpdate));
        assert_eq!(snoozed, ["korean-font", "brew-update:0.11.2"]);
        // Info-only items can be set aside too, but never count as pending.
        let x11 = Facts { wayland: true, gnome_shell: true, edge_backend: "none".into(), edge_note: "gnome-old:42".into(), udev: Udev { devices_open: true, ..Udev::default() }, ..Facts::default() };
        let items = evaluate(&x11);
        assert_eq!(shown(&items, &[]).len(), 1);
        assert_eq!(pending(&items, &[]), 0);
    }
}
