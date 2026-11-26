use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

pub const PROTO_VERSION: u32 = 1;
pub const MAX_FRAME: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}
impl Side {
    pub fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Mac,
    Windows,
    Linux,
    Android,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum InputEvent {
    Motion { dx: f64, dy: f64 },
    Button { button: MouseButton, down: bool },
    Scroll { dx: f64, dy: f64 },
    Key { usage: u16, down: bool },
}
impl InputEvent {
    pub(crate) fn valid(&self) -> bool {
        match self {
            Self::Motion { dx, dy } | Self::Scroll { dx, dy } => dx.is_finite() && dy.is_finite(),
            _ => true,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Msg {
    Hello {
        proto: u32,
        id: String,
        name: String,
        os: Os,
        app_version: String,
        #[serde(default)]
        role: crate::Role,
    },
    PairRequest,
    /// Master to sub: where the sub sits around the master's screen (None: nowhere).
    Layout {
        side: Option<Side>,
    },
    /// Master to sub: the settings the sub follows.
    MasterSettings(crate::MasterSettings),
    PairAccept,
    PairReject,
    Ping {
        t: u64,
    },
    Pong {
        t: u64,
    },
    Enter {
        side: Side,
        pos: f64,
    },
    Leave {
        pos: f64,
    },
    Input(InputEvent),
    /// Text on the sender's clipboard, sent just before control moves.
    Clipboard {
        text: String,
    },
    /// Start of a larger clipboard transfer (files, an image, long text).
    ClipStart {
        id: u32,
        kind: crate::transfer::ClipKind,
        files: Vec<crate::transfer::FileMeta>,
    },
    /// A piece of file `file` of transfer `id`, base64.
    ClipChunk {
        id: u32,
        file: u32,
        data: String,
    },
    ClipDone {
        id: u32,
    },
    ReleaseAll,
    Bye {
        reason: String,
    },
}
pub fn write_frame(mut writer: impl Write, msg: &Msg) -> io::Result<()> {
    let bytes = serde_json::to_vec(msg).map_err(io::Error::other)?;
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds 64 KiB",
        ));
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()
}
pub fn read_frame(mut reader: impl Read) -> io::Result<Msg> {
    let mut len = [0; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds 64 KiB",
        ));
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
/// Incremental decoding preserves partial frames across socket timeouts.
pub(crate) fn take_frame(buffer: &mut Vec<u8>) -> io::Result<Option<Msg>> {
    if buffer.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes(buffer[..4].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds 64 KiB",
        ));
    }
    if buffer.len() < len + 4 {
        return Ok(None);
    }
    let msg = serde_json::from_slice(&buffer[4..len + 4]).map_err(io::Error::other)?;
    buffer.drain(..len + 4);
    Ok(Some(msg))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn framing() {
        let msg = Msg::Input(InputEvent::Key {
            usage: 4,
            down: true,
        });
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &msg).unwrap();
        assert_eq!(read_frame(bytes.as_slice()).unwrap(), msg);
        let mut partial = bytes[..5].to_vec();
        assert!(take_frame(&mut partial).unwrap().is_none());
        partial.extend_from_slice(&bytes[5..]);
        assert_eq!(take_frame(&mut partial).unwrap(), Some(msg));
        assert!(read_frame(((MAX_FRAME + 1) as u32).to_be_bytes().as_slice()).is_err());
        assert!(write_frame(
            Vec::new(),
            &Msg::Bye {
                reason: "x".repeat(MAX_FRAME)
            }
        )
        .is_err());
    }
}
