//! Clipboard contents too large for one message (files, images, long text), sent in
//! pieces over the peer link just before control moves. Files are read from disk a
//! piece at a time, so a large transfer never sits in memory whole.

use crate::proto::Msg;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
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
    Files(Vec<PathBuf>, usize, Option<File>),
}

/// One transfer on its way out; `next` gives its messages in order.
pub struct Outgoing {
    id: u32,
    start: Option<Msg>,
    source: Source,
    done: bool,
    pub total: u64,
    pub sent: u64,
}

impl Outgoing {
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
        Ok(Self {
            id,
            start: Some(Msg::ClipStart { id, kind: ClipKind::Files, files: metas }),
            source: Source::Files(kept, 0, None),
            done: false,
            total,
            sent: 0,
        })
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
            source: Source::Bytes(data, 0),
            done: false,
            total,
            sent: 0,
        }
    }

    /// Whether the last message (ClipDone) was given out.
    pub fn finished(&self) -> bool {
        self.done
    }

    /// The next message, or None when everything was given out.
    pub fn next(&mut self) -> Result<Option<Msg>> {
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
                    let piece = (0u32, data[*at..end].to_vec());
                    *at = end;
                    Some(piece)
                }
            }
            Source::Files(paths, index, file) => loop {
                if *index >= paths.len() {
                    break None;
                }
                if file.is_none() {
                    *file = Some(File::open(&paths[*index]).with_context(|| format!("opening {}", paths[*index].display()))?);
                }
                let mut buf = vec![0u8; CHUNK];
                let n = file.as_mut().unwrap().read(&mut buf)?;
                if n == 0 {
                    *file = None;
                    *index += 1;
                    continue;
                }
                buf.truncate(n);
                break Some((*index as u32, buf));
            },
        };
        match piece {
            Some((file, data)) => {
                self.sent += data.len() as u64;
                Ok(Some(Msg::ClipChunk { id, file, data: STANDARD.encode(data) }))
            }
            None => {
                self.done = true;
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
        let dir = work.join(format!("incoming-{id}"));
        let _ = std::fs::remove_dir_all(&dir);
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
        for (meta, written) in self.files.iter().zip(&self.written) {
            if *written != meta.size {
                bail!("{} is incomplete", meta.name);
            }
        }
        let result = match self.kind {
            ClipKind::Files => {
                std::fs::create_dir_all(dest)?;
                let mut saved = Vec::new();
                for (i, meta) in self.files.iter().enumerate() {
                    let from = self.dir.join(i.to_string());
                    if !from.exists() {
                        File::create(&from)?;
                    }
                    let to = unique_path(dest, &meta.name);
                    if std::fs::rename(&from, &to).is_err() {
                        std::fs::copy(&from, &to)?;
                    }
                    saved.push(to);
                }
                Received::Files(saved)
            }
            ClipKind::Image => Received::Image(std::fs::read(self.dir.join("0"))?),
            ClipKind::Text => Received::Text(String::from_utf8(std::fs::read(self.dir.join("0"))?)?),
        };
        let _ = std::fs::remove_dir_all(&self.dir);
        Ok(result)
    }

    /// Throws away whatever arrived.
    pub fn abandon(self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
