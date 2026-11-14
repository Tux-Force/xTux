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
            event: Event::Message { text: text.to_string() },
        });
    }

    fn bytes(&self, done: u64, total: Option<u64>) {
        let _ = self.outbox.send(ServerMessage::Event {
            id: self.id,
            event: Event::Bytes { done, total },
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
        // A launch handles cancellation itself (it asks the game to quit and waits); other requests just stop.
        let result = if matches!(request, Request::Launch { .. }) {
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
        let _ = self.outbox.send(ServerMessage::Response { id: self.id, result });
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
                    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGINT);
                });
                let status = game.wait().await.map_err(error_body);
                stopper.abort();
                Ok(Response::GameExited { code: status?.code() })
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
            Request::Cancel { .. } => Err(ErrorBody::new(ErrorKind::InvalidRequest, "nested cancel")),
        }
    }
}
