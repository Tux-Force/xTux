//! xTux Launcher: a thin graphical client of `xtux-service`.
//!
//! The Slint event loop runs on the main thread; daemon requests run on a tokio runtime and hand their results back
//! with `slint::invoke_from_event_loop`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};
use xtux_games::settings::{GameSettings, Settings};
use xtux_protocol::{Client, ClientError, Event, GameInfo, Request, Response};

slint::include_modules!();

/// Games currently shown, in list order, so a Play click can find its folder.
type Shown = Arc<Mutex<Vec<GameInfo>>>;
/// The game open on the game page.
type Current = Arc<Mutex<Option<GameInfo>>>;

/// Shown part of a log file.
const LOG_TAIL: u64 = 256 * 1024;

fn path_text(path: &Option<PathBuf>) -> String {
    path.as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn non_empty(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// `NAME=value` lines; lines without `=` or with an empty name are skipped.
fn parse_env(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let (name, value) = line.trim().split_once('=')?;
            let name = name.trim();
            (!name.is_empty()).then(|| (name.to_string(), value.to_string()))
        })
        .collect()
}

fn show_game(ui: &AppWindow, game: &GameInfo) {
    let settings = Settings::load().unwrap_or_default();
    let own = settings.game(&game.path);
    ui.set_game_name(game.name.clone().into());
    ui.set_game_path(game.path.display().to_string().into());
    ui.set_game_wine(own.wine.unwrap_or_default().into());
    ui.set_game_prefix(path_text(&own.prefix).into());
    ui.set_game_exe(own.exe.unwrap_or_default().into());
    ui.set_game_args(own.args.join(" ").into());
    ui.set_game_env(
        own.env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("\n")
            .into(),
    );
    show_logs(ui, Some(&game.path));
}

fn show_logs(ui: &AppWindow, game: Option<&Path>) {
    if let Some(log) = game.and_then(xtux_games::logs::game_log) {
        let text = xtux_games::logs::tail(&log, LOG_TAIL);
        ui.set_game_log(
            if text.is_empty() {
                format!(
                    "No log yet; it is written to {} when the game starts.",
                    log.display()
                )
            } else {
                text
            }
            .into(),
        );
    }
    if let Some(log) = xtux_games::logs::service_log() {
        ui.set_service_log(xtux_games::logs::tail(&log, LOG_TAIL).into());
        ui.set_service_log_path(log.display().to_string().into());
    }
}

/// The launch request for a game: its own settings, then the general ones.
fn launch_request(game: &GameInfo) -> Result<Request, String> {
    let settings = Settings::load().map_err(|e| e.to_string())?;
    let game_settings = settings.effective(&game.path);
    let wine = game_settings
        .wine
        .ok_or("Set the Wine binary in Settings first")?;
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| k != "WINEPREFIX")
        .collect();
    if let Some(prefix) = &game_settings.prefix {
        env.push(("WINEPREFIX".into(), prefix.to_string_lossy().into_owned()));
    } else if let Ok(prefix) = std::env::var("WINEPREFIX") {
        env.push(("WINEPREFIX".into(), prefix));
    }
    env.extend(game_settings.env);
    Ok(Request::Launch {
        source: game.path.clone(),
        wine: expand_home(&wine).to_string_lossy().into_owned(),
        exe: game_settings.exe,
        market: settings.market,
        env,
        args: game_settings.args,
    })
}

fn start_game(
    ui: &AppWindow,
    weak: &Weak<AppWindow>,
    handle: &tokio::runtime::Handle,
    game: GameInfo,
) {
    let request = match launch_request(&game) {
        Ok(request) => request,
        Err(err) => {
            ui.set_status(err.into());
            ui.set_page(1);
            return;
        }
    };
    ui.set_busy(true);
    ui.set_status(format!("Starting {}…", game.name).into());
    let weak = weak.clone();
    handle.spawn(async move {
        let name = game.name.clone();
        let events = weak.clone();
        let result = ask(request, move |event| match event {
            Event::Message { text } => set_status(&events, format!("{name}: {text}"), None),
            Event::GameStarted { .. } => set_status(&events, format!("{name} is running"), None),
            _ => {}
        })
        .await;
        match result {
            Ok(Response::GameExited { code }) => set_status(
                &weak,
                format!("{} exited (code {})", game.name, code.unwrap_or(0)),
                Some(false),
            ),
            Ok(other) => set_status(&weak, format!("Unexpected answer: {other:?}"), Some(false)),
            Err(err) => set_status(&weak, err.to_string(), Some(false)),
        }
        let path = game.path.clone();
        with_ui(&weak, move |ui| show_logs(&ui, Some(&path)));
    });
}

