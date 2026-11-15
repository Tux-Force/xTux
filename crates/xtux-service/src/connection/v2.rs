//! Protocol v2 (see the `xtux-protocol` crate): JSON requests with ids, responses and events.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use xodus::tokens::TokenManager;
use xtux_protocol::frame;
use xtux_protocol::{ClientMessage, ErrorBody, ErrorKind, Event, Request, Response, ServerMessage};

use crate::activity::Activity;

/// Game folders a launch or install of any client is using; they cannot be deleted meanwhile.
static IN_USE: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Mutex::default);

/// Marks a game folder as in use until dropped.
struct InUse(PathBuf);

impl InUse {
    fn new(path: &Path) -> Self {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if let Ok(mut set) = IN_USE.lock() {
            set.insert(path.clone());
        }
        Self(path)
    }

    fn contains(path: &Path) -> bool {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        IN_USE.lock().is_ok_and(|set| set.contains(&path))
    }
}

impl Drop for InUse {
    fn drop(&mut self) {
        if let Ok(mut set) = IN_USE.lock() {
            set.remove(&self.0);
        }
    }
}

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
            Request::Downloads(command) => {
                let items = crate::downloads::command(command).map_err(error_body)?;
                Ok(Response::Downloads { items })
            }
            Request::Uninstall { path } => {
                if InUse::contains(&path) {
                    return Err(error_body(xtux_games::Error::msg(format!(
                        "{} is running or being installed; stop it first",
                        path.display()
                    ))));
                }
                let settings = xtux_games::settings::Settings::load().map_err(error_body)?;
                xtux_games::Progress::message(&progress, &format!("Deleting {}", path.display()));
                let target = path.clone();
                tokio::task::spawn_blocking(move || {
                    xtux_games::uninstall::remove(&target, &settings.library_dirs)
                })
                .await
                .map_err(|e| error_body(xtux_games::Error::msg(e.to_string())))?
                .map_err(error_body)?;
                Ok(Response::Uninstalled { path })
            }
            Request::Catalog { market, refresh } => {
                let settings = xtux_games::settings::Settings::load().unwrap_or_default();
                let market = market.or(settings.market.clone());
                let catalog = xtux_games::catalog::load(
                    &self.client,
                    &self.tokens,
                    market.as_deref(),
                    refresh,
                )
                .await
                .map_err(error_body)?;
                // Installed games by Store ID.
                let installed: HashMap<String, PathBuf> =
                    xtux_games::library::scan(&settings.library_dirs)
                        .into_iter()
                        .filter_map(|g| Some((g.store_id?.to_uppercase(), g.path)))
                        .collect();
                let titles = catalog
                    .titles
                    .into_iter()
                    .map(|t| xtux_protocol::CatalogTitle {
                        installed: installed.get(&t.product_id.to_uppercase()).cloned(),
                        product_id: t.product_id,
                        name: t.name,
                        publisher: t.publisher,
                        description: t.description,
                        category: t.category,
                        content_id: t.content_id,
                        download_size: t.download_size,
                        poster_url: t.poster_url,
                        hero_url: t.hero_url,
                        in_game_pass: t.in_game_pass,
                        owned: t.owned,
                        installable: t.installable,
                        package_format: t.package_format,
                        // xTux's own title list is a guess: only when the user asked for it.
                        windows_only: t.windows_only.filter(|_| settings.ui.anticheat_guess),
                    })
                    .collect();
                Ok(Response::Catalog { titles })
            }
            Request::Library => {
                let settings = xtux_games::settings::Settings::load().map_err(error_body)?;
                let games = xtux_games::library::scan(&settings.library_dirs)
                    .into_iter()
                    .map(|g| xtux_protocol::GameInfo {
                        license_problem: xtux_games::license::problems::of(&g.path),
                        anticheat: g.anticheat.as_ref().map(|a| a.name.clone()),
                        anticheat_blocks: g.anticheat.as_ref().is_some_and(|a| a.blocks),
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
                args,
            } => {
                let _in_use = InUse::new(&source);
                // Wine's output goes to the game's log, which clients show.
                let log = xtux_games::logs::game_log(&source);
                if let Some(log) = &log {
                    xtux_games::Progress::message(
                        &progress,
                        &format!("Game log: {}", log.display()),
                    );
                }
                let game_dir = source.clone();
                let options = xtux_games::run::LaunchOptions {
                    source,
                    wine,
                    exe,
                    market,
                    env,
                    args,
                    log,
                };
                let game =
                    match xtux_games::run::launch(&self.client, &self.tokens, options, &progress)
                        .await
                    {
                        Ok(game) => {
                            // Getting this far means the license was granted.
                            xtux_games::license::problems::clear(&game_dir);
                            game
                        }
                        Err(err) => {
                            if let xtux_games::Error::NotEntitled(message) = &err {
                                xtux_games::license::problems::record(&game_dir, message);
                            }
                            return Err(error_body(err));
                        }
                    };
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
                let _in_use = InUse::new(&destination);
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
                    // Only what the sign-in window needs from the client's session.
                    .env_clear()
                    .envs(env.into_iter().filter(|(key, _)| sign_in_env(key)))
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
/// Variables of the client passed to the sign-in helper: display, session bus, locale, home and keychain access.
fn sign_in_env(key: &str) -> bool {
    matches!(
        key,
        "DISPLAY"
            | "WAYLAND_DISPLAY"
            | "XAUTHORITY"
            | "DBUS_SESSION_BUS_ADDRESS"
            | "HOME"
            | "USER"
            | "LANG"
            | "PATH"
    ) || key.starts_with("XDG_")
        || key.starts_with("LC_")
}

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
