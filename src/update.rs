//! New-version check and the developer report.
//!
//! The check reads the latest release from GitHub's public API (no account, nothing
//! about the user is sent) and compares it with this build. HTTP goes through the
//! system's `curl`, which keeps TLS out of the program, and only over https. A
//! downloaded file must match the SHA256SUMS.txt published with the release.
//!
//! Installing (from "Update now", or by itself after the daily check when
//! `auto_update` is on) unpacks the release next to the running program, checks that
//! the new program runs, renames it over the old one (Linux lets a running program's
//! file be replaced that way), refreshes the menu entry install.sh made, and starts the
//! new version, which takes over from this one. A Homebrew install is updated by
//! `brew upgrade` once the user approved it in the settings window (setup.rs; the
//! automatic install only asks, with a notification), and a program in a folder this
//! user cannot write is only downloaded.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

const RELEASES_API: &str = "https://api.github.com/repos/WWhaleFe/flockswitch-linux/releases/latest";
pub const RELEASES_PAGE: &str = "https://github.com/WWhaleFe/flockswitch-linux/releases/latest";
pub const ISSUES_PAGE: &str = "https://github.com/WWhaleFe/flockswitch-linux/issues/new/choose";
pub const REPORT_EMAIL: &str = "feedback.wwhale@gmail.com";
/// How often the running app looks for a new version (plus once shortly after start).
pub const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Where the menu-bar app is with updates (shown in its menu).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Failed,
    Downloading(Release),
    Downloaded(PathBuf),
    DownloadFailed(Release),
    /// The download matched its checksum and the new program is being put in place.
    Installing(Release),
    InstallFailed(Release),
    /// This version is installed; the new copy is starting and takes over.
    Installed(String),
}

/// Checks GitHub and records the outcome; returns it as well.
pub fn check_into(shared: &crate::shared::Shared) -> UpdateState {
    shared.set_status(|s| s.update = UpdateState::Checking);
    let next = match latest() {
        Ok(r) => {
            let newer = r.is_newer_than_this_build();
            log::info!("update check: latest {} ({})", r.version, if newer { "newer" } else { "up to date" });
            if newer { UpdateState::Available(r) } else { UpdateState::UpToDate }
        }
        Err(e) => {
            log::info!("update check failed: {e:#}");
            UpdateState::Failed
        }
    };
    shared.set_status(|s| s.update = next.clone());
    next
}

/// Downloads the release file in the background and shows it in the file manager.
pub fn download_into(shared: std::sync::Arc<crate::shared::Shared>, release: Release) {
    let Some((name, url, sums)) = release.asset.clone() else {
        open_url(&release.page);
        return;
    };
    shared.set_status(|s| s.update = UpdateState::Downloading(release.clone()));
    std::thread::spawn(move || {
        let next = match download(&name, &url, &sums) {
            Ok(path) => {
                log::info!("update downloaded: {}", path.display());
                reveal(&path);
                UpdateState::Downloaded(path)
            }
            Err(e) => {
                log::warn!("update download failed: {e:#}");
                UpdateState::DownloadFailed(release)
            }
        };
        shared.set_status(|s| s.update = next);
    });
}

