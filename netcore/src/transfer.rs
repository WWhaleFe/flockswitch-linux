//! Clipboard contents too large for one message (files, images, long text), sent in
//! pieces over the peer link just before control moves. Files are read from disk a
//! piece at a time, so a large transfer never sits in memory whole.

use crate::proto::Msg;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// Bytes per piece. Base64 makes it about 43 KiB, well inside a 64 KiB frame.
pub const CHUNK: usize = 32 * 1024;
/// Text up to this size goes in one plain clipboard message.
pub const INLINE_TEXT: usize = 40 * 1024;
/// Longest text carried at all.
pub const MAX_TEXT: u64 = 1024 * 1024;
/// Fixed size limit of shared files and images, in MB (no setting since test.3).
pub const LIMIT_MB: u32 = 100;
/// Value written to `clipboard_limit_mb`, kept only so old config files still load.
pub const DEFAULT_LIMIT_MB: u32 = LIMIT_MB;
/// Most files in one transfer.
pub const MAX_FILES: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipKind {
    Files,
    Image,
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    pub name: String,
    pub size: u64,
}

/// A file name received from a peer, cut down to a plain name: no folders, no "..",
/// no control characters, no characters Windows refuses. None when nothing is left.
pub fn safe_name(name: &str) -> Option<String> {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = last
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches('.').trim().to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }
    Some(cleaned.chars().take(200).collect())
}

/// `dir/name`, or `dir/stem (2).ext` and so on when that name is taken.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap()
}

enum Source {
    Bytes(Vec<u8>, usize),
    Files(FileSource),
}

struct FileSource {
    paths: Vec<PathBuf>,
    sizes: Vec<u64>,
    index: usize,
    file: Option<File>,
    read: u64,
    hash: Sha256,
}

/// One transfer on its way out; `next` gives its messages in order.
pub struct Outgoing {
    id: u32,
    start: Option<Msg>,
    pending: Option<Msg>,
    source: Source,
    done: bool,
    sent_sig: Option<Vec<u8>>,
    pub total: u64,
    pub sent: u64,
}

impl Outgoing {
    pub(crate) fn id(&self) -> u32 {
        self.id
    }
    /// Regular files only (folders and anything unreadable are left out). Err when nothing
    /// is left or the total is over `limit` bytes.
    pub fn files(id: u32, paths: &[PathBuf], limit: u64) -> Result<Self> {
        let mut metas = Vec::new();
        let mut kept = Vec::new();
        let mut total = 0u64;
        for p in paths.iter().take(MAX_FILES) {
            let Ok(meta) = std::fs::metadata(p) else { continue };
            if !meta.is_file() {
                continue;
            }
            let Some(name) = p.file_name().and_then(|n| n.to_str()).and_then(safe_name) else {
                continue;
            };
            total += meta.len();
            metas.push(FileMeta { name, size: meta.len() });
            kept.push(p.clone());
        }
        if kept.is_empty() {
            bail!("no files to send");
        }
        if total > limit {
            bail!("too large: {total} bytes");
        }
        let sizes = metas.iter().map(|m| m.size).collect();
        let mut hash = Sha256::new();
        hash.update(b"files");
        hash.update((kept.len() as u64).to_le_bytes());
        Ok(Self {
            id,
            start: Some(Msg::ClipStart { id, kind: ClipKind::Files, files: metas }),
            pending: None,
            source: Source::Files(FileSource { paths: kept, sizes, index: 0, file: None, read: 0, hash }),
            done: false,
            sent_sig: None,
            total,
            sent: 0,
        })
    }

    /// The regular files that passed the count, name and size checks, in wire order.
    pub(crate) fn file_paths(&self) -> Option<&[PathBuf]> {
        match &self.source { Source::Files(files) => Some(&files.paths), _ => None }
    }

