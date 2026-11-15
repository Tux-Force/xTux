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
    /// The store catalog: PC Game Pass and the games the account owns (cached for a day unless `refresh`).
    Catalog {
        market: Option<String>,
        #[serde(default)]
        refresh: bool,
    },
    /// The download queue: list it or change it; answered with the queue.
    Downloads(DownloadCommand),
    /// Deletes a game folder of the library with everything in it.
    Uninstall {
        path: PathBuf,
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
    /// Why the last license check failed (for example not entitled); `None` when it passed or never ran.
    #[serde(default)]
    pub license_problem: Option<String>,
    /// Anti-cheat found in the game folder, for example `Ricochet (Activision)`.
    #[serde(default)]
    pub anticheat: Option<String>,
    /// The anti-cheat is a kernel driver Wine cannot load: the game runs on Windows only.
    #[serde(default)]
    pub anticheat_blocks: bool,
}

/// A change of the download queue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DownloadCommand {
    List,
    /// Queues a game into `library/<name>`; `library` must be a library folder of the settings.
    Add {
        product_id: String,
        name: String,
        content_id: String,
        library: PathBuf,
    },
    Pause {
        id: u64,
    },
    Resume {
        id: u64,
    },
    /// Takes the item out; an unfinished download folder the queue created is deleted.
    Remove {
        id: u64,
    },
    Move {
        id: u64,
        up: bool,
    },
    ClearDone,
}

/// An item of the download queue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DownloadItem {
    pub id: u64,
    pub product_id: String,
    pub name: String,
    pub destination: PathBuf,
    /// `queued`, `running`, `pausing`, `removing`, `paused`, `failed` or `done`.
    pub state: String,
    pub done: u64,
    pub total: Option<u64>,
    /// Bytes per second while running.
    #[serde(default)]
    pub speed: u64,
    /// Last step or error.
    pub message: String,
}

/// A title of the store catalog.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogTitle {
    /// Store ID.
    pub product_id: String,
    pub name: String,
    pub publisher: String,
    pub description: String,
    pub category: String,
    /// Content ID of the PC package (what an install downloads).
    pub content_id: Option<String>,
    pub download_size: Option<u64>,
    pub poster_url: Option<String>,
    pub hero_url: Option<String>,
    pub in_game_pass: bool,
    pub owned: bool,
    /// A GDK package xTux can install.
    pub installable: bool,
    /// Format of the PC package (`MSIXVC`, `EAppx`, `Appx` …); `None` when the Store lists no PC package.
    #[serde(default)]
    pub package_format: Option<String>,
    /// Kernel anti-cheat guessed from xTux's own title list (only when turned on in the settings).
    pub windows_only: Option<String>,
    /// Folder of the installed game, when it is in the library.
    pub installed: Option<PathBuf>,
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
    Uninstalled {
        path: PathBuf,
    },
    Catalog {
        titles: Vec<CatalogTitle>,
    },
    Downloads {
        items: Vec<DownloadItem>,
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