/// Downloads, checks and installs a release in the background, then starts the new
/// version. `automatic` (the daily check with `auto_update` on) never opens a browser
/// or saves into Downloads: where this copy cannot replace itself it only logs why.
pub fn install_into(shared: std::sync::Arc<crate::shared::Shared>, release: Release, automatic: bool) {
    let exe = match install_kind() {
        InstallKind::SelfReplace(exe) => exe,
        InstallKind::Homebrew => {
            log::info!("update {}: installed with Homebrew; brew installs it once approved in the settings window (`{BREW_UPGRADE}`)", release.version);
            return;
        }
        InstallKind::DownloadOnly if automatic => {
            log::info!("update {}: cannot replace the program here; download it from the menu", release.version);
            return;
        }
        InstallKind::DownloadOnly => return download_into(shared, release),
    };
    if release.asset.is_none() {
        log::info!("update {}: the release has no file for this computer", release.version);
        if !automatic {
            open_url(&release.page);
        }
        return;
    }
    log::info!("update {}: installing over {}{}", release.version, exe.display(), if automatic { " (automatic)" } else { "" });
    shared.set_status(|s| s.update = UpdateState::Downloading(release.clone()));
    std::thread::spawn(move || {
        let phase = |p: Phase| {
            if p == Phase::Installing {
                shared.set_status(|s| s.update = UpdateState::Installing(release.clone()));
            }
        };
        let next = match install(&release, &exe, &phase) {
            Ok(()) => {
                // The new copy asks this one to quit (see `start_new_version`).
                if let Err(e) = start_new_version(&exe, &[]) {
                    log::warn!("update {}: installed, but cannot start it: {e:#}; restart flockswitch", release.version);
                }
                UpdateState::Installed(release.version.clone())
            }
            Err(e) => {
                log::warn!("update {}: not installed: {e:#}", release.version);
                UpdateState::InstallFailed(release)
            }
        };
        shared.set_status(|s| s.update = next);
    });
}

/// How this copy can be updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// Replace this program file (the path, resolved) in place.
    SelfReplace(PathBuf),
    /// Installed by Homebrew, which owns the file: `brew upgrade --formula wwhalefe/tap/flockswitch`.
    Homebrew,
    /// The folder cannot be written (or the program's path is unknown): download only.
    DownloadOnly,
}

/// What a Homebrew install is updated with.
pub const BREW_UPGRADE: &str = "brew update && brew upgrade --formula wwhalefe/tap/flockswitch";

/// How the running copy can be updated. Worked out once, before anything is replaced
/// (afterwards Linux reports the running program as "<path> (deleted)").
pub fn install_kind() -> InstallKind {
    static KIND: std::sync::OnceLock<InstallKind> = std::sync::OnceLock::new();
    KIND.get_or_init(|| {
        let Some(exe) = running_exe() else { return InstallKind::DownloadOnly };
        let kind = if is_homebrew_path(&exe.to_string_lossy()) {
            InstallKind::Homebrew
        } else if exe.parent().is_some_and(dir_writable) {
            InstallKind::SelfReplace(exe.clone())
        } else {
            InstallKind::DownloadOnly
        };
        log::info!("updates for {}: {kind:?}", exe.display());
        kind
    })
    .clone()
}

/// A program inside a Homebrew prefix (`…/Cellar/flockswitch/<version>/bin`, or
/// anything under linuxbrew), which only Homebrew should replace.
pub fn is_homebrew_path(path: &str) -> bool {
    path.contains("/Cellar/") || path.contains("linuxbrew")
}

/// The running program file, with links resolved.
fn running_exe() -> Option<PathBuf> {
    let exe = without_deleted(&std::env::current_exe().ok()?);
    Some(std::fs::canonicalize(&exe).unwrap_or(exe))
}

/// "/home/me/.local/bin/flockswitch (deleted)" (a replaced file) → the path itself.
fn without_deleted(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_suffix(" (deleted)") {
        Some(p) => PathBuf::from(p),
        None => path.to_path_buf(),
    }
}

/// True when this user can create (and so rename) files in `dir`: tried for real,
/// because permissions, ACLs and read-only mounts all have a say.
fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".flockswitch-probe-{}", std::process::id()));
    let ok = std::fs::create_dir(&probe).is_ok();
    let _ = std::fs::remove_dir(&probe);
    ok
}

/// Where `install` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Downloading,
    Installing,
}

/// Only one copy installs at a time (the tray app and the settings window can both
/// start it): a loopback port held while installing, like the single-instance lock.
const INSTALL_LOCK_PORT: u16 = 47_853;

/// The folder a release is unpacked in, next to the program so the rename into place
/// stays on one file system (and so is atomic).
const STAGING_PREFIX: &str = ".flockswitch-update-";

