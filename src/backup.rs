//! Saving the settings to a file and loading them back, here or on another computer.
//! The file is the settings file itself, the same TOML every flockswitch writes, with a
//! few comment lines on top. So a file from the Windows or macOS version loads here
//! too: settings this version does not have are skipped, missing ones keep their
//! defaults, and everything is checked as the settings file is before it is used.

use crate::config::Config;
use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Saved files are named `flockswitch-settings-<host>-<YYYYMMDD-HHMMSS>.toml`.
const PREFIX: &str = "flockswitch-settings-";
/// A settings file is a few KB; anything this large is something else.
const MAX_SIZE: u64 = 1 << 20;
/// The top-level settings; a file with none of them is not a flockswitch settings file.
const KNOWN_KEYS: [&str; 11] = [
    "enabled",
    "trigger_device",
    "follow",
    "language",
    "check_updates",
    "auto_update",
    "buttons",
    "hotkeys",
    "edges",
    "haptics",
    "locate",
];

/// Where the files go: `flockswitch` in the Documents folder ($XDG_DOCUMENTS_DIR, or
/// ~/Documents where that is not set).
pub fn folder() -> PathBuf {
    dirs::document_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Documents")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("flockswitch")
}

/// At most this many settings files are kept in the folder; Save writes nothing more
/// until some are renamed away or deleted under Manage.
pub const MAX_FILES: usize = 10;

/// What Save did.
#[derive(Debug, PartialEq, Eq)]
pub enum Export {
    Saved(PathBuf),
    /// The folder already holds `MAX_FILES` settings files; nothing was written.
    Full,
}

/// Saves `cfg` as a new file in `folder()`.
pub fn export(cfg: &Config) -> Result<Export> {
    let (stamp, when) = now();
    export_to(&folder(), cfg, &host_name(), &stamp, &when)
}

fn export_to(dir: &Path, cfg: &Config, host: &str, stamp: &str, when: &str) -> Result<Export> {
    if list(dir).len() >= MAX_FILES {
        return Ok(Export::Full);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = unused_path(dir, &file_name(host, stamp));
    std::fs::write(&path, export_text(cfg, host, when)?).with_context(|| format!("cannot write {}", path.display()))?;
    log::info!("settings saved to {}", path.display());
    Ok(Export::Saved(path))
}

/// The file's content: a header saying where it came from, then the settings.
pub fn export_text(cfg: &Config, host: &str, when: &str) -> Result<String> {
    // What the setup check set aside is about this computer, not the settings.
    let settings = Config { setup: Default::default(), ..cfg.clone() }.to_text()?;
    Ok(format!(
        "# flockswitch settings\n\
         # Saved by flockswitch {} for Linux on {host}, {when}\n\
         # Load it with Settings → General → Settings backup → Load settings (Linux, Windows or macOS).\n\
         # Channels are set per computer: check them after loading on another computer.\n\n{settings}",
        env!("CARGO_PKG_VERSION")
    ))
}

/// `flockswitch-settings-<host>-<stamp>.toml`, with the host name made safe for a file name.
pub fn file_name(host: &str, stamp: &str) -> String {
    let mut safe: String = host
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .take(40)
        .collect();
    safe = safe.trim_matches('-').to_string();
    if safe.is_empty() {
        safe = "linux".into();
    }
    format!("{PREFIX}{safe}-{stamp}.toml")
}

/// This computer's name, as `hostname` prints it.
fn host_name() -> String {
    ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .chain(std::env::var("HOSTNAME").ok())
        .map(|h| h.trim().to_string())
        .find(|h| !h.is_empty())
        .unwrap_or_else(|| "linux".into())
}

/// Local time now as "20261110-153012" (for the file name) and "2026-11-10 15:30:12";
/// from `date`, else in UTC.
fn now() -> (String, String) {
    let out = std::process::Command::new("date").arg("+%Y%m%d-%H%M%S %Y-%m-%d %H:%M:%S").output();
    if let Some((stamp, when)) = out.ok().filter(|o| o.status.success()).and_then(|o| {
        let text = String::from_utf8(o.stdout).ok()?;
        let (stamp, when) = text.trim().split_once(' ')?;
        Some((stamp.to_string(), when.to_string()))
    }) {
        return (stamp, when);
    }
    let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    utc(secs)
}

/// Seconds since 1970 as UTC (stamp, readable), by the civil-from-days algorithm.
fn utc(secs: u64) -> (String, String) {
    let days = (secs / 86_400) as i64;
    let (h, m, s) = ((secs % 86_400) / 3600, (secs % 3600) / 60, secs % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        format!("{year:04}{month:02}{day:02}-{h:02}{m:02}{s:02}"),
        format!("{year:04}-{month:02}-{day:02} {h:02}:{m:02}:{s:02} UTC"),
    )
}

/// `dir/name`, or `name` with -1, -2… before .toml if that file exists.
fn unused_path(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let stem = name.strip_suffix(".toml").unwrap_or(name);
    (1..).map(|i| dir.join(format!("{stem}-{i}.toml"))).find(|p| !p.exists()).unwrap_or(path)
}

/// A settings file in the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub modified: SystemTime,
}

