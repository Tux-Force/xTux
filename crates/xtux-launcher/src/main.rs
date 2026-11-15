//! xTux Launcher: a thin graphical client of `xtux-service`.
//!
//! The Slint event loop runs on the main thread; daemon requests and image work run on a tokio runtime and hand their
//! results back with `slint::invoke_from_event_loop`. Images travel between threads as pixel buffers, `slint::Image`
//! is only made on the UI thread.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::{
    ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel, Weak,
};
use xtux_games::art::{self, Artwork};
use xtux_games::settings::{GameSettings, Settings};
use xtux_protocol::{Client, ClientError, Event, GameInfo, Request, Response};

slint::include_modules!();

type Pixels = SharedPixelBuffer<Rgba8Pixel>;

/// Shown part of a log file.
const LOG_TAIL: u64 = 256 * 1024;

/// A game of the library with its decoded images.
#[derive(Clone)]
struct Entry {
    info: GameInfo,
    art: Artwork,
    cover: Option<Pixels>,
    logo: Option<Pixels>,
    backdrop: Option<Pixels>,
}

#[derive(Default)]
struct State {
    all: Vec<Entry>,
    /// Indices into `all` of the games shown, in order.
    shown: Vec<usize>,
    search: String,
    /// The game open on the game page.
    current: Option<usize>,
    /// Which backdrop layer is visible, and the game it shows.
    show_a: bool,
    backdrop_of: Option<usize>,
}

type Shared = Arc<Mutex<State>>;

fn expand_home(text: &str) -> PathBuf {
    match (text.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(text),
    }
}

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

/// Decodes an image file, scaled down to fit `max_w` × `max_h`.
fn decode(path: Option<&Path>, max_w: u32, max_h: u32) -> Option<Pixels> {
    let image = image::open(path?).ok()?;
    let image = if image.width() > max_w || image.height() > max_h {
        image.thumbnail(max_w, max_h)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
    ))
}

fn load_entry(info: GameInfo) -> Entry {
    let art = art::available(&info.path, info.store_id.as_deref());
    Entry {
        cover: decode(art.cover.as_deref(), 360, 540),
        logo: decode(art.logo.as_deref(), 192, 192),
        backdrop: decode(art.backdrop.as_deref(), 640, 360),
        art,
        info,
    }
}