/// Downloads `release`, checks it against the release's SHA256SUMS.txt, unpacks it next
/// to `exe`, checks that the new program is there and runs on this computer, and then
/// renames it over `exe`. The app-menu entry and icon that install.sh made are brought
/// up to date as well. Nothing is replaced unless every check passed.
pub fn install(release: &Release, exe: &Path, phase: &dyn Fn(Phase)) -> Result<()> {
    let (name, url, sums) = release.asset.as_ref().ok_or_else(|| anyhow!("the release has no file for this computer"))?;
    let _lock = std::net::TcpListener::bind(("127.0.0.1", INSTALL_LOCK_PORT))
        .map_err(|_| anyhow!("another flockswitch is installing an update"))?;
    let dir = exe.parent().ok_or_else(|| anyhow!("{} has no folder", exe.display()))?;
    let staging = Staging::new(dir)?;

    phase(Phase::Downloading);
    log::info!("update {}: downloading {name} into {}", release.version, staging.0.display());
    let tarball = download_to(name, url, sums, &staging.0)?;
    log::info!("update {}: {name} matches {SUMS}", release.version);

    phase(Phase::Installing);
    let files = unpack_and_replace(&tarball, name, &staging.0, exe)?;
    log::info!("update {}: replaced {}", release.version, exe.display());
    if let Some(data) = dirs::data_dir() {
        update_menu_entry(&files, exe, &data);
    }
    check_udev_rule(&files);
    Ok(())
}

/// Unpacks a checked release file in `staging` (in the program's folder) and renames
/// the new program over `exe` once it proved to run. Returns the unpacked folder.
fn unpack_and_replace(tarball: &Path, name: &str, staging: &Path, exe: &Path) -> Result<PathBuf> {
    let out = Command::new("tar").arg("-xzf").arg(tarball).arg("-C").arg(staging).output().context("cannot run tar")?;
    if !out.status.success() {
        bail!("cannot unpack {name}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let files = unpacked_dir(staging, name).ok_or_else(|| anyhow!("unexpected file name {name}"))?;
    let new_exe = files.join("flockswitch");
    check_program(&new_exe)?;
    log::info!("update: unpacked {} runs here", new_exe.display());
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&new_exe, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&new_exe, exe).with_context(|| format!("cannot replace {}", exe.display()))?;
    Ok(files)
}

/// The unpack folder; removed again when dropped, with whatever is left in it.
struct Staging(PathBuf);

impl Staging {
    fn new(dir: &Path) -> Result<Staging> {
        // Leftovers of an install that was cut short (the install lock is held).
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
                if is_dir && e.file_name().to_string_lossy().starts_with(STAGING_PREFIX) {
                    log::info!("update: removing the leftover {}", e.path().display());
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        let path = dir.join(format!("{STAGING_PREFIX}{}", std::process::id()));
        std::fs::create_dir(&path).with_context(|| format!("cannot create {}", path.display()))?;
        Ok(Staging(path))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            log::warn!("cannot remove {}: {e}", self.0.display());
        }
    }
}

/// The release file `flockswitch-linux-x86_64-0.7.0.tar.gz` holds one folder of the same
/// name (see .github/workflows/release.yml).
fn unpacked_dir(staging: &Path, tarball_name: &str) -> Option<PathBuf> {
    let stem = tarball_name.strip_suffix(".tar.gz")?;
    let ok = !stem.is_empty() && !stem.starts_with('.') && !stem.contains('/');
    ok.then(|| staging.join(stem))
}

/// The new program must be a plain executable file that runs here (right architecture
/// and C library) before it replaces the working one: `flockswitch help` must answer.
fn check_program(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::symlink_metadata(path).with_context(|| format!("{} is not in the release file", path.display()))?;
    if !meta.is_file() || meta.len() == 0 {
        bail!("{} is not a program file", path.display());
    }
    if meta.permissions().mode() & 0o111 == 0 {
        bail!("{} is not executable", path.display());
    }
    let mut child = Command::new(path)
        .arg("help")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("cannot run {}", path.display()))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{} did not answer", path.display());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let out = child.wait_with_output()?;
    if !out.status.success() || !String::from_utf8_lossy(&out.stdout).contains("flockswitch switch") {
        bail!("{} does not run on this computer", path.display());
    }
    Ok(())
}

