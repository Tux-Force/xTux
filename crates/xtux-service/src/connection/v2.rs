//! Protocol v2 (see the `xtux-protocol` crate): JSON requests with ids, responses and events.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use xodus::tokens::TokenManager;
use xtux_protocol::frame;
use xtux_protocol::{ClientMessage, ErrorBody, ErrorKind, Event, Request, Response, ServerMessage};

use crate::activity::Activity;

type Outbox = mpsc::UnboundedSender<ServerMessage>;

/// Forwards library progress of one request as events.
struct EventProgress {
    id: u64,
    outbox: Outbox,
}

impl xtux_games::Progress for EventProgress {
    fn message(&self, text: &str) {
        let _ = self.outbox.send(ServerMessage::Event {
            id: self.id,
            event: Event::Message {
                text: text.to_string(),
            },
        });
    }

    fn bytes(&self, done: u64, total: Option<u64>) {
        let _ = self.outbox.send(ServerMessage::Event {
            id: self.id,
            event: Event::Bytes { done, total },
        });
    }

    fn file(&self, name: &str, done: u64, total: u64) {
        let _ = self.outbox.send(ServerMessage::Event {
            id: self.id,
            event: Event::File {
                name: name.to_string(),
                done,
                total,
            },
        });
    }

    fn file_finished(&self, name: &str) {
        let _ = self.outbox.send(ServerMessage::Event {
            id: self.id,
            event: Event::FileFinished {
                name: name.to_string(),
            },
        });
    }
}

fn error_body(err: xtux_games::Error) -> ErrorBody {
    let kind = match &err {
        xtux_games::Error::SignedOut(_) => ErrorKind::SignedOut,
        xtux_games::Error::NotEntitled(_) => ErrorKind::NotEntitled,
        xtux_games::Error::Cancelled => ErrorKind::Cancelled,
        _ => ErrorKind::Failed,
    };
    ErrorBody::new(kind, err.to_string())
}

/// Serves one v2 connection. The magic of the first frame was already read by the router.
pub async fn serve(
    socket: UnixStream,
    shutdown: CancellationToken,
    client: reqwest::Client,
    tokens: Arc<TokenManager>,
    activity: Activity,
) {
    let (mut reader, mut writer) = socket.into_split();
    let (outbox, mut queue) = mpsc::unbounded_channel::<ServerMessage>();
    let writer_task = tokio::spawn(async move {
        while let Some(message) = queue.recv().await {
            if let Err(err) = frame::write(&mut writer, &message).await {
                tracing::debug!("client went away: {err}");
                break;
            }
        }
    });

    let running: Arc<Mutex<HashMap<u64, CancellationToken>>> = Arc::default();
    let mut first = true;
    loop {
        let message: ClientMessage = if first {
            first = false;
            match frame::read_after_magic(&mut reader).await {
                Ok(message) => message,
                Err(err) => {
                    tracing::warn!("invalid v2 frame: {err}");
                    break;
                }
            }
        } else {
            tokio::select! {
                read = frame::read(&mut reader) => match read {
                    Ok(Some(message)) => message,
                    Ok(None) => break,
                    Err(err) => {
                        tracing::warn!("invalid v2 frame: {err}");
                        break;
                    }
                },
                _ = shutdown.cancelled() => break,
            }
        };

        let id = message.id;
        if let Request::Cancel { target } = message.request {
            let found = running.lock().ok().and_then(|r| r.get(&target).cloned());
            if let Some(token) = found {
                token.cancel();
            }
            let _ = outbox.send(ServerMessage::Response {
                id,
                result: Ok(Response::Cancelled),
            });
            continue;
        }

        let cancel = CancellationToken::new();
        if let Ok(mut r) = running.lock() {
            r.insert(id, cancel.clone());
        }
        let job = Job {
            id,
            outbox: outbox.clone(),
            client: client.clone(),
            tokens: tokens.clone(),
            cancel,
            running: running.clone(),
            _busy: activity.busy(),
        };
        tokio::spawn(job.run(message.request));
    }

    // Requests keep running when the client disconnects (a game outlives the launcher window).
    drop(outbox);
    let _ = writer_task.await;
}

struct Job {
    id: u64,
    outbox: Outbox,
    client: reqwest::Client,
    tokens: Arc<TokenManager>,
    cancel: CancellationToken,
    running: Arc<Mutex<HashMap<u64, CancellationToken>>>,
    /// Keeps the daemon alive while the job runs.
    _busy: crate::activity::Busy,
}

impl Job {
    async fn run(self, request: Request) {
        // Launches and installs handle cancellation themselves (quit the game, remove partial files); other
        // requests just stop.
        let result = if matches!(
            request,
            Request::Launch { .. } | Request::Install { .. } | Request::SignIn { .. }
        ) {
            self.handle(request).await
        } else {
            tokio::select! {
                result = self.handle(request) => result,
                _ = self.cancel.cancelled() => Err(ErrorBody::new(ErrorKind::Cancelled, "cancelled")),
            }
        };
        if let Ok(mut r) = self.running.lock() {
            r.remove(&self.id);
        }
        let _ = self.outbox.send(ServerMessage::Response {
            id: self.id,
            result,
        });
    }