    /// An image (PNG) or long text, held in memory (both are bounded by their limits).
    pub fn bytes(id: u32, kind: ClipKind, name: &str, data: Vec<u8>) -> Self {
        let total = data.len() as u64;
        Self {
            id,
            start: Some(Msg::ClipStart {
                id,
                kind,
                files: vec![FileMeta { name: name.into(), size: total }],
            }),
            pending: None,
            source: Source::Bytes(data, 0),
            done: false,
            sent_sig: None,
            total,
            sent: 0,
        }
    }

    /// Fingerprint of the exact file bytes handed to the transport.
    pub(crate) fn sent_signature(&self) -> Option<&[u8]> { self.sent_sig.as_deref() }

    /// Whether the last message (ClipDone) was given out.
    pub fn finished(&self) -> bool {
        self.done && self.pending.is_none()
    }

    /// Retain a frame rejected by the bounded link queue without rereading its bytes.
    pub(crate) fn retry(&mut self, msg: Msg) { self.pending = Some(msg); }

    /// The next message, or None when everything was given out.
    pub fn next(&mut self) -> Result<Option<Msg>> {
        if let Some(msg) = self.pending.take() { return Ok(Some(msg)); }
        if let Some(start) = self.start.take() {
            return Ok(Some(start));
        }
        if self.done {
            return Ok(None);
        }
        let id = self.id;
        let piece = match &mut self.source {
            Source::Bytes(data, at) => {
                if *at >= data.len() {
                    None
                } else {
                    let end = (*at + CHUNK).min(data.len());
                    let piece = (0u32, STANDARD.encode(&data[*at..end]), end - *at);
                    *at = end;
                    Some(piece)
                }
            }
            Source::Files(source) => loop {
                if source.index >= source.paths.len() { break None; }
                if source.file.is_none() {
                    source.file = Some(File::open(&source.paths[source.index])
                        .with_context(|| format!("opening {}", source.paths[source.index].display()))?);
                    source.hash.update(source.sizes[source.index].to_le_bytes());
                    source.read = 0;
                }
                let mut buf = [0u8; CHUNK];
                let n = source.file.as_mut().unwrap().read(&mut buf)?;
                if n == 0 {
                    anyhow::ensure!(source.read == source.sizes[source.index], "clipboard file changed size");
                    source.file = None;
                    source.index += 1;
                    continue;
                }
                source.read += n as u64;
                anyhow::ensure!(source.read <= source.sizes[source.index], "clipboard file changed size");
                source.hash.update(&buf[..n]);
                break Some((source.index as u32, STANDARD.encode(&buf[..n]), n));
            },
        };
        match piece {
            Some((file, data, size)) => {
                self.sent += size as u64;
                Ok(Some(Msg::ClipChunk { id, file, data }))
            }
            None => {
                self.done = true;
                if let Source::Files(files) = &self.source {
                    self.sent_sig = Some(files.hash.clone().finalize().to_vec());
                }
                self.source = Source::Bytes(Vec::new(), 0);
                Ok(Some(Msg::ClipDone { id }))
            }
        }
    }
}

/// What a finished transfer brought.
#[derive(Debug, PartialEq)]
pub enum Received {
    /// Saved files, in the order sent.
    Files(Vec<PathBuf>),
    Image(Vec<u8>),
    Text(String),
}

/// One transfer on its way in, written to a folder of its own until it is complete.
pub struct Incoming {
    pub id: u32,
    pub kind: ClipKind,
    files: Vec<FileMeta>,
    dir: PathBuf,
    written: Vec<u64>,
    pub total: u64,
    pub received: u64,
}