/// install.sh writes `~/.local/share/applications/flockswitch.desktop` (pointing at the
/// program) and the icon beside it in `~/.local/share/flockswitch` (`data` is
/// `~/.local/share` or $XDG_DATA_HOME). When that entry starts this program, both are
/// replaced with the release's. A Homebrew link or an entry for another copy is left
/// alone. Failing here does not undo the update.
fn update_menu_entry(files: &Path, exe: &Path, data: &Path) {
    let apps = data.join("applications");
    let desktop = apps.join("flockswitch.desktop");
    let Ok(meta) = std::fs::symlink_metadata(&desktop) else {
        log::info!("update: no menu entry at {}", desktop.display());
        return;
    };
    let current = if meta.is_file() { std::fs::read_to_string(&desktop).ok() } else { None };
    let Some(bin) = current.as_deref().and_then(|c| desktop_value(c, "Exec")).map(str::to_string) else {
        log::info!("update: {} is not a menu entry install.sh made; left alone", desktop.display());
        return;
    };
    if std::fs::canonicalize(&bin).ok().as_deref() != Some(exe) {
        log::info!("update: {} starts {bin}, not this program; left alone", desktop.display());
        return;
    }
    let icon = data.join("flockswitch").join("flockswitch.png");
    let result = (|| -> Result<()> {
        let template = std::fs::read_to_string(files.join("flockswitch.desktop"))?;
        if let Some(d) = icon.parent() {
            std::fs::create_dir_all(d)?;
        }
        write_atomic(&icon, &std::fs::read(files.join("flockswitch.png"))?, 0o644)?;
        write_atomic(&desktop, render_desktop(&template, &bin, &icon.to_string_lossy()).as_bytes(), 0o644)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            let _ = Command::new("update-desktop-database").arg(&apps).output();
            log::info!("update: refreshed {} and {}", desktop.display(), icon.display());
        }
        Err(e) => log::warn!("update: menu entry not refreshed: {e:#}"),
    }
}

/// `key=value` of a .desktop file's own (first) group, e.g. Exec.
fn desktop_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=')).map(str::trim).filter(|v| !v.is_empty())
}

/// flockswitch.desktop from the release with its placeholders filled, as install.sh does.
fn render_desktop(template: &str, bin: &str, icon: &str) -> String {
    template.replace("@BIN@", bin).replace("@ICON@", icon)
}

/// Writes a file whole, then renames it into place.
fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let name = path.file_name().ok_or_else(|| anyhow!("{} has no file name", path.display()))?;
    let tmp = path.with_file_name(format!(".{}.new", name.to_string_lossy()));
    std::fs::write(&tmp, bytes).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))
}

/// The udev rule needs root, which an update never asks for: say so when it changed.
/// The new version's setup check offers it (with the password dialog) once it runs.
fn check_udev_rule(files: &Path) {
    const RULE: &str = "/etc/udev/rules.d/70-flockswitch.rules";
    let new = std::fs::read(files.join("70-flockswitch.rules")).ok();
    if new.is_some() && new != std::fs::read(RULE).ok() {
        log::warn!("update: the release's udev rule differs from {RULE}; Settings → General → Setup check offers to install it (or run install.sh from the release)");
    }
}

/// Starts the installed program the way start-at-login does. It finds this copy still
/// running with the older version, asks it to quit (quit.request) and takes over the
/// single-instance lock once it is gone. Started in its own process group, so it lives
/// on when this one (or the terminal it came from) goes.
pub fn start_new_version(exe: &Path, args: &[&str]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("cannot start {}", exe.display()))?;
    log::info!("update: started {} {}", exe.display(), args.join(" "));
    Ok(())
}