fn expand_home(text: &str) -> PathBuf {
    match (text.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(text),
    }
}

/// Runs one request against the daemon (starting it when needed), forwarding events.
async fn ask(request: Request, mut on_event: impl FnMut(Event)) -> Result<Response, ClientError> {
    let mut daemon = Client::connect_or_spawn().await?;
    daemon.request(request, &mut on_event).await
}

/// Updates the window from any thread.
fn with_ui(ui: &Weak<AppWindow>, update: impl FnOnce(AppWindow) + Send + 'static) {
    let ui = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui.upgrade() {
            update(ui);
        }
    });
}

fn set_status(ui: &Weak<AppWindow>, text: impl Into<String>, busy: Option<bool>) {
    let text = text.into();
    with_ui(ui, move |ui| {
        ui.set_status(SharedString::from(text));
        if let Some(busy) = busy {
            ui.set_busy(busy);
        }
    });
}

async fn refresh_status(ui: Weak<AppWindow>) {
    match ask(Request::Status, |_| {}).await {
        Ok(Response::Status {
            signed_in, user, ..
        }) => {
            let user = user.unwrap_or_default();
            with_ui(&ui, move |ui| {
                ui.set_signed_in(signed_in);
                ui.set_user(SharedString::from(user));
            });
        }
        Ok(other) => set_status(&ui, format!("Unexpected answer: {other:?}"), None),
        Err(err) => set_status(&ui, err.to_string(), None),
    }
}