impl Entry {
    /// The name without .toml, as the list shows it.
    pub fn stem(&self) -> &str {
        let cut = self.name.len().saturating_sub(".toml".len());
        if self.name.is_char_boundary(cut) && self.name[cut..].eq_ignore_ascii_case(".toml") { &self.name[..cut] } else { &self.name }
    }
}

/// The longest name (without .toml) a file can be given.
pub const MAX_NAME: usize = 60;
/// Characters a name cannot have, so it works as a file name on Linux, Windows and macOS.
pub const BAD_CHARS: [char; 9] = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// Why a file cannot be given a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameError {
    Empty,
    TooLong,
    /// The first character it cannot have (a control character shows as '?').
    BadChar(char),
    /// Another file has that name already.
    Taken,
    /// The rename itself failed.
    Failed(String),
}

/// The file name for a new name typed by the user: trimmed, a typed ".toml" taken off,
/// and checked; ".toml" is added back.
pub fn checked_name(typed: &str) -> Result<String, NameError> {
    let mut name = typed.trim();
    if name.len() >= 5 && name.is_char_boundary(name.len() - 5) && name[name.len() - 5..].eq_ignore_ascii_case(".toml") {
        name = name[..name.len() - 5].trim_end();
    }
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.chars().count() > MAX_NAME {
        return Err(NameError::TooLong);
    }
    if let Some(c) = name.chars().find(|c| BAD_CHARS.contains(c) || c.is_control()) {
        return Err(NameError::BadChar(if c.is_control() { '?' } else { c }));
    }
    // A name starting with a dot would hide the file (and "." / ".." are no names).
    if name.starts_with('.') {
        return Err(NameError::BadChar('.'));
    }
    Ok(format!("{name}.toml"))
}

/// Gives the file at `path` a new name (see `checked_name`) in the same folder, never
/// replacing another file. Returns the new path.
pub fn rename(path: &Path, typed: &str) -> Result<PathBuf, NameError> {
    let name = checked_name(typed)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let target = dir.join(&name);
    if target == path {
        return Ok(target);
    }
    // Only the case changes where names ignore case: the target is this very file.
    let same_file = target.exists() && same_inode(path, &target);
    let clash = list(dir).iter().any(|e| e.path != path && e.name.eq_ignore_ascii_case(&name));
    if !same_file && (target.exists() || clash) {
        return Err(NameError::Taken);
    }
    if same_file {
        std::fs::rename(path, &target).map_err(|e| NameError::Failed(e.to_string()))?;
    } else {
        // A hard link fails when the name exists, so a file made meanwhile is never replaced.
        match std::fs::hard_link(path, &target) {
            Ok(()) => std::fs::remove_file(path).map_err(|e| NameError::Failed(e.to_string()))?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(NameError::Taken),
            // A folder without hard links (FAT, some network drives): checked just above.
            Err(_) => std::fs::rename(path, &target).map_err(|e| NameError::Failed(e.to_string()))?,
        }
    }
    log::info!("settings file {} renamed to {name}", path.display());
    Ok(target)
}