/// The newest release on GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// "0.3.2" (without the leading v).
    pub version: String,
    pub page: String,
    /// The file for this kind of install and the checksums published with it:
    /// (file name, download URL, SHA256SUMS.txt URL). None when either is missing.
    pub asset: Option<(String, String, String)>,
}

impl Release {
    pub fn is_newer_than_this_build(&self) -> bool {
        is_newer(&self.version, env!("CARGO_PKG_VERSION"))
    }
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// The system's curl, quiet and following redirects, https only (redirects too).
fn curl() -> Command {
    let mut c = Command::new("curl");
    c.args(["--silent", "--show-error", "--fail", "--location", "--proto", "=https", "--proto-redir", "=https"]);
    c.arg("--user-agent").arg(concat!("flockswitch/", env!("CARGO_PKG_VERSION")));
    c
}

/// Asks GitHub for the latest release.
pub fn latest() -> Result<Release> {
    let out = curl()
        .args(["--max-time", "15", "--header", "Accept: application/vnd.github+json", RELEASES_API])
        .output()
        .context("cannot run curl")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let r: GhRelease = serde_json::from_slice(&out.stdout).context("unexpected reply from GitHub")?;
    let version = r.tag_name.trim().trim_start_matches(['v', 'V']).to_string();
    let names: Vec<&str> = r.assets.iter().map(|a| a.name.as_str()).collect();
    let url = |name: &str| r.assets.iter().find(|a| a.name == name).map(|a| a.browser_download_url.clone());
    let asset = pick_asset(&names).and_then(|name| Some((name.to_string(), url(name)?, url(SUMS)?)));
    Ok(Release { version, page: r.html_url, asset })
}

/// The checksum list published with every release (`sha256sum` output).
const SUMS: &str = "SHA256SUMS.txt";

/// The release file for this machine (flockswitch-linux-x86_64-….tar.gz).
fn pick_asset<'a>(names: &[&'a str]) -> Option<&'a str> {
    pick_asset_for(names, std::env::consts::ARCH)
}

fn pick_asset_for<'a>(names: &[&'a str], arch: &str) -> Option<&'a str> {
    let prefix = format!("flockswitch-linux-{arch}-");
    names.iter().copied().find(|n| n.starts_with(&prefix) && n.ends_with(".tar.gz"))
}

/// "1.2.10" > "1.2.9"; parts that are not numbers count as 0.
pub fn is_newer(a: &str, b: &str) -> bool {
    let parts = |s: &str| -> Vec<u64> {
        s.split('.').map(|p| p.chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0)).collect()
    };
    let (x, y) = (parts(a), parts(b));
    for i in 0..x.len().max(y.len()) {
        let (l, r) = (x.get(i).copied().unwrap_or(0), y.get(i).copied().unwrap_or(0));
        if l != r {
            return l > r;
        }
    }
    false
}

/// Downloads a release file into the Downloads folder (never over an existing file).
/// A file whose SHA-256 is not the one in the release's checksum list is deleted.
pub fn download(name: &str, url: &str, sums_url: &str) -> Result<PathBuf> {
    let dir = dirs::download_dir().or_else(|| dirs::home_dir().map(|h| h.join("Downloads"))).ok_or_else(|| anyhow!("no Downloads folder"))?;
    std::fs::create_dir_all(&dir)?;
    download_to(name, url, sums_url, &dir)
}

/// Downloads a release file into `dir`; see `download`.
fn download_to(name: &str, url: &str, sums_url: &str, dir: &Path) -> Result<PathBuf> {
    let out = curl().args(["--max-time", "15", sums_url]).output().context("cannot run curl")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let want = expected_sha256(&String::from_utf8_lossy(&out.stdout), name).ok_or_else(|| anyhow!("{name} is not in {SUMS}"))?;
    let path = unique_path(dir, name);
    let out = curl().args(["--max-time", "600", "--output"]).arg(&path).arg(url).output().context("cannot run curl")?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&path);
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    match sha256(&path) {
        Ok(have) if have == want => {}
        Ok(have) => {
            let _ = std::fs::remove_file(&path);
            bail!("{name} does not match {SUMS} (got {have}, expected {want})");
        }
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
    }
    Ok(path)
}