impl Incoming {
    /// Checks the announced sizes against `limit` (bytes) before anything is written.
    pub fn start(id: u32, kind: ClipKind, files: Vec<FileMeta>, limit: u64, work: &Path) -> Result<Self> {
        if files.is_empty() || files.len() > MAX_FILES {
            bail!("bad file count");
        }
        if kind != ClipKind::Files && files.len() != 1 {
            bail!("one item expected");
        }
        let total: u64 = files.iter().map(|f| f.size).sum();
        let cap = if kind == ClipKind::Text { MAX_TEXT } else { limit };
        if total > cap {
            bail!("too large: {total} bytes");
        }
        let files = files
            .into_iter()
            .map(|f| safe_name(&f.name).map(|name| FileMeta { name, size: f.size }))
            .collect::<Option<Vec<_>>>()
            .context("bad file name")?;
        // A worker may still be finishing an older transfer with the same wire id.
        let dir = unique_path(work, &format!("incoming-{id}"));
        std::fs::create_dir_all(&dir)?;
        let written = vec![0; files.len()];
        Ok(Self { id, kind, files, dir, written, total, received: 0 })
    }

    pub fn chunk(&mut self, file: u32, data: &str) -> Result<()> {
        let index = file as usize;
        let meta = self.files.get(index).context("bad file index")?;
        let bytes = STANDARD.decode(data)?;
        if self.written[index] + bytes.len() as u64 > meta.size {
            bail!("more data than announced");
        }
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(index.to_string()))?;
        out.write_all(&bytes)?;
        self.written[index] += bytes.len() as u64;
        self.received += bytes.len() as u64;
        Ok(())
    }

    /// Checks every size; files move into `dest` (made if missing) under their own,
    /// unique names; an image or text is read back into memory.
    pub fn finish(self, dest: &Path) -> Result<Received> {
        self.finish_with(dest, &mut || false)
    }

    pub(crate) fn finish_cancellable(self, dest: &Path, cancel: &AtomicBool) -> Result<Received> {
        self.finish_with(dest, &mut || cancel.load(Ordering::Acquire))
    }

    fn finish_with(self, dest: &Path, cancelled: &mut impl FnMut() -> bool) -> Result<Received> {
        anyhow::ensure!(!cancelled(), "cancelled");
        for (meta, written) in self.files.iter().zip(&self.written) {
            if *written != meta.size { bail!("{} is incomplete", meta.name); }
        }
        let result = match self.kind {
            ClipKind::Files => {
                std::fs::create_dir_all(dest)?;
                let mut saved = SavedPaths(Vec::new());
                for (i, meta) in self.files.iter().enumerate() {
                    anyhow::ensure!(!cancelled(), "cancelled");
                    let from = self.dir.join(i.to_string());
                    if !from.exists() { File::create(&from)?; }
                    let to = unique_path(dest, &meta.name);
                    // Track even a partially copied destination for rollback.
                    saved.0.push(to.clone());
                    if std::fs::rename(&from, &to).is_err() {
                        copy_cancellable(&from, &to, cancelled)?;
                    }
                }
                anyhow::ensure!(!cancelled(), "cancelled");
                Received::Files(std::mem::take(&mut saved.0))
            }
            ClipKind::Image | ClipKind::Text => {
                let mut data = Vec::new();
                let mut file = File::open(self.dir.join("0"))?;
                let mut chunk = [0; CHUNK];
                loop {
                    anyhow::ensure!(!cancelled(), "cancelled");
                    let n = file.read(&mut chunk)?;
                    if n == 0 { break; }
                    data.extend_from_slice(&chunk[..n]);
                }
                if self.kind == ClipKind::Image { Received::Image(data) }
                else { Received::Text(String::from_utf8(data)?) }
            }
        };
        Ok(result)
    }

    /// Throws away whatever arrived.
    pub fn abandon(self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A destination transaction, rolled back unless all files are committed.
struct SavedPaths(Vec<PathBuf>);
impl Drop for SavedPaths {
    fn drop(&mut self) {
        for path in &self.0 { let _ = std::fs::remove_file(path); }
    }
}
fn copy_cancellable(from: &Path, to: &Path, cancelled: &mut impl FnMut() -> bool) -> Result<()> {
    let mut input = File::open(from)?;
    let mut output = File::create(to)?;
    let mut buffer = [0; CHUNK];
    loop {
        anyhow::ensure!(!cancelled(), "cancelled");
        let n = input.read(&mut buffer)?;
        if n == 0 { break; }
        output.write_all(&buffer[..n])?;
    }
    Ok(())
}

impl Drop for Incoming {
    fn drop(&mut self) {
        // Also covers failed finish, superseded queued work and shutdown.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_between_published_files_rolls_back_the_destination() {
        let work = temp("cancel-moves");
        let dest = work.join("dest");
        let incoming = Incoming::start(10, ClipKind::Files,
            vec![FileMeta { name: "first".into(), size: 0 }, FileMeta { name: "second".into(), size: 0 }],
            100, &work).unwrap();
        let result = incoming.finish_with(&dest, &mut || dest.join("first").exists());
        assert!(result.is_err());
        assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 0);
        assert!(!work.join("incoming-10").exists());
        std::fs::remove_dir_all(work).unwrap();
    }

    #[test]
    fn fallback_copy_checks_cancellation_between_chunks() {
        let work = temp("cancel-copy");
        let from = work.join("source");
        let to = work.join("destination");
        std::fs::write(&from, vec![7; CHUNK * 8]).unwrap();
        let saved = SavedPaths(vec![to.clone()]);
        let mut checks = 0;
        let result = copy_cancellable(&from, &to, &mut || { checks += 1; checks > 2 });
        assert!(result.is_err());
        assert_eq!(std::fs::metadata(&to).unwrap().len(), (CHUNK * 2) as u64);
        drop(saved);
        assert!(!to.exists());
        assert!(from.exists());
        std::fs::remove_dir_all(work).unwrap();
    }

    #[test]
    fn changing_a_file_size_aborts_before_clip_done() {
        let dir = temp("changed-size");
        let path = dir.join("file");
        std::fs::write(&path, b"before").unwrap();
        let mut out = Outgoing::files(20, &[path.clone()], 100).unwrap();
        assert!(matches!(out.next().unwrap(), Some(Msg::ClipStart { .. })));
        std::fs::write(path, b"larger now").unwrap();
        assert!(out.next().is_err());
        assert!(!out.finished());
        assert!(out.sent_signature().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reused_transfer_ids_do_not_remove_another_workers_files() {
        let work = temp("reused-id");
        let meta = vec![FileMeta { name: "text".into(), size: 3 }];
        let mut old = Incoming::start(1, ClipKind::Text, meta.clone(), 100, &work).unwrap();
        old.chunk(0, &STANDARD.encode(b"old")).unwrap();
        let mut new = Incoming::start(1, ClipKind::Text, meta, 100, &work).unwrap();
        new.chunk(0, &STANDARD.encode(b"new")).unwrap();
        assert_ne!(old.dir, new.dir);
        drop(old);
        assert_eq!(new.finish(&work.join("dest")).unwrap(), Received::Text("new".into()));
        assert_eq!(std::fs::read_dir(&work).unwrap().count(), 0);
    }

    #[test]
    fn sent_bytes_are_released_and_no_chunk_exceeds_the_wire_limit() {
        let data = vec![7; CHUNK * 4 + 1];
        let mut outgoing = Outgoing::bytes(99, ClipKind::Image, "image", data);
        let mut chunks = 0;
        while let Some(msg) = outgoing.next().unwrap() {
            if let Msg::ClipChunk { data, .. } = &msg {
                assert!(STANDARD.decode(data).unwrap().len() <= CHUNK);
                chunks += 1;
            }
            let mut wire = Vec::new();
            crate::proto::write_frame(&mut wire, &msg).unwrap();
        }
        assert_eq!(chunks, 5);
        assert!(matches!(&outgoing.source, Source::Bytes(bytes, 0) if bytes.capacity() == 0));
    }

    #[test]
    fn failed_finish_cleans_up_incomplete_files() {
        let work = temp("incomplete-cleanup");
        let mut incoming = Incoming::start(90, ClipKind::Text,
            vec![FileMeta { name: "text".into(), size: 5 }], 100, &work).unwrap();
        incoming.chunk(0, &STANDARD.encode(b"part")).unwrap();
        assert!(incoming.finish(&work.join("dest")).is_err());
        assert_eq!(std::fs::read_dir(&work).unwrap().count(), 0);
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flock-transfer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn run(mut out: Outgoing, limit: u64, work: &Path, dest: &Path) -> Result<Received> {
        let mut incoming = None;
        while let Some(msg) = out.next()? {
            match msg {
                Msg::ClipStart { id, kind, files } => incoming = Some(Incoming::start(id, kind, files, limit, work)?),
                Msg::ClipChunk { file, data, .. } => incoming.as_mut().unwrap().chunk(file, &data)?,
                Msg::ClipDone { .. } => return incoming.take().unwrap().finish(dest),
                _ => unreachable!(),
            }
        }
        unreachable!()
    }

    #[test]
    fn names_are_cut_to_plain_file_names() {
        assert_eq!(safe_name("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(safe_name("C:\\Users\\me\\a:b?.txt").as_deref(), Some("a_b_.txt"));
        assert_eq!(safe_name(".."), None);
        assert_eq!(safe_name("  "), None);
        assert_eq!(safe_name("보고서.pdf").as_deref(), Some("보고서.pdf"));
    }

    #[test]
    fn files_round_trip_and_names_do_not_clash() {
        let src = temp("src");
        let work = temp("work");
        let dest = temp("dest");
        let big: Vec<u8> = (0..(CHUNK * 3 + 17)).map(|i| (i % 251) as u8).collect();
        std::fs::write(src.join("big.bin"), &big).unwrap();
        std::fs::write(src.join("empty.txt"), b"").unwrap();
        std::fs::create_dir_all(src.join("folder")).unwrap();
        std::fs::write(dest.join("big.bin"), b"already here").unwrap();
        let paths = vec![src.join("big.bin"), src.join("empty.txt"), src.join("folder")];
        let out = Outgoing::files(1, &paths, 10 << 20).unwrap();
        let Received::Files(saved) = run(out, 10 << 20, &work, &dest).unwrap() else { panic!() };
        assert_eq!(saved, vec![dest.join("big (2).bin"), dest.join("empty.txt")]);
        assert_eq!(std::fs::read(&saved[0]).unwrap(), big);
        assert_eq!(std::fs::read(dest.join("big.bin")).unwrap(), b"already here");
    }

    #[test]
    fn limits_hold_on_both_sides() {
        let src = temp("limit");
        std::fs::write(src.join("a.bin"), vec![1u8; 2000]).unwrap();
        assert!(Outgoing::files(1, &[src.join("a.bin")], 1000).is_err());
        let work = temp("limit-work");
        let big = vec![FileMeta { name: "a.bin".into(), size: 2000 }];
        assert!(Incoming::start(1, ClipKind::Files, big, 1000, &work).is_err());
        let mut small = Incoming::start(2, ClipKind::Files, vec![FileMeta { name: "a".into(), size: 3 }], 1000, &work).unwrap();
        assert!(small.chunk(0, &STANDARD.encode([1, 2, 3, 4])).is_err());
        small.abandon();
    }

    #[test]
    fn image_and_text_round_trip() {
        let work = temp("img-work");
        let dest = temp("img-dest");
        let png = vec![137u8, 80, 78, 71, 1, 2, 3];
        let out = Outgoing::bytes(3, ClipKind::Image, "clipboard.png", png.clone());
        assert_eq!(run(out, 1 << 20, &work, &dest).unwrap(), Received::Image(png));
        let text = "가".repeat(30_000);
        let out = Outgoing::bytes(4, ClipKind::Text, "clipboard.txt", text.clone().into_bytes());
        assert_eq!(run(out, 0, &work, &dest).unwrap(), Received::Text(text));
    }
}