fn same_inode(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// What Delete did with a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deleted {
    /// Moved to the desktop's Trash, from where it can be put back.
    Trashed,
    /// There was no Trash to use: deleted for good.
    Removed,
}

/// Moves the file to the Trash (`gio trash`), or deletes it where that does not work.
pub fn delete(path: &Path) -> Result<Deleted> {
    delete_with(path, |p| {
        std::process::Command::new("gio")
            .arg("trash")
            .arg("--")
            .arg(p)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

fn delete_with(path: &Path, trash: impl Fn(&Path) -> bool) -> Result<Deleted> {
    anyhow::ensure!(path.is_file(), "{} is not there any more", path.display());
    if trash(path) && !path.exists() {
        log::info!("settings file {} moved to the trash", path.display());
        return Ok(Deleted::Trashed);
    }
    std::fs::remove_file(path).with_context(|| format!("cannot delete {}", path.display()))?;
    log::info!("settings file {} deleted", path.display());
    Ok(Deleted::Removed)
}

/// The local time zone's offset from UTC in seconds, as `date +%z` says (0 without it).
pub fn local_offset() -> i64 {
    let out = std::process::Command::new("date").arg("+%z").output();
    let text = out.ok().filter(|o| o.status.success()).and_then(|o| String::from_utf8(o.stdout).ok()).unwrap_or_default();
    parse_offset(text.trim()).unwrap_or(0)
}

/// "+0900" or "-0530" in seconds.
fn parse_offset(text: &str) -> Option<i64> {
    let (sign, digits) = match text.as_bytes().first()? {
        b'+' => (1, &text[1..]),
        b'-' => (-1, &text[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m): (i64, i64) = (digits[..2].parse().ok()?, digits[2..].parse().ok()?);
    Some(sign * (h * 3600 + m * 60))
}

/// When a file was saved, as "2026-11-10 15:30" in local time (`offset` from `local_offset`).
pub fn format_time(t: SystemTime, offset: i64) -> String {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (_, when) = utc((secs + offset).max(0) as u64);
    when.trim_end_matches(" UTC").rsplit_once(':').map_or(when.clone(), |(minutes, _)| minutes.to_string())
}

/// The .toml files in `dir`, newest first.
pub fn list(dir: &Path) -> Vec<Entry> {
    let mut files: Vec<Entry> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let toml = name.to_lowercase().ends_with(".toml") && !name.starts_with('.');
            toml.then(|| Entry { modified: e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH), path: e.path(), name })
        })
        .collect();
    files.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.name.cmp(&a.name)));
    files
}

/// Reads a saved file into settings that have passed every check, or says why not.
pub fn read(path: &Path) -> Result<Config> {
    let size = std::fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?.len();
    if size > MAX_SIZE {
        bail!("{} is too large to be a settings file", path.display());
    }
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let text = String::from_utf8(bytes).map_err(|_| anyhow!("not a text file"))?;
    parse(&text)
}

/// Settings from a saved file's text, read as the settings file is and checked the same.
pub fn parse(text: &str) -> Result<Config> {
    let table: toml::Table = text.parse().map_err(|e| anyhow!("not a settings file: {e}"))?;
    if !KNOWN_KEYS.iter().any(|k| table.contains_key(*k)) {
        bail!("no flockswitch settings in it");
    }
    let mut cfg = Config::parse(text)?;
    drop_unusable_shortcuts(&mut cfg);
    cfg.validate()?;
    Ok(cfg)
}