/// The hash listed for `name` in `sha256sum` output ("<hex>  <name>", or "*<name>" in
/// binary mode), lowercased.
fn expected_sha256(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.trim().split_once(char::is_whitespace)?;
        let ok = file.trim_start().trim_start_matches('*') == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        ok.then(|| hash.to_ascii_lowercase())
    })
}

/// SHA-256 of a file, lowercase hex, from the system's `sha256sum` (coreutils).
fn sha256(path: &Path) -> Result<String> {
    let out = Command::new("sha256sum").arg(path).output().context("cannot run sha256sum")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let hash = text.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    if hash.len() != 64 {
        bail!("unexpected sha256sum output: {}", text.trim());
    }
    Ok(hash)
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (name.to_string(), String::new()),
    };
    (1..).map(|i| dir.join(format!("{stem}-{i}{ext}"))).find(|p| !p.exists()).unwrap_or(path)
}

/// Shows a file in the file manager: selected where the file manager supports that
/// (the freedesktop FileManager1 interface), otherwise its folder.
pub fn reveal(path: &Path) {
    let uri = format!("file://{}", path.display());
    let selected = Command::new("dbus-send")
        .args([
            "--session",
            "--print-reply",
            "--dest=org.freedesktop.FileManager1",
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1.ShowItems",
        ])
        .arg(format!("array:string:{uri}"))
        .arg("string:")
        .output()
        .is_ok_and(|o| o.status.success());
    if !selected {
        open_url(&path.parent().unwrap_or(path).display().to_string());
    }
}

/// Opens a web or mailto link (or a folder) with the desktop's default app.
pub fn open_url(url: &str) {
    if let Err(e) = Command::new("xdg-open").arg(url).spawn() {
        log::warn!("cannot open {url}: {e}");
    }
}

/// The start of a report: room to write, then what helps to find the cause. Only
/// what the user can see in the mail before sending it.
pub fn report_body(cfg: &crate::config::Config, devices: &[crate::shared::DeviceInfo], master: Option<&str>) -> String {
    use crate::config::{button_name, BUTTONS};
    let mut s = String::from("\n\n\n---- flockswitch ----\n");
    s += &format!("version: {}\n", env!("CARGO_PKG_VERSION"));
    s += &format!("system: {} {}\n", os_version(), std::env::consts::ARCH);
    s += &format!("master mouse: {} (setting: {})\n", master.unwrap_or("not here"), cfg.trigger_device);
    for cid in BUTTONS {
        let b = cfg.button(cid);
        if b.on {
            s += &format!("button {}: channels {:?}", button_name(cid), b.channels);
            for (name, m) in [("up", &b.moves.up), ("down", &b.moves.down), ("left", &b.moves.left), ("right", &b.moves.right)] {
                if m.on {
                    s += &format!(", {name} {:?}", m.channels);
                }
            }
            s += "\n";
        }
    }
    for d in devices {
        let here = if d.online { "" } else { ", on another computer" };
        s += &format!("device: {} [{}] {}{here}\n", d.name, d.key, d.kind);
    }
    s
}

fn os_version() -> String {
    // e.g. PRETTY_NAME="Ubuntu 24.04.1 LTS", plus the desktop and session type.
    let name = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
        })
        .unwrap_or_else(|| "Linux".into());
    let var = |n: &str| std::env::var(n).unwrap_or_default();
    format!("{name} ({} {})", var("XDG_CURRENT_DESKTOP"), var("XDG_SESSION_TYPE")).replace(" ( )", "")
}

