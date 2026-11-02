//! New-version check and the developer report.
//!
//! The check reads the latest release from GitHub's public API (no account, nothing
//! about the user is sent) and compares it with this build. Downloading happens only
//! when the user asks. HTTP goes through the system's `curl`, which keeps TLS out of the
//! program.

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
    let Some((name, url)) = release.asset.clone() else {
        open_url(&release.page);
        return;
    };
    shared.set_status(|s| s.update = UpdateState::Downloading(release.clone()));
    std::thread::spawn(move || {
        let next = match download(&name, &url) {
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

/// The newest release on GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// "0.3.2" (without the leading v).
    pub version: String,
    pub page: String,
    /// The file for this kind of install: (file name, download URL).
    pub asset: Option<(String, String)>,
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

/// The system's curl, quiet and following redirects.
fn curl() -> Command {
    let mut c = Command::new("curl");
    c.args(["--silent", "--show-error", "--fail", "--location"]);
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
    let asset = pick_asset(&names)
        .and_then(|name| r.assets.iter().find(|a| a.name == name))
        .map(|a| (a.name.clone(), a.browser_download_url.clone()));
    Ok(Release { version, page: r.html_url, asset })
}

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
pub fn download(name: &str, url: &str) -> Result<PathBuf> {
    let dir = dirs::download_dir().or_else(|| dirs::home_dir().map(|h| h.join("Downloads"))).ok_or_else(|| anyhow!("no Downloads folder"))?;
    std::fs::create_dir_all(&dir)?;
    let path = unique_path(&dir, name);
    let out = curl().args(["--max-time", "600", "--output"]).arg(&path).arg(url).output().context("cannot run curl")?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&path);
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(path)
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
    fn mailto_is_encoded() {
        let link = report_mailto("flockswitch 리포트", "a & b\nc=d");
        assert!(link.starts_with("mailto:feedback.wwhale@gmail.com?subject=flockswitch%20%EB%A6%AC"));
        assert!(link.ends_with("&body=a%20%26%20b%0Ac%3Dd"));
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
