//! Client side of protocol v2, used by `xtux-cli` and the launcher.

use std::path::PathBuf;
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::frame::{self, FrameError};
use crate::{ClientMessage, ErrorBody, Event, Request, Response, ServerMessage, socket_path};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot reach xtux-service: {0}")]
    Connect(std::io::Error),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("xtux-service closed the connection")]
    Closed,
    #[error(transparent)]
    Remote(#[from] ErrorBody),
}

pub struct Client {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl Client {
    /// Connects to a running daemon.
    pub async fn connect() -> Result<Self, ClientError> {
        let stream = UnixStream::connect(socket_path()).await.map_err(ClientError::Connect)?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader,
            writer,
            next_id: 1,
        })
    }

    /// Connects to the daemon, starting it first when it is not running.
    pub async fn connect_or_spawn() -> Result<Self, ClientError> {
        if let Ok(client) = Self::connect().await {
            return Ok(client);
        }
        spawn_service().map_err(ClientError::Connect)?;
        let mut last = None;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            match Self::connect().await {
                Ok(client) => return Ok(client),
                Err(err) => last = Some(err),
            }
        }
        Err(last.unwrap_or(ClientError::Closed))
    }

    /// Sends a request and waits for its response, passing events for it to `on_event`.
    pub async fn request(
        &mut self,
        request: Request,
        mut on_event: impl FnMut(Event),
    ) -> Result<Response, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        frame::write(&mut self.writer, &ClientMessage { id, request }).await?;
        loop {
            let Some(message) = frame::read::<_, ServerMessage>(&mut self.reader).await? else {
                return Err(ClientError::Closed);
            };
            match message {
                ServerMessage::Event { id: event_id, event } if event_id == id => on_event(event),
                ServerMessage::Response { id: reply_id, result } if reply_id == id => {
                    return result.map_err(ClientError::Remote);
                }
                other => tracing::debug!("ignoring message for another request: {other:?}"),
            }
        }
    }
}

/// The daemon binary: `XTUX_SERVICE`, next to the running program, or `xtux-service` from `PATH`.
fn service_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("XTUX_SERVICE") {
        return PathBuf::from(path);
    }
    if let Some(sibling) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("xtux-service")))
        .filter(|path| path.is_file())
    {
        return sibling;
    }
    PathBuf::from("xtux-service")
}

/// Starts the daemon detached from the terminal, so Ctrl+C in a client does not stop it.
fn spawn_service() -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let binary = service_binary();
    tracing::debug!("starting {}", binary.display());
    let log = log_file()
        .and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        })
        .map(Stdio::from)
        .unwrap_or_else(Stdio::null);
    Command::new(binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()
        .map(|_| ())
}

/// Where a daemon started by a client logs: `$XDG_STATE_HOME/xtux/service.log` or `~/.local/state/xtux/service.log`.
pub fn log_file() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?
        .join("xtux");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("service.log"))
}
