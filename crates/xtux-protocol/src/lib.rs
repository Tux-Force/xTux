//! Protocol v2 between `xtux-service` (the daemon) and its clients (`xtux-cli`, the launcher).
//!
//! Frames on the daemon's unix socket: `u32` LE magic [`MAGIC`] (`"XTX2"`), `u32` LE payload length (at most
//! [`MAX_FRAME`]), then a JSON payload: a [`ClientMessage`] from clients, a [`ServerMessage`] from the daemon.
//! The older XML frames of xgameruntime use another magic on the same socket and are not affected.
//!
//! Every request carries an id chosen by the client. The daemon answers with exactly one
//! [`ServerMessage::Response`] for it and may send [`ServerMessage::Event`]s with the same id before and after
//! (progress, game started and exited). A [`Request::Cancel`] stops a running request.

pub mod client;
pub mod frame;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use client::{Canceller, Client, ClientError};

/// `"XTX2"` read as a little endian `u32`.
pub const MAGIC: u32 = u32::from_le_bytes(*b"XTX2");
/// Largest accepted payload.
pub const MAX_FRAME: u32 = 16 * 1024 * 1024;
/// Version of this protocol, reported by [`Response::Status`].
pub const VERSION: u32 = 2;

/// Socket of the daemon: `$XDG_RUNTIME_DIR/xtux.sock`, or `/tmp/xtux-<uid>.sock` without a runtime dir.
pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("xtux.sock"),
        None => PathBuf::from(format!("/tmp/xtux-{}.sock", current_uid())),
    }
}

fn current_uid() -> u32 {
    // getuid never fails; avoid a libc dependency by reading it from /proc.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
        })
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientMessage {
    pub id: u64,
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Ping,
    /// Daemon version and who is signed in.
    Status,
    /// Games in the library folders of the user's settings.
    Library,
    /// Starts a game; answered when the game has exited, with `GameStarted`/`GameExited` events in between.
    Launch {
        source: PathBuf,
        wine: String,
        exe: Option<String>,
        market: Option<String>,
        /// Extra environment for the game, for example `WINEPREFIX`.
        #[serde(default)]
        env: Vec<(String, String)>,
        /// Command line arguments for the game.
        #[serde(default)]
        args: Vec<String>,
    },
    /// Prepares a Wine prefix (DXVK, vkd3d-proton, certificates).
    PrefixInit {
        wine: String,
        prefix: Option<String>,
        from_proton: Option<String>,
        skip_graphics: bool,
    },
    /// Installs a game by streaming its package; answered when done, with progress events in between.
    Install {
        source: InstallSource,
        destination: PathBuf,
        try_skip_ntfs: bool,
        parallel: usize,
        market: Option<String>,
    },
    /// Signs a user in: the daemon opens the Microsoft sign-in window in a helper process (`xtux-cli login`)
    /// with this environment (it needs the client's display) and answers with the new status.
    SignIn {
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    /// Stops the request with this id.
    Cancel {
        target: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GameInfo {
    pub name: String,
    /// Game folder, the `source` of a launch.
    pub path: PathBuf,
    /// `installed` (by xTux) or `windows` (installed by the Windows Xbox app).
    pub kind: String,
    pub title_id: Option<String>,
    pub store_id: Option<String>,
}

/// What to install from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum InstallSource {
    /// A local package file (absolute path).
    File(PathBuf),
    /// A direct `http(s)://` URL of the package.
    Url(String),
    /// A content id, looked up for the signed in user.
    ContentId(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Response {
        id: u64,
        result: Result<Response, ErrorBody>,
    },
    Event {
        id: u64,
        event: Event,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Pong,
    Status {
        version: u32,
        signed_in: bool,
        user: Option<String>,
    },
    GameExited {
        code: Option<i32>,
    },
    PrefixReady {
        prefix: PathBuf,
    },
    Installed {
        destination: PathBuf,
    },
    Library {
        games: Vec<GameInfo>,
    },
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Message {
        text: String,
    },
    Bytes {
        done: u64,
        total: Option<u64>,
    },
    GameStarted {
        pid: u32,
    },
    /// Byte progress of one file being downloaded.
    File {
        name: String,
        done: u64,
        total: u64,
    },
    FileFinished {
        name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct ErrorBody {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    SignedOut,
    NotEntitled,
    Cancelled,
    InvalidRequest,
    Failed,
}

impl ErrorBody {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape_is_stable() {
        let msg = ClientMessage {
            id: 7,
            request: Request::Cancel { target: 3 },
        };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"id":7,"request":{"type":"cancel","target":3}}"#
        );
        let reply = ServerMessage::Response {
            id: 7,
            result: Ok(Response::Cancelled),
        };
        let text = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<ServerMessage>(&text).unwrap(), reply);
    }

    #[test]
    fn magic_spells_xtx2() {
        assert_eq!(MAGIC.to_le_bytes(), *b"XTX2");
    }
}