/// A mailto: link to the developer with the subject and a body filled in.
pub fn report_mailto(subject: &str, body: &str) -> String {
    format!("mailto:{REPORT_EMAIL}?subject={}&body={}", percent_encode(subject), percent_encode(body))
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("0.3.2", "0.3.1"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0", "0.99.99"));
        assert!(!is_newer("0.3.1", "0.3.1"));
        assert!(!is_newer("0.3.1", "0.3.2"));
        assert!(!is_newer("0.3", "0.3.0"));
    }

    #[test]
    fn assets() {
        let names = [
            "flockswitch-linux-aarch64-0.1.0.tar.gz",
            "flockswitch-linux-x86_64-0.1.0.tar.gz",
            "SHA256SUMS.txt",
        ];
        assert_eq!(pick_asset_for(&names, "x86_64"), Some("flockswitch-linux-x86_64-0.1.0.tar.gz"));
        assert_eq!(pick_asset_for(&names, "aarch64"), Some("flockswitch-linux-aarch64-0.1.0.tar.gz"));
        assert_eq!(pick_asset_for(&names, "riscv64"), None);
    }

    #[test]
    fn checksums() {
        let h = "a".repeat(64);
        let sums = format!("{}  flockswitch-linux-x86_64-0.1.2.tar.gz\n{h} *other.tar.gz\n", "B".repeat(64));
        assert_eq!(expected_sha256(&sums, "flockswitch-linux-x86_64-0.1.2.tar.gz"), Some("b".repeat(64)));
        assert_eq!(expected_sha256(&sums, "other.tar.gz"), Some(h));
        assert_eq!(expected_sha256(&sums, "missing.tar.gz"), None);
        assert_eq!(expected_sha256("abc  short.tar.gz", "short.tar.gz"), None);
    }

    #[test]
    fn mailto_is_encoded() {
        let link = report_mailto("flockswitch 리포트", "a & b\nc=d");
        assert!(link.starts_with("mailto:feedback.wwhale@gmail.com?subject=flockswitch%20%EB%A6%AC"));
        assert!(link.ends_with("&body=a%20%26%20b%0Ac%3Dd"));
    }

    #[test]
    fn homebrew_paths() {
        assert!(is_homebrew_path("/home/linuxbrew/.linuxbrew/Cellar/flockswitch/0.6.0/bin/flockswitch"));
        assert!(is_homebrew_path("/home/me/.linuxbrew/bin/flockswitch"));
        assert!(is_homebrew_path("/opt/brew/Cellar/flockswitch/0.6.0/bin/flockswitch"));
        assert!(!is_homebrew_path("/home/me/.local/bin/flockswitch"));
        assert!(!is_homebrew_path("/usr/bin/flockswitch"));
    }

    #[test]
    fn replaced_program_path() {
        assert_eq!(without_deleted(Path::new("/home/me/.local/bin/flockswitch (deleted)")), PathBuf::from("/home/me/.local/bin/flockswitch"));
        assert_eq!(without_deleted(Path::new("/home/me/.local/bin/flockswitch")), PathBuf::from("/home/me/.local/bin/flockswitch"));
    }

    #[test]
    fn unpacked_folder() {
        let s = Path::new("/home/me/.local/bin/.flockswitch-update-42");
        assert_eq!(
            unpacked_dir(s, "flockswitch-linux-x86_64-0.7.0.tar.gz"),
            Some(s.join("flockswitch-linux-x86_64-0.7.0"))
        );
        assert_eq!(unpacked_dir(s, "flockswitch.zip"), None);
        assert_eq!(unpacked_dir(s, ".tar.gz"), None);
        assert_eq!(unpacked_dir(s, "../x.tar.gz"), None);
    }

    #[test]
    fn menu_entry() {
        let template = "[Desktop Entry]\nType=Application\nExec=@BIN@\nIcon=@ICON@\n";
        let text = render_desktop(template, "/home/me/.local/bin/flockswitch", "/home/me/.local/share/flockswitch/flockswitch.png");
        assert_eq!(desktop_value(&text, "Exec"), Some("/home/me/.local/bin/flockswitch"));
        assert_eq!(desktop_value(&text, "Icon"), Some("/home/me/.local/share/flockswitch/flockswitch.png"));
        assert_eq!(desktop_value("Exec=\n", "Exec"), None);
        assert_eq!(desktop_value("TryExec=x\n", "Exec"), None);
    }

    /// A release folder with a stand-in program that answers `help` like flockswitch.
    fn fake_release(root: &Path, program: &str) -> (PathBuf, &'static str) {
        use std::os::unix::fs::PermissionsExt;
        let name = "flockswitch-linux-x86_64-9.9.9.tar.gz";
        let src = root.join("src").join("flockswitch-linux-x86_64-9.9.9");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("flockswitch"), program).unwrap();
        std::fs::set_permissions(src.join("flockswitch"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(src.join("flockswitch.png"), b"new icon").unwrap();
        std::fs::write(src.join("flockswitch.desktop"), "[Desktop Entry]\nName=flockswitch 9\nExec=@BIN@\nIcon=@ICON@\n").unwrap();
        let tarball = root.join(name);
        let ok = Command::new("tar")
            .arg("-czf")
            .arg(&tarball)
            .arg("-C")
            .arg(root.join("src"))
            .arg("flockswitch-linux-x86_64-9.9.9")
            .status()
            .unwrap()
            .success();
        assert!(ok);
        (tarball, name)
    }

    #[test]
    fn replaces_the_program_and_menu_entry() {
        let root = std::env::temp_dir().join(format!("flockswitch-itest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let bin = root.join("bin");
        let exe = bin.join("flockswitch");
        std::fs::write(&exe, "#!/bin/sh\necho old\n").unwrap();
        let (tarball, name) = fake_release(&root, "#!/bin/sh\necho 'flockswitch switch <N>'\n");
        let staging = bin.join(".flockswitch-update-test");
        std::fs::create_dir_all(&staging).unwrap();

        // A program that does not answer like flockswitch is not installed.
        let (bad, _) = fake_release(&root.join("bad"), "#!/bin/sh\nexit 0\n");
        assert!(unpack_and_replace(&bad, name, &staging, &exe).is_err());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "#!/bin/sh\necho old\n");

        let files = unpack_and_replace(&tarball, name, &staging, &exe).unwrap();
        assert!(std::fs::read_to_string(&exe).unwrap().contains("flockswitch switch"));

        // The menu entry install.sh made for this program follows; one for another copy does not.
        let data = root.join("share");
        let desktop = data.join("applications").join("flockswitch.desktop");
        std::fs::create_dir_all(desktop.parent().unwrap()).unwrap();
        std::fs::write(&desktop, "[Desktop Entry]\nExec=/usr/bin/flockswitch\n").unwrap();
        update_menu_entry(&files, &exe, &data);
        assert_eq!(std::fs::read_to_string(&desktop).unwrap(), "[Desktop Entry]\nExec=/usr/bin/flockswitch\n");
        std::fs::write(&desktop, format!("[Desktop Entry]\nExec={}\n", exe.display())).unwrap();
        update_menu_entry(&files, &exe, &data);
        let icon = data.join("flockswitch").join("flockswitch.png");
        assert_eq!(
            std::fs::read_to_string(&desktop).unwrap(),
            format!("[Desktop Entry]\nName=flockswitch 9\nExec={}\nIcon={}\n", exe.display(), icon.display())
        );
        assert_eq!(std::fs::read(&icon).unwrap(), b"new icon");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn writable_folder() {
        let dir = std::env::temp_dir().join(format!("flockswitch-wtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(dir_writable(&dir));
        assert!(!dir_writable(&dir.join("missing")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_names() {
        let dir = std::env::temp_dir().join(format!("flockswitch-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.zip"), b"").unwrap();
        assert_eq!(unique_path(&dir, "a.zip"), dir.join("a-1.zip"));
        assert_eq!(unique_path(&dir, "b.zip"), dir.join("b.zip"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