fn to_image(pixels: &Option<Pixels>) -> (Image, bool) {
    match pixels {
        Some(p) => (Image::from_rgba8(p.clone()), true),
        None => (Image::default(), false),
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

/// Applies the search and pushes the shown games to the window (UI thread).
fn show_games(ui: &AppWindow, state: &Shared) {
    let Ok(mut state) = state.lock() else { return };
    let needle = state.search.to_lowercase();
    state.shown = state
        .all
        .iter()
        .enumerate()
        .filter(|(_, e)| needle.is_empty() || e.info.name.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect();
    let rows: Vec<GameRow> = state
        .shown
        .iter()
        .map(|&i| {
            let entry = &state.all[i];
            let (cover, has_cover) = to_image(&entry.cover);
            let (logo, has_logo) = to_image(&entry.logo);
            let windows = entry.info.kind == "windows";
            GameRow {
                name: entry.info.name.clone().into(),
                detail: format!(
                    "{} · {}",
                    if windows {
                        "Windows Xbox app install"
                    } else {
                        "Installed by xTux"
                    },
                    entry.info.path.display()
                )
                .into(),
                windows,
                cover,
                has_cover,
                logo,
                has_logo,
            }
        })
        .collect();
    ui.set_game_count(state.all.len() as i32);
    ui.set_games(ModelRc::new(VecModel::from(rows)));
}

/// Cross-fades the window backdrop to the blurred art of `all[index]` (UI thread).
fn show_backdrop(ui: &AppWindow, state: &Shared, index: usize) {
    let Ok(mut state) = state.lock() else { return };
    if state.backdrop_of == Some(index) {
        return;
    }
    let Some(pixels) = state.all.get(index).and_then(|e| e.backdrop.clone()) else {
        return;
    };
    let image = Image::from_rgba8(pixels);
    // Fill the hidden layer, then make it the visible one.
    if state.show_a {
        ui.set_backdrop_b(image);
    } else {
        ui.set_backdrop_a(image);
    }
    state.show_a = !state.show_a;
    state.backdrop_of = Some(index);
    ui.set_show_a(state.show_a);
    ui.set_has_backdrop(true);
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

/// Loads the library with the art at hand, then fetches missing catalog art in the background.
async fn refresh_library(ui: Weak<AppWindow>, state: Shared) {
    let games = match ask(Request::Library, |_| {}).await {
        Ok(Response::Library { games }) => games,
        Ok(other) => return set_status(&ui, format!("Unexpected answer: {other:?}"), None),
        Err(err) => return set_status(&ui, err.to_string(), None),
    };
    let count = games.len();
    let entries =
        tokio::task::spawn_blocking(move || games.into_iter().map(load_entry).collect::<Vec<_>>())
            .await
            .unwrap_or_default();
    let missing: Vec<String> = {
        let mut ids: Vec<String> = entries
            .iter()
            .filter(|e| e.art.cover.is_none() || e.art.backdrop.is_none() || e.art.hero.is_none())
            .filter_map(|e| e.info.store_id.clone())
            .filter(|id| art::is_store_id(id))
            .collect();
        ids.sort();
        ids.dedup();
        ids
    };
    if let Ok(mut s) = state.lock() {
        s.all = entries;
        s.current = None;
        s.backdrop_of = None;
    }
    {
        let state = state.clone();
        with_ui(&ui, move |ui| {
            show_games(&ui, &state);
            ui.set_status(format!("{count} games in the library").into());
            // An ambient backdrop from the first game with art.
            let first = state
                .lock()
                .ok()
                .and_then(|s| s.all.iter().position(|e| e.backdrop.is_some()));
            if let Some(first) = first {
                show_backdrop(&ui, &state, first);
            }
        });
    }
    if missing.is_empty() {
        return;
    }

    // Catalog art: public Store data, only the Store ID and market are sent.
    let market = Settings::load().ok().and_then(|s| s.market);
    let client = reqwest::Client::builder()
        .user_agent(format!("xtux-launcher/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default();
    let mut fetched = false;
    for id in missing {
        match art::fetch(&client, &id, market.as_deref()).await {
            Ok(_) => fetched = true,
            Err(err) => tracing::warn!("no catalog art for {id}: {err}"),
        }
    }
    if !fetched {
        return;
    }
    let infos: Vec<GameInfo> = state
        .lock()
        .map(|s| s.all.iter().map(|e| e.info.clone()).collect())
        .unwrap_or_default();
    let entries =
        tokio::task::spawn_blocking(move || infos.into_iter().map(load_entry).collect::<Vec<_>>())
            .await
            .unwrap_or_default();
    if let Ok(mut s) = state.lock() {
        if s.all.len() == entries.len() {
            s.all = entries;
            s.backdrop_of = None;
        }
    }
    with_ui(&ui, move |ui| {
        show_games(&ui, &state);
        let first = state
            .lock()
            .ok()
            .and_then(|s| s.all.iter().position(|e| e.backdrop.is_some()));
        if let Some(first) = first {
            show_backdrop(&ui, &state, first);
        }
    });
}

fn show_logs(ui: &AppWindow, game: Option<&Path>) {
    if let Some(log) = game.and_then(xtux_games::logs::game_log) {
        let text = xtux_games::logs::tail(&log, LOG_TAIL);
        ui.set_game_log(
            if text.is_empty() {
                format!(
                    "No log yet. It is written when the game starts:\n{}",
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

/// Opens the game page of `all[index]` (UI thread); the hero image follows from a worker.
fn open_game(ui: &AppWindow, state: &Shared, handle: &tokio::runtime::Handle, index: usize) {
    let Some(entry) = state.lock().ok().and_then(|s| s.all.get(index).cloned()) else {
        return;
    };
    if let Ok(mut s) = state.lock() {
        s.current = Some(index);
    }
    let game = &entry.info;
    let own = Settings::load().unwrap_or_default().game(&game.path);
    ui.set_game_name(game.name.clone().into());
    ui.set_game_path(game.path.display().to_string().into());
    ui.set_game_kind(
        if game.kind == "windows" {
            "Windows Xbox app install"
        } else {
            "Installed by xTux"
        }
        .into(),
    );
    ui.set_game_wine(own.wine.unwrap_or_default().into());
    ui.set_game_prefix(path_text(&own.prefix).into());
    ui.set_game_exe(own.exe.unwrap_or_default().into());
    ui.set_game_args(own.args.join("\n").into());
    ui.set_game_env(
        own.env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("\n")
            .into(),
    );
    let (logo, has_logo) = to_image(&entry.logo);
    ui.set_game_logo(logo);
    ui.set_has_game_logo(has_logo);
    ui.set_has_game_hero(false);
    show_logs(ui, Some(&game.path));
    show_backdrop(ui, state, index);
    ui.set_page(2);

    let weak = ui.as_weak();
    let state = state.clone();
    let hero = entry.art.hero.clone();
    handle.spawn(async move {
        let pixels = tokio::task::spawn_blocking(move || decode(hero.as_deref(), 1920, 1080))
            .await
            .ok()
            .flatten();
        with_ui(&weak, move |ui| {
            // Still the same game?
            if state.lock().ok().and_then(|s| s.current) == Some(index) {
                let (hero, has_hero) = to_image(&pixels);
                ui.set_game_hero(hero);
                ui.set_has_game_hero(has_hero);
            }
        });
    });
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

fn start_game(ui: &AppWindow, handle: &tokio::runtime::Handle, game: GameInfo) {
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
    let weak = ui.as_weak();
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

/// Settings for saving; `None` (with a status message) when the file has an error, so it is never overwritten.
fn settings_for_saving(ui: &AppWindow) -> Option<Settings> {
    match Settings::load() {
        Ok(settings) => Some(settings),
        Err(err) => {
            ui.set_status(format!("Not saved, the settings file has an error: {err}").into());
            None
        }
    }
}

/// `XTUX_SNAPSHOT=<file.png>` renders one page (`XTUX_SNAPSHOT_PAGE`: library, list, game, settings, logs) to a
/// PNG and quits; used to check the look without a person at the screen.
fn schedule_snapshot(ui: &AppWindow, state: &Shared, handle: &tokio::runtime::Handle) {
    let Some(file) = std::env::var_os("XTUX_SNAPSHOT").map(PathBuf::from) else {
        return;
    };
    let page = std::env::var("XTUX_SNAPSHOT_PAGE").unwrap_or_else(|_| "library".into());
    let weak = ui.as_weak();
    let state = state.clone();
    let handle = handle.clone();
    slint::Timer::single_shot(Duration::from_secs(5), move || {
        let Some(ui) = weak.upgrade() else { return };
        match page.as_str() {
            "list" => ui.set_view("list".into()),
            "game" => open_game(&ui, &state, &handle, 0),
            "settings" => ui.set_page(1),
            "logs" => {
                ui.set_page(3);
                show_logs(&ui, None);
            }
            _ => ui.set_view("grid".into()),
        }
        let weak = weak.clone();
        let file = file.clone();
        slint::Timer::single_shot(Duration::from_secs(2), move || {
            if let Some(ui) = weak.upgrade() {
                match ui.window().take_snapshot() {
                    Ok(shot) => {
                        let saved = image::RgbaImage::from_raw(
                            shot.width(),
                            shot.height(),
                            shot.as_bytes().to_vec(),
                        )
                        .map(|img| img.save(&file));
                        if !matches!(saved, Some(Ok(()))) {
                            eprintln!("could not save the snapshot to {}", file.display());
                        }
                    }
                    Err(err) => eprintln!("snapshot failed: {err}"),
                }
            }
            let _ = slint::quit_event_loop();
        });
    });
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
    let state: Shared = Arc::default();
    let handle = runtime.handle().clone();

    let settings = Settings::load().unwrap_or_default();
    {
        let theme = ui.global::<Theme>();
        theme.set_choice(settings.ui.theme.clone().into());
        theme.set_dark_variant(settings.ui.dark_variant.clone().into());
        theme.set_accent_name(settings.ui.accent.clone().into());
        ui.set_view(settings.ui.view.clone().into());
        ui.invoke_apply_theme();
    }
    ui.set_wine(settings.wine.clone().unwrap_or_default().into());
    ui.set_prefix(path_text(&settings.prefix).into());
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
        let state = state.clone();
        runtime.spawn(async move {
            refresh_status(weak.clone()).await;
            refresh_library(weak, state).await;
        });
    }

    ui.on_refresh({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move || {
            let weak = weak.clone();
            let state = state.clone();
            handle.spawn(async move {
                refresh_status(weak.clone()).await;
                refresh_library(weak, state).await;
            });
        }
    });

    ui.on_sign_in({
        let weak = ui.as_weak();
        let handle = handle.clone();
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

    ui.on_search_edited({
        let weak = ui.as_weak();
        let state = state.clone();
        move |text| {
            let Some(ui) = weak.upgrade() else { return };
            if let Ok(mut s) = state.lock() {
                s.search = text.to_string();
            }
            show_games(&ui, &state);
        }
    });

    ui.on_hover_game({
        let weak = ui.as_weak();
        let state = state.clone();
        move |row| {
            let Some(ui) = weak.upgrade() else { return };
            let index = state
                .lock()
                .ok()
                .and_then(|s| s.shown.get(row as usize).copied());
            if let Some(index) = index {
                show_backdrop(&ui, &state, index);
            }
        }
    });

    ui.on_ui_changed({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(mut settings) = settings_for_saving(&ui) else {
                return;
            };
            let theme = ui.global::<Theme>();
            settings.ui.theme = theme.get_choice().into();
            settings.ui.dark_variant = theme.get_dark_variant().into();
            settings.ui.accent = theme.get_accent_name().into();
            settings.ui.view = ui.get_view().into();
            if let Err(err) = settings.save() {
                ui.set_status(format!("Could not save the settings: {err}").into());
            }
        }
    });

    ui.on_save_settings({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            // Keep what this page does not show (market, look, game settings).
            let Some(mut settings) = settings_for_saving(&ui) else {
                return;
            };
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
                    handle.spawn(refresh_library(weak.clone(), state.clone()));
                }
                Err(err) => ui.set_status(format!("Could not save the settings: {err}").into()),
            }
        }
    });

    ui.on_open_game({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move |row| {
            let Some(ui) = weak.upgrade() else { return };
            let index = state
                .lock()
                .ok()
                .and_then(|s| s.shown.get(row as usize).copied());
            if let Some(index) = index {
                open_game(&ui, &state, &handle, index);
            }
        }
    });

    ui.on_save_game({
        let weak = ui.as_weak();
        let state = state.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(game) = state
                .lock()
                .ok()
                .and_then(|s| s.current.and_then(|i| s.all.get(i).map(|e| e.info.clone())))
            else {
                return;
            };
            let own = GameSettings {
                wine: non_empty(&ui.get_game_wine())
                    .map(|w| expand_home(&w).to_string_lossy().into_owned()),
                prefix: non_empty(&ui.get_game_prefix()).map(|p| expand_home(&p)),
                exe: non_empty(&ui.get_game_exe()),
                args: ui
                    .get_game_args()
                    .lines()
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .map(str::to_string)
                    .collect(),
                env: parse_env(&ui.get_game_env()),
            };
            let Some(mut settings) = settings_for_saving(&ui) else {
                return;
            };
            settings.set_game(&game.path, own);
            match settings.save() {
                Ok(()) => ui.set_status(format!("Settings of {} saved", game.name).into()),
                Err(err) => ui.set_status(format!("Could not save the settings: {err}").into()),
            }
        }
    });

    ui.on_refresh_logs({
        let weak = ui.as_weak();
        let state = state.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let game = state.lock().ok().and_then(|s| {
                s.current
                    .and_then(|i| s.all.get(i).map(|e| e.info.path.clone()))
            });
            show_logs(&ui, game.as_deref());
        }
    });

    ui.on_play({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move |row| {
            let Some(ui) = weak.upgrade() else { return };
            let game = state.lock().ok().and_then(|s| {
                s.shown
                    .get(row as usize)
                    .and_then(|&i| s.all.get(i).map(|e| e.info.clone()))
            });
            if let Some(game) = game {
                start_game(&ui, &handle, game);
            }
        }
    });

    ui.on_play_game({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let game = state
                .lock()
                .ok()
                .and_then(|s| s.current.and_then(|i| s.all.get(i).map(|e| e.info.clone())));
            if let Some(game) = game {
                start_game(&ui, &handle, game);
            }
        }
    });

    schedule_snapshot(&ui, &state, &handle);
    ui.run().expect("the window failed");
}