    async fn handle(&self, request: Request) -> Result<Response, ErrorBody> {
        let progress = EventProgress {
            id: self.id,
            outbox: self.outbox.clone(),
        };
        match request {
            Request::Ping => Ok(Response::Pong),
            Request::Status => {
                let user = self.tokens.get_user().ok().map(|u| u.username);
                let signed_in = user.is_some() && self.tokens.get_user_sts_token().is_ok();
                Ok(Response::Status {
                    version: xtux_protocol::VERSION,
                    signed_in,
                    user,
                })
            }
            Request::Library => {
                let settings = xtux_games::settings::Settings::load().map_err(error_body)?;
                let games = xtux_games::library::scan(&settings.library_dirs)
                    .into_iter()
                    .map(|g| xtux_protocol::GameInfo {
                        name: g.name,
                        path: g.path,
                        kind: match g.kind {
                            xtux_games::library::GameKind::Installed => "installed".into(),
                            xtux_games::library::GameKind::Windows => "windows".into(),
                        },
                        title_id: g.title_id,
                        store_id: g.store_id,
                    })
                    .collect();
                Ok(Response::Library { games })
            }
            Request::Launch {
                source,
                wine,
                exe,
                market,
                env,
            } => {
                let options = xtux_games::run::LaunchOptions {
                    source,
                    wine,
                    exe,
                    market,
                    env,
                };
                let game = xtux_games::run::launch(&self.client, &self.tokens, options, &progress)
                    .await
                    .map_err(error_body)?;
                let pid = game.pid;
                let _ = self.outbox.send(ServerMessage::Event {
                    id: self.id,
                    event: Event::GameStarted { pid },
                });
                // A cancelled launch asks the game to quit, like Ctrl+C in a terminal.
                let cancel = self.cancel.clone();
                let stopper = tokio::spawn(async move {
                    cancel.cancelled().await;
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(pid as i32),
                        nix::sys::signal::Signal::SIGINT,
                    );
                });
                let status = game.wait().await.map_err(error_body);
                stopper.abort();
                Ok(Response::GameExited {
                    code: status?.code(),
                })
            }
            Request::PrefixInit {
                wine,
                prefix,
                from_proton,
                skip_graphics,
            } => {
                let options = xtux_games::prefix::InitOptions {
                    wine,
                    prefix,
                    from_proton,
                    skip_graphics,
                };
                let prefix = xtux_games::prefix::init(&self.client, options, &progress)
                    .await
                    .map_err(error_body)?;
                Ok(Response::PrefixReady { prefix })
            }
            Request::Install {
                source,
                destination,
                try_skip_ntfs,
                parallel,
                market,
            } => {
                let source = match source {
                    xtux_protocol::InstallSource::File(path) => {
                        xtux_games::install::Source::File(path)
                    }
                    xtux_protocol::InstallSource::Url(url) => xtux_games::install::Source::Url(url),
                    xtux_protocol::InstallSource::ContentId(id) => {
                        xtux_games::install::Source::ContentId(id)
                    }
                };
                let mut options = xtux_games::install::InstallOptions::new(destination.clone());
                options.try_skip_ntfs = try_skip_ntfs;
                options.parallel = parallel.max(1);
                options.market = market;
                xtux_games::install::install(
                    &self.client,
                    &self.tokens,
                    source,
                    &options,
                    &progress,
                    &self.cancel,
                )
                .await
                .map_err(error_body)?;
                Ok(Response::Installed { destination })
            }
            Request::SignIn { env } => {
                xtux_games::Progress::message(&progress, "Opening the Microsoft sign-in window");
                let mut helper = tokio::process::Command::new(sign_in_helper())
                    .arg("login")
                    .envs(env)
                    .env("XTUX_NO_BANNER", "1")
                    .env("XTUX_DIRECT", "1")
                    .stdin(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| {
                        ErrorBody::new(
                            ErrorKind::Failed,
                            format!("cannot start the sign-in helper: {e}"),
                        )
                    })?;
                let status = tokio::select! {
                    status = helper.wait() => status.map_err(|e| ErrorBody::new(ErrorKind::Failed, e.to_string()))?,
                    _ = self.cancel.cancelled() => {
                        let _ = helper.kill().await;
                        return Err(ErrorBody::new(ErrorKind::Cancelled, "sign-in cancelled"));
                    }
                };
                if !status.success() {
                    return Err(ErrorBody::new(
                        ErrorKind::SignedOut,
                        format!("sign-in did not complete ({status})"),
                    ));
                }
                let user = self.tokens.get_user().ok().map(|u| u.username);
                Ok(Response::Status {
                    version: xtux_protocol::VERSION,
                    signed_in: user.is_some(),
                    user,
                })
            }
            Request::Cancel { .. } => {
                Err(ErrorBody::new(ErrorKind::InvalidRequest, "nested cancel"))
            }
        }
    }
}

/// The sign-in helper: `XTUX_CLI`, `xtux-cli` next to the daemon, or `xtux-cli` from `PATH`.
fn sign_in_helper() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("XTUX_CLI") {
        return path.into();
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("xtux-cli")))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| "xtux-cli".into())
}