/// A shortcut this computer cannot use (written by another system, say) or one already
/// used by an earlier slot is left empty, so the rest of the file still loads.
fn drop_unusable_shortcuts(cfg: &mut Config) {
    cfg.hotkeys.keys.truncate(3);
    let mut seen = Vec::new();
    let slots = cfg.hotkeys.keys.iter_mut().chain([&mut cfg.locate.hotkey]);
    for key in slots.filter(|k| !k.is_empty()) {
        match crate::hotkey::parse(key) {
            Ok(parsed) if !seen.contains(&parsed) => seen.push(parsed),
            _ => {
                log::info!("settings file: shortcut '{key}' cannot be used here; left empty");
                key.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ShakeKey;

    fn same(a: &Config, b: &Config) -> bool {
        toml::to_string(a).unwrap() == toml::to_string(b).unwrap()
    }

    #[test]
    fn round_trip() {
        let mut c = Config::default();
        c.set("edges", "true").unwrap();
        c.set("edge.right", "2").unwrap();
        c.set("locate", "true").unwrap();
        c.set("locate.shake_key", "alt").unwrap();
        c.set("hotkey.3", "ctrl+shift+f5").unwrap();
        c.set("haptics.intensity", "75").unwrap();
        c.set("follow", "B366,MX Keys").unwrap();
        c.set("language", "en").unwrap();
        let text = export_text(&c, "desk", "2026-11-10 15:30:12").unwrap();
        assert!(text.starts_with("# flockswitch settings\n# Saved by flockswitch "), "{text}");
        assert!(text.contains("for Linux on desk, 2026-11-10 15:30:12"), "{text}");
        let back = parse(&text).unwrap();
        assert!(same(&back, &c), "{text}");
        assert_eq!(back.locate.shake_key, ShakeKey::Alt);
        assert_eq!(back.follow, vec!["B366".to_string(), "MX Keys".to_string()]);
    }

    #[test]
    fn setup_items_set_aside_stay_on_this_computer() {
        let mut c = Config::default();
        c.setup.snoozed = vec!["tray-icon".into()];
        assert!(c.to_text().unwrap().contains("[setup]"));
        let text = export_text(&c, "desk", "2026-11-11 10:00:00").unwrap();
        assert!(!text.contains("[setup]") && !text.contains("tray-icon"), "{text}");
        assert!(parse(&text).unwrap().setup.is_empty());
        // Nothing set aside: no [setup] table in the settings file either.
        assert!(!Config::default().to_text().unwrap().contains("[setup]"));
        // The first check still to come is kept in the settings file, not in a backup.
        let mut c = Config::default();
        c.setup.first_check = true;
        let text = c.to_text().unwrap();
        assert!(text.contains("[setup]") && text.contains("first_check = true") && !text.contains("snoozed"), "{text}");
        assert!(Config::parse(&text).unwrap().setup.first_check);
        assert!(!export_text(&c, "desk", "2026-11-11 10:00:00").unwrap().contains("first_check"));
    }

    #[test]
    fn saves_and_lists_newest_first() {
        let dir = std::env::temp_dir().join(format!("flockswitch-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let c = Config::default();
        let name = file_name("desk", "20261110-153012");
        let first = unused_path(&dir, &name);
        std::fs::write(&first, export_text(&c, "desk", "now").unwrap()).unwrap();
        let second = unused_path(&dir, &name);
        assert_eq!(second, dir.join("flockswitch-settings-desk-20261110-153012-1.toml"));
        std::fs::write(&second, export_text(&c, "desk", "now").unwrap()).unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        std::fs::create_dir_all(dir.join("folder.toml")).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&second).unwrap().set_modified(old).unwrap();
        let names: Vec<String> = list(&dir).into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec![name.clone(), "flockswitch-settings-desk-20261110-153012-1.toml".to_string()]);
        assert!(same(&read(&first).unwrap(), &c));
        assert!(list(&dir.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch(what: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("flockswitch-backup-{what}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn at_most_ten_files() {
        let dir = scratch("limit");
        let c = Config::default();
        std::fs::write(dir.join("notes.txt"), "not counted").unwrap();
        for i in 0..MAX_FILES {
            let done = export_to(&dir, &c, "desk", &format!("20261110-1530{i:02}"), "now").unwrap();
            assert!(matches!(done, Export::Saved(_)), "{i}: {done:?}");
        }
        assert_eq!(list(&dir).len(), MAX_FILES);
        // The eleventh writes nothing.
        assert_eq!(export_to(&dir, &c, "desk", "20261110-160000", "now").unwrap(), Export::Full);
        assert_eq!(list(&dir).len(), MAX_FILES);
        // Deleting one makes room again.
        delete_with(&list(&dir)[0].path, |_| false).unwrap();
        assert!(matches!(export_to(&dir, &c, "desk", "20261110-160000", "now").unwrap(), Export::Saved(_)));
        assert_eq!(export_to(&dir, &c, "desk", "20261110-160001", "now").unwrap(), Export::Full);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_are_checked() {
        assert_eq!(checked_name("  office pc  "), Ok("office pc.toml".to_string()));
        assert_eq!(checked_name("home.TOML"), Ok("home.toml".to_string()));
        assert_eq!(checked_name("집 컴퓨터"), Ok("집 컴퓨터.toml".to_string()));
        assert_eq!(checked_name(""), Err(NameError::Empty));
        assert_eq!(checked_name("   "), Err(NameError::Empty));
        assert_eq!(checked_name(".toml"), Err(NameError::Empty));
        assert_eq!(checked_name(&"가".repeat(MAX_NAME)), Ok(format!("{}.toml", "가".repeat(MAX_NAME))));
        assert_eq!(checked_name(&"a".repeat(MAX_NAME + 1)), Err(NameError::TooLong));
        for c in BAD_CHARS {
            assert_eq!(checked_name(&format!("a{c}b")), Err(NameError::BadChar(c)), "{c}");
        }
        assert_eq!(checked_name("a\tb"), Err(NameError::BadChar('?')));
        assert_eq!(checked_name(".hidden"), Err(NameError::BadChar('.')));
        assert_eq!(checked_name(".."), Err(NameError::BadChar('.')));
    }

    #[test]
    fn rename_never_replaces_a_file() {
        let dir = scratch("rename");
        let (a, b) = (dir.join("a.toml"), dir.join("b.toml"));
        std::fs::write(&a, "enabled = true\n# a\n").unwrap();
        std::fs::write(&b, "enabled = true\n# b\n").unwrap();
        assert_eq!(rename(&a, "b"), Err(NameError::Taken));
        assert_eq!(rename(&a, "B.toml"), Err(NameError::Taken), "names differing in case only clash on other systems");
        assert_eq!(rename(&a, "x/y"), Err(NameError::BadChar('/')));
        assert!(std::fs::read_to_string(&b).unwrap().contains("# b"));
        let moved = rename(&a, " office ").unwrap();
        assert_eq!(moved, dir.join("office.toml"));
        assert!(!a.exists() && std::fs::read_to_string(&moved).unwrap().contains("# a"));
        assert_eq!(rename(&moved, "office"), Ok(moved.clone()), "the same name is no change");
        let entry = list(&dir).into_iter().find(|e| e.path == moved).unwrap();
        assert_eq!(entry.stem(), "office");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_uses_the_trash_when_it_can() {
        let dir = scratch("delete");
        let file = dir.join("old.toml");
        std::fs::write(&file, "enabled = true\n").unwrap();
        // A Trash that takes the file.
        let trash = dir.join("trash");
        std::fs::create_dir_all(&trash).unwrap();
        let into_trash = |p: &Path| std::fs::rename(p, trash.join(p.file_name().unwrap())).is_ok();
        assert_eq!(delete_with(&file, into_trash).unwrap(), Deleted::Trashed);
        assert!(!file.exists() && trash.join("old.toml").exists());
        // No Trash (gio missing or failing): deleted for good.
        std::fs::write(&file, "enabled = true\n").unwrap();
        assert_eq!(delete_with(&file, |_| false).unwrap(), Deleted::Removed);
        assert!(!file.exists());
        assert!(delete_with(&file, |_| false).is_err(), "already gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn times_are_shown_in_local_time() {
        assert_eq!(parse_offset("+0900"), Some(9 * 3600));
        assert_eq!(parse_offset("-0530"), Some(-(5 * 3600 + 30 * 60)));
        assert_eq!(parse_offset("KST"), None);
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_794_324_612);
        assert_eq!(format_time(t, 0), "2026-11-10 15:30");
        assert_eq!(format_time(t, 9 * 3600), "2026-11-11 00:30");
    }

    #[test]
    fn file_names() {
        assert_eq!(file_name("desk", "20261110-153012"), "flockswitch-settings-desk-20261110-153012.toml");
        assert_eq!(file_name(" my pc.local ", "1"), "flockswitch-settings-my-pc-local-1.toml");
        assert_eq!(file_name("../..", "1"), "flockswitch-settings-linux-1.toml");
        assert_eq!(file_name("", "1"), "flockswitch-settings-linux-1.toml");
    }

    #[test]
    fn utc_times() {
        assert_eq!(utc(0), ("19700101-000000".to_string(), "1970-01-01 00:00:00 UTC".to_string()));
        assert_eq!(utc(1_794_324_612).0, "20261110-153012");
        assert_eq!(utc(951_782_400).0, "20000229-000000");
    }

    #[test]
    fn garbage_is_rejected() {
        for bad in [
            "",
            "\u{0}\u{1}\u{2}binary",
            "<html><body>not settings</body></html>",
            "just some words",
            "color = \"blue\"\nsize = 3\n",
            "enabled = \"yes\"\n",
            "[edges]\ndwell_ms = 1\n",
            "[locate]\nshake_time_ms = 99999\n",
            "[buttons.wheel_mode]\non = true\nchannels = [7]\n",
        ] {
            assert!(parse(bad).is_err(), "accepted {bad:?}");
        }
        let dir = std::env::temp_dir().join(format!("flockswitch-backup-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("bad.toml");
        std::fs::write(&file, [0xFFu8, 0xFE, 0x00, 0x41]).unwrap();
        assert!(read(&file).unwrap_err().to_string().contains("not a text file"));
        std::fs::write(&file, vec![b'#'; (MAX_SIZE + 1) as usize]).unwrap();
        assert!(read(&file).unwrap_err().to_string().contains("too large"));
        assert!(read(&dir.join("missing.toml")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_from_other_computers_load() {
        // A newer Mac or Windows version: keys this one lacks are skipped, and a shake
        // key Linux does not offer becomes none.
        let text = "# flockswitch settings\n# Saved by flockswitch 1.5.0 for Windows on OFFICE\n\
                    enabled = true\nlanguage = \"en\"\nstore_only = true\n\
                    [locate]\non = true\nshake_key = \"cmd\"\nshake_time_ms = 1500\nfuture_effect = \"sparkle\"\n\
                    [edges]\nright = 2\non = true\n[windows_only]\nx = 1\n";
        let c = parse(text).unwrap();
        assert!(c.locate.on && c.edges.on);
        assert_eq!((c.locate.shake_key, c.locate.shake_time_ms, c.edges.right), (ShakeKey::None, 1500, 2));
        assert_eq!(c.language, "en");
    }

    #[test]
    fn a_mac_file_loads() {
        // As the macOS version writes it: a Command key for shaking, Command shortcuts,
        // and shortcuts Linux cannot register, which are left empty instead.
        let text = "# flockswitch settings\n# Saved by flockswitch 0.8.0 for macOS on MacBook\n\
                    enabled = true\nlanguage = \"ko\"\ntrigger_device = \"B042\"\n\
                    [hotkeys]\non = true\nkeys = [\"cmd+alt+1\", \"ctrl+alt+⌘\", \"cmd+alt+1\", \"cmd+alt+4\"]\n\
                    [locate]\non = true\non_shake = true\nshake_key = \"cmd\"\nshake_time_ms = 2000\nhotkey = \"cmd+opt+L\"\n";
        let c = parse(text).unwrap();
        assert_eq!(c.hotkeys.keys, vec!["cmd+alt+1".to_string(), String::new(), String::new()]);
        assert_eq!(c.locate.hotkey, "");
        assert_eq!((c.locate.shake_key, c.locate.shake_time_ms), (ShakeKey::None, 2000));
        assert_eq!(c.trigger_device, "B042");
        for other in ["command", "super", "meta", "win", "hyper", "f13"] {
            let c = parse(&format!("[locate]\nshake_key = \"{other}\"\n")).unwrap();
            assert_eq!(c.locate.shake_key, ShakeKey::None, "{other}");
        }
    }
}