async fn refresh_library(ui: Weak<AppWindow>, shown: Shown) {
    match ask(Request::Library, |_| {}).await {
        Ok(Response::Library { games }) => {
            let rows: Vec<(String, String, bool)> = games
                .iter()
                .map(|g| {
                    let source = if g.kind == "windows" {
                        "Windows Xbox app install"
                    } else {
                        "Installed by xTux"
                    };
                    (
                        g.name.clone(),
                        format!("{source} · {}", g.path.display()),
                        g.kind == "windows",
                    )
                })
                .collect();
            let count = games.len();
            if let Ok(mut list) = shown.lock() {
                *list = games;
            }
            with_ui(&ui, move |ui| {
                let model: Vec<GameRow> = rows
                    .into_iter()
                    .map(|(name, detail, windows)| GameRow {
                        name: name.into(),
                        detail: detail.into(),
                        windows,
                    })
                    .collect();
                ui.set_games(ModelRc::new(VecModel::from(model)));
                ui.set_status(SharedString::from(format!("{count} games in the library")));
            });
        }
        Ok(other) => set_status(&ui, format!("Unexpected answer: {other:?}"), None),
        Err(err) => set_status(&ui, err.to_string(), None),
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("XODUS_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let runtime = tokio::runtime::Runtime::new().expect("cannot start the async runtime");
    let ui = AppWindow::new().expect("cannot create the window");
    let shown: Shown = Arc::default();
    let current: Current = Arc::default();

    let settings = Settings::load().unwrap_or_default();
    ui.set_wine(settings.wine.clone().unwrap_or_default().into());
    ui.set_prefix(
        settings
            .prefix
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into(),
    );
    ui.set_library_dirs(
        settings
            .library_dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n")
            .into(),
    );

    {
        let weak = ui.as_weak();
        let shown = shown.clone();
        runtime.spawn(async move {
            refresh_status(weak.clone()).await;
            refresh_library(weak, shown).await;
        });
    }

    ui.on_refresh({
        let weak = ui.as_weak();
        let shown = shown.clone();
        let handle = runtime.handle().clone();
        move || {
            let weak = weak.clone();
            let shown = shown.clone();
            handle.spawn(async move {
                refresh_status(weak.clone()).await;
                refresh_library(weak, shown).await;
            });
        }
    });

    ui.on_sign_in({
        let weak = ui.as_weak();
        let handle = runtime.handle().clone();
        move || {
            let weak = weak.clone();
            set_status(&weak, "Opening the Microsoft sign-in window…", Some(true));
            handle.spawn(async move {
                let result = ask(
                    Request::SignIn {
                        env: std::env::vars().collect(),
                    },
                    |_| {},
                )
                .await;
                match result {
                    Ok(_) => set_status(&weak, "Signed in", Some(false)),
                    Err(err) => set_status(&weak, err.to_string(), Some(false)),
                }
                refresh_status(weak).await;
            });
        }
    });

    ui.on_save_settings({
        let weak = ui.as_weak();
        let shown = shown.clone();
        let handle = runtime.handle().clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            // Keep what this page does not show (market, game settings).
            let mut settings = Settings::load().unwrap_or_default();
            settings.library_dirs = ui
                .get_library_dirs()
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(expand_home)
                .collect();
            settings.wine =
                non_empty(&ui.get_wine()).map(|w| expand_home(&w).to_string_lossy().into_owned());
            settings.prefix = non_empty(&ui.get_prefix()).map(|p| expand_home(&p));
            match settings.save() {
                Ok(()) => {
                    ui.set_status("Settings saved".into());
                    handle.spawn(refresh_library(weak.clone(), shown.clone()));
                }
                Err(err) => ui.set_status(format!("Could not save the settings: {err}").into()),
            }
        }
    });

    ui.on_open_game({
        let weak = ui.as_weak();
        let shown = shown.clone();
        let current = current.clone();
        move |index| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(game) = shown
                .lock()
                .ok()
                .and_then(|l| l.get(index as usize).cloned())
            else {
                return;
            };
            show_game(&ui, &game);
            if let Ok(mut current) = current.lock() {
                *current = Some(game);
            }
            ui.set_page(2);
        }
    });

    ui.on_save_game({
        let weak = ui.as_weak();
        let current = current.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(game) = current.lock().ok().and_then(|c| c.clone()) else {
                return;
            };
            let own = GameSettings {
                wine: non_empty(&ui.get_game_wine())
                    .map(|w| expand_home(&w).to_string_lossy().into_owned()),
                prefix: non_empty(&ui.get_game_prefix()).map(|p| expand_home(&p)),
                exe: non_empty(&ui.get_game_exe()),
                args: ui
                    .get_game_args()
                    .split_whitespace()
                    .map(str::to_string)
                    .collect(),
                env: parse_env(&ui.get_game_env()),
            };
            let mut settings = Settings::load().unwrap_or_default();
            settings.set_game(&game.path, own);
            match settings.save() {
                Ok(()) => ui.set_status(format!("Settings of {} saved", game.name).into()),
                Err(err) => ui.set_status(format!("Could not save the settings: {err}").into()),
            }
        }
    });

    ui.on_refresh_logs({
        let weak = ui.as_weak();
        let current = current.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let game = current
                .lock()
                .ok()
                .and_then(|c| c.as_ref().map(|g| g.path.clone()));
            show_logs(&ui, game.as_deref());
        }
    });

    ui.on_play({
        let weak = ui.as_weak();
        let shown = shown.clone();
        let handle = runtime.handle().clone();
        move |index| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(game) = shown
                .lock()
                .ok()
                .and_then(|l| l.get(index as usize).cloned())
            else {
                return;
            };
            start_game(&ui, &weak, &handle, game);
        }
    });

    ui.on_play_game({
        let weak = ui.as_weak();
        let current = current.clone();
        let handle = runtime.handle().clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(game) = current.lock().ok().and_then(|c| c.clone()) else {
                return;
            };
            start_game(&ui, &weak, &handle, game);
        }
    });

    ui.run().expect("the window failed");
}
