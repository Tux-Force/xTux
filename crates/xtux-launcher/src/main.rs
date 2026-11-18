//! xTux Launcher: a thin graphical client of `xtux-service`.
//!
//! The Slint event loop runs on the main thread; daemon requests and image work run on a tokio runtime and hand their
//! results back with `slint::invoke_from_event_loop`. Images travel between threads as pixel buffers, `slint::Image`
//! is only made on the UI thread.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::{
    ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel, Weak,
};
use xtux_games::art::{self, Artwork};
use xtux_games::settings::{ControllerMode, GameSettings, Settings};
use xtux_protocol::{
    Canceller, Client, ClientError, DownloadCommand, ErrorKind, Event, GameInfo, Request, Response,
};

mod theme;

slint::include_modules!();

type Pixels = SharedPixelBuffer<Rgba8Pixel>;

thread_local! {
    // The window's models live as long as it: rows are updated in place, so a cover that arrives fades in
    // instead of the whole grid being rebuilt.
    static GAMES: std::rc::Rc<VecModel<GameRow>> = std::rc::Rc::new(VecModel::default());
    static STORE: std::rc::Rc<VecModel<StoreRow>> = std::rc::Rc::new(VecModel::default());
    static DOWNLOAD_ROWS: std::rc::Rc<VecModel<DownloadRow>> = std::rc::Rc::new(VecModel::default());
    static SHOTS: std::rc::Rc<VecModel<ShotRow>> = std::rc::Rc::new(VecModel::default());
    static ADDONS: std::rc::Rc<VecModel<AddonRow>> = std::rc::Rc::new(VecModel::default());
    static UPDATES: std::rc::Rc<VecModel<UpdateRow>> = std::rc::Rc::new(VecModel::default());
    static GRAPH: std::rc::Rc<VecModel<GraphPoint>> = std::rc::Rc::new(VecModel::default());
    /// Counts gallery loads, so screenshots of a title left meanwhile are dropped.
    static SHOT_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Replaces the rows of `model`, changing only rows that differ when the length stays (UI thread).
fn update_rows<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    use slint::Model;
    if model.row_count() == rows.len() {
        for (i, row) in rows.into_iter().enumerate() {
            if model.row_data(i).as_ref() != Some(&row) {
                model.set_row_data(i, row);
            }
        }
    } else {
        model.set_vec(rows);
    }
}

/// Shown part of a log file.
const LOG_TAIL: u64 = 256 * 1024;

/// A game of the library with its decoded images.
#[derive(Clone)]
struct Entry {
    info: GameInfo,
    art: Artwork,
    cover: Option<Pixels>,
    /// Strongly blurred cover for the frosted action bar.
    cover_blur: Option<Pixels>,
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
    /// `dynamic`, `static` or `off` (Settings).
    backdrop_mode: String,
    /// Running games by folder, with what stops them.
    running: HashMap<PathBuf, Canceller>,
    /// The store catalog and which of its titles are shown.
    catalog: Vec<xtux_protocol::CatalogTitle>,
    /// Owned add-ons with a PC package; shown on the page of their game.
    addons: Vec<xtux_protocol::CatalogTitle>,
    pending_addon_delete: Option<xtux_protocol::CatalogTitle>,
    updates: Vec<xtux_protocol::UpdateInfo>,
    store_filtered: Vec<usize>,
    store_limit: usize,
    store_current: Option<usize>,
    /// Decoded store covers by Store ID.
    covers: HashMap<String, (Pixels, Pixels)>,
    /// The download queue as xtux-service last reported it, and the library folders offered for an install.
    downloads: Vec<xtux_protocol::DownloadItem>,
    /// Network and disk rates, oldest first.
    history: Vec<xtux_protocol::RateSample>,
    download_polling: bool,
    install_libraries: Vec<PathBuf>,
    /// What the command line asked for, done once the library is loaded.
    startup: Option<Startup>,
    handle: Option<tokio::runtime::Handle>,
}

/// Start options, for desktop shortcuts or Steam entries.
#[derive(clap::Parser)]
#[command(
    name = "xtux-launcher",
    version,
    about = "xTux Launcher: library, sign-in and game start of Tux-Force"
)]
struct Args {
    /// Start this game right away: its name as shown in the library, its folder name or its folder path
    #[arg(long, value_name = "GAME")]
    play: Option<String>,
    /// Open the page of this game
    #[arg(long, value_name = "GAME", conflicts_with = "play")]
    game: Option<String>,
    /// Open a page
    #[arg(long, value_parser = ["library", "store", "updates", "settings", "about", "logs"], conflicts_with_all = ["play", "game"])]
    page: Option<String>,
}

#[derive(Clone)]
enum Startup {
    Play(String),
    Game(String),
    Page(String),
}

/// The library index of `query`: a folder path, a display name or a folder name (case-insensitive).
fn find_game(all: &[Entry], query: &str) -> Result<usize, String> {
    let as_path = std::fs::canonicalize(query).ok();
    let folder = |e: &Entry| {
        e.info
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    let matching = |pick: &dyn Fn(&Entry) -> bool| -> Vec<usize> {
        all.iter()
            .enumerate()
            .filter(|(_, e)| pick(e))
            .map(|(i, _)| i)
            .collect()
    };
    let mut found = Vec::new();
    if let Some(path) = &as_path {
        found = matching(&|e| std::fs::canonicalize(&e.info.path).ok().as_ref() == Some(path));
    }
    if found.is_empty() {
        found = matching(&|e| e.info.name.eq_ignore_ascii_case(query));
    }
    if found.is_empty() {
        found = matching(&|e| folder(e) == query.to_lowercase());
    }
    match found.as_slice() {
        [index] => Ok(*index),
        [] => Err(format!("{query:?} is not in the library")),
        _ => Err(format!(
            "{query:?} is installed more than once; pass its folder path"
        )),
    }
}

/// Carries out the start option once the library is there (UI thread).
fn run_startup(ui: &AppWindow, state: &Shared) {
    let (startup, handle) = match state.lock() {
        Ok(mut s) => (s.startup.take(), s.handle.clone()),
        Err(_) => return,
    };
    let (Some(startup), Some(handle)) = (startup, handle) else {
        return;
    };
    let all = state.lock().map(|s| s.all.clone()).unwrap_or_default();
    match startup {
        Startup::Page(page) => ui.set_page(match page.as_str() {
            "settings" => 1,
            "updates" => 8,
            "store" => {
                open_store(ui, state, &handle, false);
                5
            }
            "logs" => 3,
            "about" => 4,
            _ => 0,
        }),
        Startup::Game(query) => match find_game(&all, &query) {
            Ok(index) => open_game(ui, state, &handle, index),
            Err(err) => ui.set_status(err.into()),
        },
        Startup::Play(query) => match find_game(&all, &query) {
            Ok(index) => {
                open_game(ui, state, &handle, index);
                if all[index].info.anticheat_blocks {
                    ui.set_status(
                        format!(
                            "{} runs on Windows only (kernel anti-cheat)",
                            all[index].info.name
                        )
                        .into(),
                    );
                } else {
                    start_game(ui, state, &handle, all[index].info.clone());
                }
            }
            Err(err) => ui.set_status(err.into()),
        },
    }
}

/// What asks for a backdrop change.
#[derive(PartialEq)]
enum BackdropCause {
    /// The library loaded: the ambient backdrop.
    Ambient,
    /// The mouse is over a game, or its page opened.
    Game,
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
    let image = art::open_image(path?).ok()?;
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

/// A small, strongly blurred copy for frosted glass.
fn blur(pixels: &Option<Pixels>) -> Option<Pixels> {
    let pixels = pixels.as_ref()?;
    let image =
        image::RgbaImage::from_raw(pixels.width(), pixels.height(), pixels.as_bytes().to_vec())?;
    let small = image::imageops::thumbnail(&image, 120, 180);
    let blurred = image::imageops::fast_blur(&small, 10.0);
    Some(SharedPixelBuffer::clone_from_slice(
        blurred.as_raw(),
        blurred.width(),
        blurred.height(),
    ))
}

fn load_entry(info: GameInfo) -> Entry {
    let art = art::available(&info.path, info.store_id.as_deref());
    let cover = decode(art.cover.as_deref(), 360, 540);
    Entry {
        cover_blur: blur(&cover),
        cover,
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
            let (cover_blur, _) = to_image(&entry.cover_blur);
            let (logo, has_logo) = to_image(&entry.logo);
            let windows = entry.info.kind == "windows";
            let running = state.running.contains_key(&entry.info.path);
            let license_problem = entry.info.license_problem.clone().unwrap_or_default();
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
                running,
                license_problem: license_problem.into(),
                anticheat: entry.info.anticheat.clone().unwrap_or_default().into(),
                anticheat_blocks: entry.info.anticheat_blocks,
                cover,
                cover_blur,
                has_cover,
                logo,
                has_logo,
            }
        })
        .collect();
    ui.set_game_count(state.all.len() as i32);
    GAMES.with(|model| update_rows(model, rows));
}

/// Cross-fades the window backdrop to the blurred art of `all[index]` (UI thread), as the backdrop setting allows.
fn show_backdrop(ui: &AppWindow, state: &Shared, index: usize, cause: BackdropCause) {
    let Ok(mut state) = state.lock() else { return };
    match state.backdrop_mode.as_str() {
        "off" => {
            ui.set_has_backdrop(false);
            return;
        }
        "static" if cause == BackdropCause::Game && state.backdrop_of.is_some() => return,
        _ => {}
    }
    if state.backdrop_of == Some(index) {
        ui.set_has_backdrop(true);
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
            run_startup(&ui, &state);
            // An ambient backdrop from the first game with art.
            let first = state
                .lock()
                .ok()
                .and_then(|s| s.all.iter().position(|e| e.backdrop.is_some()));
            if let Some(first) = first {
                show_backdrop(&ui, &state, first, BackdropCause::Ambient);
            }
        });
    }
    // Opt-out in Settings: the catalog learns the Store IDs of installed games.
    let online = Settings::load().map(|s| s.ui.online_art).unwrap_or(true);
    if missing.is_empty() || !online {
        return;
    }

    // Catalog art: public Store data, only the Store ID and market are sent.
    let market = Settings::load().ok().and_then(|s| s.market);
    let client = art::client(&format!("xtux-launcher/{}", env!("CARGO_PKG_VERSION")));
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
            show_backdrop(&ui, &state, first, BackdropCause::Ambient);
        }
    });
}

/// The end of a log, newest line first.
fn newest_first(path: &Path) -> String {
    let text = xtux_games::logs::tail(path, LOG_TAIL);
    text.lines().rev().collect::<Vec<_>>().join("\n")
}

fn show_logs(ui: &AppWindow, game: Option<&Path>) {
    if let Some(log) = game.and_then(xtux_games::logs::game_log) {
        let text = newest_first(&log);
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
        ui.set_service_log(newest_first(&log).into());
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
    ui.set_game_controller(own.controller.map_or("", ControllerMode::as_str).into());
    ui.set_game_args(own.args.join("\n").into());
    ui.set_game_env(
        own.env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("\n")
            .into(),
    );
    ui.set_game_windows(game.kind == "windows");
    ui.set_game_license_problem(game.license_problem.clone().unwrap_or_default().into());
    ui.set_game_anticheat(game.anticheat.clone().unwrap_or_default().into());
    ui.set_game_anticheat_blocks(game.anticheat_blocks);
    ui.set_game_running(
        state
            .lock()
            .is_ok_and(|s| s.running.contains_key(&game.path)),
    );
    let (logo, has_logo) = to_image(&entry.logo);
    ui.set_game_logo(logo);
    ui.set_has_game_logo(has_logo);
    ui.set_has_game_hero(false);
    // The blurred backdrop is the hero's soft first look.
    ui.set_game_hero_soft(to_image(&entry.backdrop).0);
    show_logs(ui, Some(&game.path));
    show_backdrop(ui, state, index, BackdropCause::Game);
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

/// Shows the running state of the games (UI thread).
fn show_running(ui: &AppWindow, state: &Shared) {
    show_games(ui, state);
    let running = state.lock().ok().and_then(|s| {
        let current = s.all.get(s.current?)?;
        Some(s.running.contains_key(&current.info.path))
    });
    ui.set_game_running(running.unwrap_or(false));
    let problem = state.lock().ok().and_then(|s| {
        let current = s.all.get(s.current?)?;
        current.info.license_problem.clone()
    });
    ui.set_game_license_problem(problem.unwrap_or_default().into());
}

fn start_game(ui: &AppWindow, state: &Shared, handle: &tokio::runtime::Handle, game: GameInfo) {
    if state
        .lock()
        .is_ok_and(|s| s.running.contains_key(&game.path))
    {
        return;
    }
    let request = match launch_request(&game) {
        Ok(request) => request,
        Err(err) => {
            ui.set_status(err.into());
            ui.set_page(1);
            return;
        }
    };
    ui.set_status(format!("Starting {}…", game.name).into());
    let weak = ui.as_weak();
    let state = state.clone();
    handle.spawn(async move {
        let mut daemon = match Client::connect_or_spawn().await {
            Ok(daemon) => daemon,
            Err(err) => return set_status(&weak, err.to_string(), None),
        };
        // Running from now on, so Stop works already while the game starts.
        if let Ok(mut s) = state.lock() {
            s.running.insert(game.path.clone(), daemon.canceller());
        }
        {
            let state = state.clone();
            with_ui(&weak, move |ui| show_running(&ui, &state));
        }
        let name = game.name.clone();
        let events = weak.clone();
        let started = state.clone();
        let started_path = game.path.clone();
        let result = daemon
            .request(request, move |event| match event {
                Event::Message { text } => set_status(&events, format!("{name}: {text}"), None),
                Event::GameStarted { .. } => {
                    // The license was granted.
                    set_license_problem(&started, &started_path, None);
                    set_status(&events, format!("{name} is running"), None)
                }
                _ => {}
            })
            .await;
        match result {
            Ok(Response::GameExited { code }) => set_status(
                &weak,
                format!("{} exited (code {})", game.name, code.unwrap_or(0)),
                None,
            ),
            Ok(Response::Cancelled) => set_status(&weak, format!("{} stopped", game.name), None),
            Ok(other) => set_status(&weak, format!("Unexpected answer: {other:?}"), None),
            Err(ClientError::Remote(body)) if body.kind == ErrorKind::NotEntitled => {
                set_license_problem(&state, &game.path, Some(body.message.clone()));
                set_status(
                    &weak,
                    format!("{}: license check failed: {}", game.name, body.message),
                    None,
                )
            }
            Err(err) => set_status(&weak, err.to_string(), None),
        }
        if let Ok(mut s) = state.lock() {
            s.running.remove(&game.path);
        }
        let path = game.path.clone();
        with_ui(&weak, move |ui| {
            show_running(&ui, &state);
            show_logs(&ui, Some(&path));
        });
    });
}

/// Updates the license mark of a game in the library (the daemon remembers it too).
fn set_license_problem(state: &Shared, path: &Path, problem: Option<String>) {
    if let Ok(mut s) = state.lock() {
        if let Some(entry) = s.all.iter_mut().find(|e| e.info.path == path) {
            entry.info.license_problem = problem;
        }
    }
}

/// Titles shown at first, and added per "Show more".
const STORE_PAGE: usize = 120;

fn size_text(bytes: Option<u64>) -> String {
    match bytes {
        Some(b) if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        Some(b) if b > 0 => format!("{} MB", b >> 20),
        _ => String::new(),
    }
}

/// What the label of a store title means, in one or two short sentences; empty for titles without a label
/// (installable or installed ones need no explanation).
fn store_hint(title: &xtux_protocol::CatalogTitle) -> String {
    if title.installed.is_some() || (title.installable && title.windows_only.is_none()) {
        return String::new();
    }
    let size = size_text(title.download_size);
    let mut hint = if title.installed.is_some() {
        "Installed: ready to play.".to_string()
    } else if let Some(anticheat) = &title.windows_only {
        format!(
            "Likely Windows only: xTux's own list guesses the kernel anti-cheat {anticheat} (not verified)."
        )
    } else if title.installable {
        format!(
            "GDK game: xTux can install it{}.",
            if size.is_empty() {
                String::new()
            } else {
                format!(" ({size})")
            }
        )
    } else if title.package_format.is_none() {
        "No PC package in the Store: usually installs through another launcher, or is offered for consoles only."
            .to_string()
    } else {
        format!(
            "Not supported: an older UWP app package ({}), not a GDK game.",
            title.package_format.as_deref().unwrap_or_default()
        )
    };
    let access = match (title.owned, title.in_game_pass) {
        (true, true) => " Owned and in Game Pass.",
        (true, false) => " Owned by your account.",
        (false, true) => " Included in Game Pass.",
        _ => "",
    };
    hint.push_str(access);
    hint
}

/// State and progress of a title in the download queue; empty when it is not queued (or finished).
fn download_of(downloads: &[xtux_protocol::DownloadItem], product_id: &str) -> (String, f32) {
    downloads
        .iter()
        .rev()
        .find(|d| d.product_id == product_id && d.state != "done" && d.state != "removing")
        .map(|d| {
            let progress = match d.total {
                Some(total) if total > 0 => d.done as f32 / total as f32,
                _ => 0.0,
            };
            (d.state.clone(), progress)
        })
        .unwrap_or_default()
}

fn store_row(
    title: &xtux_protocol::CatalogTitle,
    cover: Option<&(Pixels, Pixels)>,
    downloads: &[xtux_protocol::DownloadItem],
) -> StoreRow {
    let (dl_state, dl_progress) = download_of(downloads, &title.product_id);
    let (cover_blur, _) = to_image(&cover.map(|c| c.1.clone()));
    let (cover, has_cover) = to_image(&cover.map(|c| c.0.clone()));
    StoreRow {
        name: title.name.clone().into(),
        publisher: title.publisher.clone().into(),
        category: title.category.clone().into(),
        description: title.description.clone().into(),
        size: size_text(title.download_size).into(),
        cover,
        has_cover,
        in_game_pass: title.in_game_pass,
        owned: title.owned,
        installable: title.installable,
        installed: title.installed.is_some(),
        cover_blur,
        no_package: title.package_format.is_none(),
        package_format: title.package_format.clone().unwrap_or_default().into(),
        windows_only: title.windows_only.clone().unwrap_or_default().into(),
        hint: store_hint(title).into(),
        dl_state: dl_state.into(),
        dl_progress,
    }
}

/// Applies the store filters of the window (UI thread).
fn filter_store(ui: &AppWindow, state: &Shared) {
    let search = ui.get_store_search().to_lowercase();
    let source = ui.get_store_source().to_string();
    let installable = ui.get_store_installable();
    let hide_windows_only = ui.get_store_hide_windows_only();
    if let Ok(mut s) = state.lock() {
        s.store_filtered = s
            .catalog
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                (search.is_empty()
                    || t.name.to_lowercase().contains(&search)
                    || t.publisher.to_lowercase().contains(&search)
                    || t.category.to_lowercase().contains(&search))
                    && match source.as_str() {
                        "gamepass" => t.in_game_pass,
                        "owned" => t.owned,
                        "installed" => t.installed.is_some(),
                        _ => true,
                    }
                    && (!installable || t.installable)
                    && (!hide_windows_only || t.windows_only.is_none())
            })
            .map(|(i, _)| i)
            .collect();
        s.store_limit = STORE_PAGE;
    }
    // New results start at the top.
    ui.set_store_scroll(0.0);
    show_store(ui, state);
}

/// Pushes the shown store titles to the window (UI thread).
fn show_store(ui: &AppWindow, state: &Shared) {
    let Ok(s) = state.lock() else { return };
    let rows: Vec<StoreRow> = s
        .store_filtered
        .iter()
        .take(s.store_limit)
        .filter_map(|&i| s.catalog.get(i))
        .map(|t| store_row(t, s.covers.get(&t.product_id), &s.downloads))
        .collect();
    ui.set_store_count(s.store_filtered.len() as i32);
    ui.set_store_total(s.catalog.len() as i32);
    STORE.with(|model| update_rows(model, rows));
}

/// Loads the covers of the shown store titles: from the art cache, else from the Store (when allowed).
fn load_store_covers(ui: &AppWindow, state: &Shared, handle: &tokio::runtime::Handle) {
    let wanted: Vec<(String, Option<String>)> = match state.lock() {
        Ok(s) => s
            .store_filtered
            .iter()
            .take(s.store_limit)
            .filter_map(|&i| s.catalog.get(i))
            .filter(|t| !s.covers.contains_key(&t.product_id))
            .map(|t| (t.product_id.clone(), t.poster_url.clone()))
            .collect(),
        Err(_) => return,
    };
    if wanted.is_empty() {
        return;
    }
    let online = Settings::load().map(|s| s.ui.online_art).unwrap_or(true);
    let weak = ui.as_weak();
    let state = state.clone();
    handle.spawn(async move {
        let client = art::client(&format!("xtux-launcher/{}", env!("CARGO_PKG_VERSION")));
        let limit = Arc::new(tokio::sync::Semaphore::new(8));
        let mut tasks = tokio::task::JoinSet::new();
        for (id, url) in wanted {
            let client = client.clone();
            let limit = limit.clone();
            tasks.spawn(async move {
                let _permit = limit.acquire_owned().await.ok()?;
                let mut cover = art::cached(&id).cover;
                if cover.is_none() && online {
                    cover = art::fetch_from(&client, &id, url.as_deref(), None)
                        .await
                        .ok()?
                        .cover;
                }
                let pixels = tokio::task::spawn_blocking(move || {
                    let sharp = decode(cover.as_deref(), 320, 480)?;
                    let soft = blur(&Some(sharp.clone()))?;
                    Some((sharp, soft))
                })
                .await
                .ok()??;
                Some((id, pixels))
            });
        }
        let mut done = 0;
        while let Some(result) = tasks.join_next().await {
            if let Ok(Some((id, pixels))) = result {
                if let Ok(mut s) = state.lock() {
                    s.covers.insert(id, pixels);
                }
                done += 1;
                // Show covers in batches, not one redraw per image.
                if done % 16 == 0 {
                    let state = state.clone();
                    with_ui(&weak, move |ui| show_store(&ui, &state));
                }
            }
        }
        with_ui(&weak, move |ui| show_store(&ui, &state));
    });
}

/// Opens the store, loading the catalog through xtux-service when needed (UI thread).
fn open_store(ui: &AppWindow, state: &Shared, handle: &tokio::runtime::Handle, refresh: bool) {
    if !refresh && state.lock().is_ok_and(|s| !s.catalog.is_empty()) {
        return;
    }
    ui.set_store_loading(true);
    ui.set_status("Loading the store catalog…".into());
    let weak = ui.as_weak();
    let state = state.clone();
    let handle2 = handle.clone();
    handle.spawn(async move {
        let result = ask(
            Request::Catalog {
                market: None,
                refresh,
            },
            |_| {},
        )
        .await;
        match result {
            Ok(Response::Catalog { titles, addons }) => {
                let count = titles.len();
                if let Ok(mut s) = state.lock() {
                    s.catalog = titles;
                    s.addons = addons;
                    // The old indices are invalid now; filter_store rebuilds them.
                    s.store_filtered.clear();
                    s.store_current = None;
                }
                with_ui(&weak, move |ui| {
                    ui.set_store_loading(false);
                    ui.set_status(format!("{count} titles in the store").into());
                    filter_store(&ui, &state);
                    load_store_covers(&ui, &state, &handle2);
                });
            }
            Ok(other) => set_status(&weak, format!("Unexpected answer: {other:?}"), None),
            Err(err) => {
                set_status(&weak, format!("Could not load the store: {err}"), None);
                with_ui(&weak, |ui| ui.set_store_loading(false));
            }
        }
    });
}

/// Opens the page of a store title (UI thread); the hero image follows.
fn open_store_title(ui: &AppWindow, state: &Shared, handle: &tokio::runtime::Handle, row: usize) {
    let Some((index, title, cover)) = state.lock().ok().and_then(|s| {
        let index = *s.store_filtered.get(row)?;
        let title = s.catalog.get(index)?.clone();
        let cover = s.covers.get(&title.product_id).cloned();
        Some((index, title, cover))
    }) else {
        return;
    };
    let downloads = match state.lock() {
        Ok(mut s) => {
            s.store_current = Some(index);
            s.downloads.clone()
        }
        Err(_) => Vec::new(),
    };
    ui.set_store_title(store_row(&title, cover.as_ref(), &downloads));
    show_addons(state);
    load_screenshots(ui, &title, handle);
    ui.set_has_store_hero(false);
    ui.set_page(6);
    let online = Settings::load().map(|s| s.ui.online_art).unwrap_or(true);
    let weak = ui.as_weak();
    let state = state.clone();
    handle.spawn(async move {
        let mut hero = art::cached(&title.product_id).hero;
        if hero.is_none() && online {
            let client = art::client(&format!("xtux-launcher/{}", env!("CARGO_PKG_VERSION")));
            hero = art::fetch_from(&client, &title.product_id, None, title.hero_url.as_deref())
                .await
                .ok()
                .and_then(|a| a.hero);
        }
        let backdrop = art::cached(&title.product_id).backdrop;
        let (pixels, soft) = tokio::task::spawn_blocking(move || {
            (
                decode(hero.as_deref(), 1920, 1080),
                decode(backdrop.as_deref(), 640, 360),
            )
        })
        .await
        .unwrap_or_default();
        with_ui(&weak, move |ui| {
            if state.lock().ok().and_then(|s| s.store_current) == Some(index) {
                let (hero, has_hero) = to_image(&pixels);
                ui.set_store_hero_soft(to_image(&soft).0);
                ui.set_store_hero(hero);
                ui.set_has_store_hero(has_hero);
            }
        });
    });
}

/// Free space of the file system holding `path`, as text.
fn free_space(path: &Path) -> String {
    match rustix::fs::statvfs(path) {
        Ok(stat) => format!("{} free", size_text(Some(stat.f_bavail * stat.f_frsize))),
        Err(_) => String::new(),
    }
}

fn download_detail(item: &xtux_protocol::DownloadItem) -> String {
    let amount = match item.total {
        Some(total) if total > 0 => format!(
            "{} of {}",
            size_text(Some(item.done)),
            size_text(Some(total))
        ),
        _ => String::new(),
    };
    let speed = if item.state == "running" && item.speed > 0 {
        let left = item
            .total
            .map(|t| t.saturating_sub(item.done) / item.speed.max(1))
            .map(|secs| match secs {
                s if s >= 3600 => format!(" · {} h {} min left", s / 3600, (s % 3600) / 60),
                s if s >= 60 => format!(" · {} min left", s / 60),
                s => format!(" · {s} s left"),
            })
            .unwrap_or_default();
        format!(" · {}{left}", rate_text(item.speed))
    } else {
        String::new()
    };
    match item.state.as_str() {
        "done" => format!("Installed in {}", item.destination.display()),
        "failed" => item.message.clone(),
        _ if amount.is_empty() => item.message.clone(),
        _ => format!("{amount}{speed} · {}", item.message),
    }
}

/// The library game installed in `folder`.
fn library_entry<'a>(all: &'a [Entry], folder: &Path) -> Option<&'a Entry> {
    let folder = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
    all.iter().find(|e| {
        e.info.path == folder || std::fs::canonicalize(&e.info.path).is_ok_and(|p| p == folder)
    })
}

/// Pushes the download queue to the window (UI thread).
/// Bytes per second as text, also below a megabyte.
fn rate_text(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB/s", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.0} MB/s", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{} KB/s", b >> 10),
        b => format!("{b} B/s"),
    }
}

/// The activity graph of the Downloads page from the rate history (UI thread).
fn show_graph(ui: &AppWindow, history: &[xtux_protocol::RateSample]) {
    let network: Vec<u64> = history.iter().map(|h| h.network).collect();
    let disk: Vec<u64> = history.iter().map(|h| h.disk_write).collect();
    let max = network
        .iter()
        .chain(disk.iter())
        .copied()
        .max()
        .unwrap_or(0)
        .max(1) as f32;
    // Always five minutes wide: older seconds are empty until the history has them.
    let width: usize = 300;
    let mut points = vec![GraphPoint::default(); width.saturating_sub(history.len())];
    points.extend(history.iter().rev().take(width).rev().map(|h| GraphPoint {
        network: h.network as f32 / max,
        disk: h.disk_write as f32 / max,
    }));
    GRAPH.with(|model| update_rows(model, points));
    let now = history.last().copied().unwrap_or_default();
    let peak = network.iter().copied().max().unwrap_or(0);
    ui.set_graph_label(
        if history.is_empty() {
            "No activity yet".to_string()
        } else {
            format!(
                "Download {} · disk write {} · read {} · peak {}",
                rate_text(now.network),
                rate_text(now.disk_write),
                rate_text(now.disk_read),
                rate_text(peak)
            )
        }
        .into(),
    );
}

/// Screenshots of a store title, loaded into the gallery one by one (UI thread starts it).
fn load_screenshots(
    ui: &AppWindow,
    title: &xtux_protocol::CatalogTitle,
    handle: &tokio::runtime::Handle,
) {
    let count = title.screenshots.len().min(8);
    SHOTS.with(|model| {
        model.set_vec(vec![ShotRow::default(); count]);
    });
    let generation = SHOT_GENERATION.with(|g| {
        g.set(g.get() + 1);
        g.get()
    });
    ui.set_shot_open(-1);
    if count == 0 {
        return;
    }
    let online = Settings::load().map(|s| s.ui.online_art).unwrap_or(true);
    let weak = ui.as_weak();
    let id = title.product_id.clone();
    let urls = title.screenshots.clone();
    handle.spawn(async move {
        let client = art::client(&format!("xtux-launcher/{}", env!("CARGO_PKG_VERSION")));
        for (index, url) in urls.into_iter().take(8).enumerate() {
            let cached = art::cache_dir().map(|d| d.join(&id).join(art::screenshot_file(&url)));
            let path = match cached.filter(|p| p.is_file()) {
                Some(path) => Some(path),
                None if online => art::fetch_screenshot(&client, &id, index, &url).await.ok(),
                None => None,
            };
            let pixels = tokio::task::spawn_blocking(move || {
                let sharp = decode(path.as_deref(), 1280, 720)?;
                let soft = blur(&Some(sharp.clone()))?;
                Some((sharp, soft))
            })
            .await
            .ok()
            .flatten();
            let Some((sharp, soft)) = pixels else {
                continue;
            };
            with_ui(&weak, move |_ui| {
                // Still the same title?
                if SHOT_GENERATION.with(|g| g.get()) != generation {
                    return;
                }
                SHOTS.with(|model| {
                    use slint::Model;
                    if index < model.row_count() {
                        model.set_row_data(
                            index,
                            ShotRow {
                                image: Image::from_rgba8(sharp),
                                blur: Image::from_rgba8(soft),
                                loaded: true,
                            },
                        );
                    }
                });
            });
        }
    });
}

fn show_downloads(ui: &AppWindow, state: &Shared) {
    show_updates(state);
    // Rings on store tiles and the title page follow the queue.
    show_store(ui, state);
    if let Some(row) = state.lock().ok().and_then(|s| {
        let title = s.catalog.get(s.store_current?)?;
        Some(store_row(
            title,
            s.covers.get(&title.product_id),
            &s.downloads,
        ))
    }) {
        ui.set_store_title(row);
    }
    show_addons(state);
    let Ok(s) = state.lock() else { return };
    show_graph(ui, &s.history);
    let rows: Vec<DownloadRow> = s
        .downloads
        .iter()
        .map(|item| {
            let cover = s.covers.get(&item.product_id);
            let (cover_blur, _) = to_image(&cover.map(|c| c.1.clone()));
            let (cover_image, has_cover) = to_image(&cover.map(|c| c.0.clone()));
            DownloadRow {
                id: item.id as i32,
                name: item.name.clone().into(),
                state: item.state.clone().into(),
                progress: match (item.state.as_str(), item.total) {
                    ("done", _) => 1.0,
                    (_, Some(total)) if total > 0 => item.done as f32 / total as f32,
                    _ => 0.0,
                },
                detail: download_detail(item).into(),
                playable: item.state == "done"
                    && library_entry(&s.all, &item.destination).is_some(),
                running: library_entry(&s.all, &item.destination)
                    .is_some_and(|e| s.running.contains_key(&e.info.path)),
                cover: cover_image,
                cover_blur,
                has_cover,
            }
        })
        .collect();
    let active = s
        .downloads
        .iter()
        .filter(|i| {
            matches!(
                i.state.as_str(),
                "queued" | "running" | "pausing" | "removing"
            )
        })
        .count();
    ui.set_downloads_active(active as i32);
    let queued = s
        .store_current
        .and_then(|i| s.catalog.get(i))
        .is_some_and(|t| {
            s.downloads
                .iter()
                .any(|d| d.product_id == t.product_id && d.state != "done")
        });
    ui.set_store_title_queued(queued);
    DOWNLOAD_ROWS.with(|model| update_rows(model, rows));
}

/// Sends a queue command to xtux-service and shows the queue it answers with; refreshes the library when an
/// install finished.
fn download_command(
    ui: &AppWindow,
    state: &Shared,
    handle: &tokio::runtime::Handle,
    command: DownloadCommand,
) {
    // One list request at a time: a slow answer is not stacked up by the poll timer.
    let is_list = command == DownloadCommand::List;
    if is_list {
        match state.lock() {
            Ok(mut s) if !s.download_polling => s.download_polling = true,
            _ => return,
        }
    }
    let weak = ui.as_weak();
    let state = state.clone();
    let handle2 = handle.clone();
    handle.spawn(async move {
        let answer = ask(Request::Downloads(command), |_| {}).await;
        if is_list {
            if let Ok(mut s) = state.lock() {
                s.download_polling = false;
            }
        }
        let items = match answer {
            Ok(Response::Downloads { items, history }) => {
                if let Ok(mut s) = state.lock() {
                    s.history = history;
                }
                items
            }
            Ok(other) => return set_status(&weak, format!("Unexpected answer: {other:?}"), None),
            Err(err) => return set_status(&weak, err.to_string(), None),
        };
        // Covers for items without one yet, from the art cache.
        let missing: Vec<String> = match state.lock() {
            Ok(s) => items
                .iter()
                .filter(|i| !s.covers.contains_key(&i.product_id))
                .map(|i| i.product_id.clone())
                .collect(),
            Err(_) => Vec::new(),
        };
        let decoded = tokio::task::spawn_blocking(move || {
            missing
                .into_iter()
                .filter_map(|id| {
                    let sharp = decode(art::cached(&id).cover.as_deref(), 320, 480)?;
                    let soft = blur(&Some(sharp.clone()))?;
                    Some((id, (sharp, soft)))
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        let finished = match state.lock() {
            Ok(mut s) => {
                let finished = items.iter().any(|new| {
                    new.state == "done"
                        && s.downloads
                            .iter()
                            .any(|old| old.id == new.id && old.state != "done")
                });
                s.downloads = items;
                s.covers.extend(decoded);
                finished
            }
            Err(_) => false,
        };
        let weak2 = weak.clone();
        let state2 = state.clone();
        with_ui(&weak, move |ui| show_downloads(&ui, &state2));
        if finished {
            set_status(
                &weak2,
                "A download finished; the game is in the library",
                None,
            );
            refresh_library(weak2.clone(), state.clone()).await;
            with_ui(&weak2, move |ui| {
                reload_store(&ui, &state, &handle2);
                if state.lock().is_ok_and(|s| !s.updates.is_empty()) {
                    check_updates(&ui, &state, &handle2);
                }
            });
        }
    });
}

/// Reloads the catalog from xtux-service's cache, so installed marks are current (UI thread).
fn reload_store(ui: &AppWindow, state: &Shared, handle: &tokio::runtime::Handle) {
    if state.lock().is_ok_and(|s| s.catalog.is_empty()) {
        return;
    }
    if let Ok(mut s) = state.lock() {
        s.catalog.clear();
    }
    open_store(ui, state, handle, false);
}

/// The owned add-ons of the store title on screen.
fn current_addons(s: &State) -> Vec<xtux_protocol::CatalogTitle> {
    let Some(title) = s.store_current.and_then(|i| s.catalog.get(i)) else {
        return Vec::new();
    };
    s.addons
        .iter()
        .filter(|a| title.has_addon(a))
        .cloned()
        .collect()
}

fn show_updates(state: &Shared) {
    let Ok(s) = state.lock() else { return };
    let rows = s
        .updates
        .iter()
        .map(|item| {
            let queued = s
                .downloads
                .iter()
                .any(|d| d.destination == item.path && d.state != "done");
            UpdateRow {
                name: item.name.clone().into(),
                versions: format!(
                    "Installed: {} · Latest: {}",
                    item.installed_version,
                    item.latest_version.as_deref().unwrap_or("unknown")
                )
                .into(),
                detail: item.message.clone().into(),
                available: item.can_update && !queued && !s.running.contains_key(&item.path),
                queued,
            }
        })
        .collect();
    drop(s);
    UPDATES.with(|model| update_rows(model, rows));
}

fn check_updates(ui: &AppWindow, state: &Shared, handle: &tokio::runtime::Handle) {
    if ui.get_updates_loading() {
        return;
    }
    ui.set_updates_loading(true);
    ui.set_updates_status("Checking installed games and add-ons…".into());
    let weak = ui.as_weak();
    let state = state.clone();
    handle.spawn(async move {
        let result = ask(Request::CheckUpdates, |_| {}).await;
        with_ui(&weak, move |ui| {
            ui.set_updates_loading(false);
            match result {
                Ok(Response::Updates { items }) => {
                    let available = items.iter().filter(|i| i.state == "available").count();
                    let failed = items.iter().filter(|i| i.state == "failed").count();
                    ui.set_updates_status(format!("{} packages checked · {available} updates available · {failed} checks failed", items.len()).into());
                    if let Ok(mut s) = state.lock() { s.updates = items; }
                    show_updates(&state);
                }
                Ok(other) => ui.set_updates_status(format!("Unexpected answer: {other:?}").into()),
                Err(err) => ui.set_updates_status(format!("Could not check updates: {err}").into()),
            }
        });
    });
}

/// Shows the add-ons of the store title on screen (UI thread).
fn show_addons(state: &Shared) {
    let Ok(s) = state.lock() else { return };
    let rows: Vec<AddonRow> = current_addons(&s)
        .iter()
        .map(|a| AddonRow {
            name: a.name.clone().into(),
            size: a
                .download_size
                .map(|b| size_text(Some(b)))
                .unwrap_or_default()
                .into(),
            state: if a.installed.is_some() {
                "installed"
            } else if s
                .downloads
                .iter()
                .any(|d| d.product_id == a.product_id && d.state != "done")
            {
                "queued"
            } else {
                "install"
            }
            .into(),
        })
        .collect();
    drop(s);
    ADDONS.with(|model| update_rows(model, rows));
}

/// Queues add-on `index` of the store title on screen, into the library folder of its game (or the first one).
fn store_addon_install(
    ui: &AppWindow,
    state: &Shared,
    handle: &tokio::runtime::Handle,
    index: usize,
) {
    let Some((addon, game_dir)) = state.lock().ok().and_then(|s| {
        let addon = current_addons(&s).into_iter().nth(index)?;
        let game_dir = s
            .store_current
            .and_then(|i| s.catalog.get(i))
            .and_then(|t| t.installed.clone());
        Some((addon, game_dir))
    }) else {
        return;
    };
    let Some(content_id) = addon.content_id.clone() else {
        ui.set_status("This add-on has no PC package to install".into());
        return;
    };
    let settings = Settings::load().unwrap_or_default();
    let library = game_dir
        .as_deref()
        .and_then(Path::parent)
        .and_then(|parent| settings.library_dirs.iter().find(|d| d.as_path() == parent))
        .or_else(|| {
            settings
                .library_dirs
                .iter()
                .find(|dir| dir.is_dir() || std::fs::create_dir_all(dir).is_ok())
        })
        .cloned();
    let Some(library) = library else {
        ui.set_status("No library folder to install into; add one in Settings".into());
        return;
    };
    ui.set_status(format!("{} added to the downloads", addon.name).into());
    download_command(
        ui,
        state,
        handle,
        DownloadCommand::Add {
            product_id: addon.product_id,
            name: addon.name,
            content_id,
            library,
        },
    );
}

/// Capture the exact add-on before opening the confirmation; catalog refreshes can change row indices.
fn confirm_addon_remove(ui: &AppWindow, state: &Shared, index: usize) {
    let Ok(mut s) = state.lock() else { return };
    let Some(addon) = current_addons(&s).into_iter().nth(index) else {
        return;
    };
    let Some(path) = addon.installed.as_ref() else {
        return;
    };
    if !s.running.is_empty() {
        ui.set_status("Stop running games before removing an add-on".into());
        return;
    }
    ui.set_delete_addon_name(addon.name.clone().into());
    ui.set_delete_addon_path(path.to_string_lossy().as_ref().into());
    s.pending_addon_delete = Some(addon);
    ui.set_deleting_addon(true);
    ui.set_confirm_delete(true);
}

fn delete_addon(ui: &AppWindow, state: &Shared, handle: &tokio::runtime::Handle) {
    let addon = state.lock().ok().and_then(|mut s| {
        if !s.running.is_empty() {
            return None;
        }
        s.pending_addon_delete.take()
    });
    let Some(addon) = addon else {
        ui.set_status("Stop running games before removing an add-on".into());
        return;
    };
    let Some(path) = addon.installed else { return };
    ui.set_busy(true);
    ui.set_status(format!("Removing {}…", addon.name).into());
    let weak = ui.as_weak();
    let state = state.clone();
    handle.spawn(async move {
        match ask(Request::Uninstall { path: path.clone() }, |_| {}).await {
            Ok(Response::Uninstalled { .. }) => {
                if let Ok(mut s) = state.lock() {
                    for a in &mut s.addons {
                        if a.installed.as_ref() == Some(&path) {
                            a.installed = None;
                        }
                    }
                }
                set_status(&weak, format!("{} removed", addon.name), Some(false));
                with_ui(&weak, move |_| show_addons(&state));
            }
            Ok(other) => set_status(&weak, format!("Unexpected answer: {other:?}"), Some(false)),
            Err(err) => set_status(&weak, err.to_string(), Some(false)),
        }
    });
}

/// Opens the install dialog for the store title on screen (UI thread).
fn store_install(ui: &AppWindow, state: &Shared) {
    if state.lock().ok().and_then(|s| s.store_current).is_none() {
        return;
    }
    let settings = Settings::load().unwrap_or_default();
    let libraries: Vec<PathBuf> = settings
        .library_dirs
        .iter()
        .filter(|dir| dir.is_dir() || std::fs::create_dir_all(dir).is_ok())
        .cloned()
        .collect();
    let choices: Vec<LibraryChoice> = libraries
        .iter()
        .map(|dir| LibraryChoice {
            path: dir.display().to_string().into(),
            free: free_space(dir).into(),
        })
        .collect();
    if let Ok(mut s) = state.lock() {
        s.install_libraries = libraries;
    }
    ui.set_install_libraries(ModelRc::new(VecModel::from(choices)));
    ui.set_install_choice(0);
    ui.set_install_open(true);
}

/// Licenses of all dependencies, from licenses/THIRD-PARTY-LICENSES.md (cargo-about, checked by the CI).
const THIRD_PARTY: &str = include_str!("../../../licenses/THIRD-PARTY-LICENSES.md");

/// One row per license: name, crates using it, full text.
fn license_rows() -> Vec<LicenseRow> {
    let mut rows = Vec::new();
    let Some(start) = THIRD_PARTY.find("## Licenses and the crates that use them") else {
        return rows;
    };
    for section in THIRD_PARTY[start..].split("\n### ").skip(1) {
        let name = section
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        let crates: Vec<String> = section
            .lines()
            .find_map(|l| l.strip_prefix("Used by: "))
            .unwrap_or_default()
            .split("), ")
            .filter_map(|link| {
                link.strip_prefix('[')?
                    .split(']')
                    .next()
                    .map(str::to_string)
            })
            .collect();
        let text = section
            .split("```text")
            .nth(1)
            .and_then(|t| t.split("```").next())
            .unwrap_or_default()
            .trim()
            .to_string();
        rows.push(LicenseRow {
            name: name.into(),
            count: crates.len() as i32,
            crates: crates.join(", ").into(),
            text: text.into(),
        });
    }
    rows.sort_by_key(|r| -r.count);
    // The same license with different copyright lines comes as several texts; number them.
    let mut seen: HashMap<String, usize> = HashMap::new();
    let totals: HashMap<String, usize> = rows.iter().fold(HashMap::new(), |mut m, r| {
        *m.entry(r.name.to_string()).or_default() += 1;
        m
    });
    for row in &mut rows {
        let name = row.name.to_string();
        if totals[&name] > 1 {
            let n = seen.entry(name.clone()).or_default();
            *n += 1;
            row.name = format!("{name} · text {n} of {}", totals[&name]).into();
        }
    }
    rows
}

/// Asks a running game to quit, like closing its window.
fn stop_game(ui: &AppWindow, state: &Shared, handle: &tokio::runtime::Handle, path: &Path) {
    let Some(canceller) = state.lock().ok().and_then(|s| s.running.get(path).cloned()) else {
        return;
    };
    ui.set_status("Asking the game to quit…".into());
    let weak = ui.as_weak();
    handle.spawn(async move {
        if let Err(err) = canceller.cancel_current().await {
            set_status(&weak, format!("Could not stop the game: {err}"), None);
        }
    });
}

/// Deletes the game open on the game page, after the confirmation in the window.
fn delete_game(ui: &AppWindow, state: &Shared, handle: &tokio::runtime::Handle) {
    let Some(game) = state
        .lock()
        .ok()
        .and_then(|s| s.current.and_then(|i| s.all.get(i).map(|e| e.info.clone())))
    else {
        return;
    };
    if state
        .lock()
        .is_ok_and(|s| s.running.contains_key(&game.path))
    {
        ui.set_status("Stop the game before deleting it".into());
        return;
    }
    ui.set_busy(true);
    ui.set_status(format!("Deleting {}…", game.name).into());
    let weak = ui.as_weak();
    let state = state.clone();
    handle.spawn(async move {
        match ask(
            Request::Uninstall {
                path: game.path.clone(),
            },
            |_| {},
        )
        .await
        {
            Ok(Response::Uninstalled { .. }) => {
                set_status(&weak, format!("{} deleted", game.name), Some(false));
                with_ui(&weak, |ui| ui.set_page(0));
                refresh_library(weak, state).await;
            }
            Ok(other) => set_status(&weak, format!("Unexpected answer: {other:?}"), Some(false)),
            Err(err) => set_status(&weak, err.to_string(), Some(false)),
        }
    });
}

/// Colours of the chosen theme and accent (contrast checked in theme.rs).
fn apply_theme(ui: &AppWindow) {
    let theme = ui.global::<Theme>();
    let variant = theme::variant(
        &theme.get_choice(),
        &theme.get_dark_variant(),
        theme.get_system_dark(),
    );
    let p = theme::palette(variant, &theme.get_accent_name());
    theme.set_light(variant == theme::Variant::Light);
    theme.set_base(p.base.to_slint());
    theme.set_base_2(p.base_2.to_slint());
    theme.set_text(p.text.to_slint());
    theme.set_muted(p.muted.to_slint());
    theme.set_glass(p.glass.to_slint());
    theme.set_glass_strong(p.glass_strong.to_slint());
    theme.set_glass_border(p.glass_border.to_slint());
    theme.set_shine(p.shine.to_slint());
    theme.set_shadow(p.shadow.to_slint());
    theme.set_field(p.field.to_slint());
    theme.set_accent(p.accent.to_slint());
    theme.set_accent_text(p.accent_text.to_slint());
    theme.set_on_accent(p.on_accent.to_slint());
    theme.set_danger(p.danger.to_slint());
    theme.set_on_danger(p.on_danger.to_slint());
    theme.set_warning(p.warning.to_slint());
    theme.set_on_warning(p.on_warning.to_slint());
    theme.set_backdrop_opacity(p.backdrop_opacity);
    theme.set_veil_top(p.veil_min);
    ui.invoke_apply_theme();
    apply_icon(ui, p.accent);
}

/// The chosen app icon, in brand colours or recoloured to the accent, for the sidebar and the window.
fn apply_icon(ui: &AppWindow, accent: theme::Rgba) {
    let bytes: &[u8] = match ui.get_icon_choice().as_str() {
        "mark" => include_bytes!("../assets/icon-mark.png"),
        "symbol" => include_bytes!("../assets/icon-symbol.png"),
        "mono" => include_bytes!("../assets/icon-mono.png"),
        _ => include_bytes!("../assets/icon-mascot.png"),
    };
    let Ok(image) = image::load_from_memory(bytes) else {
        return;
    };
    let mut rgba = image.to_rgba8();
    if ui.get_icon_colour() == "accent" {
        // Colourful and white pixels take the accent with their brightness; dark ones (badges, outlines) stay.
        for pixel in rgba.pixels_mut() {
            let [r, g, b, _] = pixel.0;
            let max = r.max(g).max(b);
            let min = r.min(g).min(b);
            let colourful = max - min > 60 || min > 200;
            if colourful {
                let v = max as f32 / 255.0;
                pixel.0[0] = (accent.r as f32 * v) as u8;
                pixel.0[1] = (accent.g as f32 * v) as u8;
                pixel.0[2] = (accent.b as f32 * v) as u8;
            }
        }
    }
    let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
    );
    ui.set_app_icon(Image::from_rgba8(buffer));
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
            "store" => {
                ui.set_page(5);
                open_store(&ui, &state, &handle, false);
            }
            "downloads" => {
                ui.set_page(7);
                download_command(&ui, &state, &handle, DownloadCommand::List);
            }
            "updates" => {
                ui.set_page(8);
                check_updates(&ui, &state, &handle);
            }
            "install" => {
                ui.set_store_search("digging".into());
                filter_store(&ui, &state);
                open_store_title(&ui, &state, &handle, 0);
                store_install(&ui, &state);
            }
            "store-title" => {
                ui.set_store_search("farming".into());
                filter_store(&ui, &state);
                open_store_title(&ui, &state, &handle, 0);
            }
            "addons" | "addon-delete" => {
                let title = state.lock().ok().and_then(|s| {
                    s.catalog
                        .iter()
                        .find(|t| s.addons.iter().any(|a| t.has_addon(a)))
                        .map(|t| t.name.clone())
                });
                if let Some(title) = title {
                    ui.set_store_search(title.into());
                    filter_store(&ui, &state);
                    open_store_title(&ui, &state, &handle, 0);
                    if page == "addon-delete" {
                        let installed = state.lock().ok().and_then(|s| {
                            current_addons(&s)
                                .iter()
                                .position(|a| a.installed.is_some())
                        });
                        if let Some(index) = installed {
                            confirm_addon_remove(&ui, &state, index);
                        }
                    }
                }
            }
            "about" => {
                ui.set_page(4);
                ui.set_license_open(0);
            }
            "delete" => {
                open_game(&ui, &state, &handle, 0);
                ui.set_confirm_delete(true);
            }
            // Shows the first game as running (display only) to check the running look.
            "running" => {
                use slint::Model;
                let games = ui.get_games();
                if let Some(mut row) = games.row_data(0) {
                    row.running = true;
                    games.set_row_data(0, row);
                }
            }
            "logs" => {
                ui.set_page(3);
                show_logs(&ui, None);
            }
            _ => ui.set_view("grid".into()),
        }
        let weak = weak.clone();
        let file = file.clone();
        let delay = std::env::var("XTUX_SNAPSHOT_DELAY")
            .ok()
            .and_then(|d| d.parse().ok())
            .unwrap_or(2);
        slint::Timer::single_shot(Duration::from_secs(delay), move || {
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
    let args = <Args as clap::Parser>::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("XODUS_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let runtime = tokio::runtime::Runtime::new().expect("cannot start the async runtime");
    let ui = AppWindow::new().expect("cannot create the window");
    GAMES.with(|model| ui.set_games(ModelRc::from(model.clone())));
    STORE.with(|model| ui.set_store(ModelRc::from(model.clone())));
    let state: Shared = Arc::default();
    if let Ok(mut s) = state.lock() {
        s.handle = Some(runtime.handle().clone());
        s.startup = match (args.play, args.game, args.page) {
            (Some(game), _, _) => Some(Startup::Play(game)),
            (_, Some(game), _) => Some(Startup::Game(game)),
            (_, _, Some(page)) => Some(Startup::Page(page)),
            _ => None,
        };
    }
    let handle = runtime.handle().clone();

    let settings = Settings::load().unwrap_or_default();
    {
        let theme = ui.global::<Theme>();
        theme.set_choice(settings.ui.theme.clone().into());
        theme.set_dark_variant(settings.ui.dark_variant.clone().into());
        theme.set_accent_name(settings.ui.accent.clone().into());
        ui.set_view(settings.ui.view.clone().into());
        ui.set_online_art(settings.ui.online_art);
        ui.set_backdrop_mode(settings.ui.backdrop.clone().into());
        ui.set_icon_choice(settings.ui.icon.clone().into());
        ui.set_icon_colour(settings.ui.icon_colour.clone().into());
        ui.set_anticheat_guess(settings.ui.anticheat_guess);
        ui.set_controller_mode(settings.controller.as_str().into());
        if let Ok(mut s) = state.lock() {
            s.backdrop_mode = settings.ui.backdrop.clone();
        }
        apply_theme(&ui);
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
                show_backdrop(&ui, &state, index, BackdropCause::Game);
            }
        }
    });

    ui.on_ui_changed({
        let weak = ui.as_weak();
        let state = state.clone();
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
            settings.ui.online_art = ui.get_online_art();
            settings.ui.backdrop = ui.get_backdrop_mode().into();
            settings.ui.icon = ui.get_icon_choice().into();
            settings.ui.icon_colour = ui.get_icon_colour().into();
            settings.ui.anticheat_guess = ui.get_anticheat_guess();
            settings.controller =
                ControllerMode::parse(&ui.get_controller_mode()).unwrap_or_default();
            apply_theme(&ui);
            // Show the backdrop setting at once.
            if let Ok(mut s) = state.lock() {
                s.backdrop_mode = settings.ui.backdrop.clone();
                s.backdrop_of = None;
            }
            let first = state.lock().ok().and_then(|s| {
                s.current
                    .or_else(|| s.all.iter().position(|e| e.backdrop.is_some()))
            });
            if let Some(first) = first {
                show_backdrop(&ui, &state, first, BackdropCause::Ambient);
            } else if settings.ui.backdrop == "off" {
                ui.set_has_backdrop(false);
            }
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
                controller: ControllerMode::parse(&ui.get_game_controller()),
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
                start_game(&ui, &state, &handle, game);
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
                start_game(&ui, &state, &handle, game);
            }
        }
    });

    ui.on_stop({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move |row| {
            let Some(ui) = weak.upgrade() else { return };
            let path = state.lock().ok().and_then(|s| {
                s.shown
                    .get(row as usize)
                    .and_then(|&i| s.all.get(i).map(|e| e.info.path.clone()))
            });
            if let Some(path) = path {
                stop_game(&ui, &state, &handle, &path);
            }
        }
    });

    ui.on_stop_game({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let path = state.lock().ok().and_then(|s| {
                s.current
                    .and_then(|i| s.all.get(i).map(|e| e.info.path.clone()))
            });
            if let Some(path) = path {
                stop_game(&ui, &state, &handle, &path);
            }
        }
    });

    ui.on_delete_game({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_deleting_addon() {
                    delete_addon(&ui, &state, &handle);
                } else {
                    delete_game(&ui, &state, &handle);
                }
            }
        }
    });

    ui.set_version(env!("CARGO_PKG_VERSION").into());
    ui.set_licenses(ModelRc::new(VecModel::from(license_rows())));
    ui.on_open_url(|url| {
        // Fixed https links of the About page only.
        if url.starts_with("https://") {
            let _ = std::process::Command::new("xdg-open")
                .arg(url.as_str())
                .spawn();
        }
    });

    ui.on_open_store({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move || {
            if let Some(ui) = weak.upgrade() {
                open_store(&ui, &state, &handle, false);
            }
        }
    });

    ui.on_store_refresh({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move || {
            if let Some(ui) = weak.upgrade() {
                open_store(&ui, &state, &handle, true);
            }
        }
    });

    ui.on_store_filter_changed({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move || {
            if let Some(ui) = weak.upgrade() {
                filter_store(&ui, &state);
                load_store_covers(&ui, &state, &handle);
            }
        }
    });

    ui.on_store_more({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            if let Ok(mut s) = state.lock() {
                s.store_limit += STORE_PAGE;
            }
            show_store(&ui, &state);
            load_store_covers(&ui, &state, &handle);
        }
    });

    ui.on_open_store_title({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move |row| {
            if let Some(ui) = weak.upgrade() {
                open_store_title(&ui, &state, &handle, row as usize);
            }
        }
    });

    ui.on_store_play({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let game = state.lock().ok().and_then(|s| {
                let path = s.catalog.get(s.store_current?)?.installed.clone()?;
                s.all
                    .iter()
                    .find(|e| e.info.path == path)
                    .map(|e| e.info.clone())
            });
            if let Some(game) = game {
                start_game(&ui, &state, &handle, game);
            }
        }
    });

    DOWNLOAD_ROWS.with(|model| ui.set_downloads(ModelRc::from(model.clone())));
    SHOTS.with(|model| ui.set_store_shots(ModelRc::from(model.clone())));
    ADDONS.with(|model| ui.set_store_addons(ModelRc::from(model.clone())));
    UPDATES.with(|model| ui.set_updates(ModelRc::from(model.clone())));
    ui.on_check_updates({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move || {
            if let Some(ui) = weak.upgrade() {
                check_updates(&ui, &state, &handle);
            }
        }
    });
    ui.on_install_update({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move |index| {
            let Some(ui) = weak.upgrade() else { return };
            let item = usize::try_from(index).ok().and_then(|index| {
                state.lock().ok().and_then(|s| {
                    s.updates
                        .get(index)
                        .filter(|i| i.can_update && !s.running.contains_key(&i.path))
                        .cloned()
                })
            });
            if let Some(item) = item {
                download_command(
                    &ui,
                    &state,
                    &handle,
                    DownloadCommand::Update { path: item.path },
                );
            }
        }
    });
    ui.on_store_addon_remove({
        let weak = ui.as_weak();
        let state = state.clone();
        move |index| {
            if let (Some(ui), Ok(index)) = (weak.upgrade(), usize::try_from(index)) {
                confirm_addon_remove(&ui, &state, index);
            }
        }
    });

    ui.on_store_addon_install({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move |index| {
            if let (Some(ui), Ok(index)) = (weak.upgrade(), usize::try_from(index)) {
                store_addon_install(&ui, &state, &handle, index);
            }
        }
    });
    GRAPH.with(|model| ui.set_graph(ModelRc::from(model.clone())));

    ui.on_store_download_toggle({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let item = state.lock().ok().and_then(|s| {
                let title = s.catalog.get(s.store_current?)?;
                s.downloads
                    .iter()
                    .rev()
                    .find(|d| d.product_id == title.product_id && d.state != "done")
                    .map(|d| (d.id, d.state.clone()))
            });
            let Some((id, state_name)) = item else { return };
            let command = match state_name.as_str() {
                "running" | "queued" => DownloadCommand::Pause { id },
                "paused" | "failed" => DownloadCommand::Resume { id },
                _ => return,
            };
            download_command(&ui, &state, &handle, command);
        }
    });

    ui.on_open_downloads({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move || {
            if let Some(ui) = weak.upgrade() {
                download_command(&ui, &state, &handle, DownloadCommand::List);
            }
        }
    });

    ui.on_download_action({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move |id, action| {
            let Some(ui) = weak.upgrade() else { return };
            let id = id as u64;
            if action == "play" {
                let game = state.lock().ok().and_then(|s| {
                    let item = s.downloads.iter().find(|d| d.id == id)?;
                    library_entry(&s.all, &item.destination).map(|e| e.info.clone())
                });
                if let Some(game) = game {
                    start_game(&ui, &state, &handle, game);
                }
                return;
            }
            let command = match action.as_str() {
                "pause" => DownloadCommand::Pause { id },
                "resume" => DownloadCommand::Resume { id },
                "up" => DownloadCommand::Move { id, up: true },
                "down" => DownloadCommand::Move { id, up: false },
                "remove" => DownloadCommand::Remove { id },
                "clear" => DownloadCommand::ClearDone,
                _ => return,
            };
            download_command(&ui, &state, &handle, command);
        }
    });

    ui.on_store_install({
        let weak = ui.as_weak();
        let state = state.clone();
        move || {
            if let Some(ui) = weak.upgrade() {
                store_install(&ui, &state);
            }
        }
    });

    ui.on_confirm_install({
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let choice = ui.get_install_choice().max(0) as usize;
            let Some((title, library)) = state.lock().ok().and_then(|s| {
                let title = s.catalog.get(s.store_current?)?.clone();
                let library = s.install_libraries.get(choice)?.clone();
                Some((title, library))
            }) else {
                return;
            };
            let Some(content_id) = title.content_id.clone() else {
                ui.set_status("This title has no PC package to install".into());
                return;
            };
            ui.set_status(format!("{} added to the downloads", title.name).into());
            download_command(
                &ui,
                &state,
                &handle,
                DownloadCommand::Add {
                    product_id: title.product_id,
                    name: title.name,
                    content_id,
                    library,
                },
            );
        }
    });

    // Progress: ask for the queue every second while it runs or its page is open.
    let poll = slint::Timer::default();
    poll.start(slint::TimerMode::Repeated, Duration::from_secs(1), {
        let weak = ui.as_weak();
        let state = state.clone();
        let handle = handle.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_page() == 7 || ui.get_downloads_active() > 0 {
                download_command(&ui, &state, &handle, DownloadCommand::List);
            }
        }
    });
    // Once at start: a queue left from last time shows its count.
    download_command(&ui, &state, &handle, DownloadCommand::List);

    schedule_snapshot(&ui, &state, &handle);
    ui.run().expect("the window failed");
}

#[cfg(test)]
mod tests {
    fn entry(name: &str, path: &str) -> super::Entry {
        super::Entry {
            info: xtux_protocol::GameInfo {
                name: name.into(),
                path: path.into(),
                kind: "windows".into(),
                title_id: None,
                store_id: None,
                license_problem: None,
                anticheat: None,
                anticheat_blocks: false,
            },
            art: Default::default(),
            cover: None,
            cover_blur: None,
            logo: None,
            backdrop: None,
        }
    }

    #[test]
    fn finds_games_by_name_or_folder() {
        let all = vec![
            entry(
                "Farming Simulator 25 (PC)",
                "/g/XboxGames/Farming Simulator 25 (PC)",
            ),
            entry("Farming Simulator 25 (PC)", "/g/XodusGames/LS25"),
            entry("Atomfall", "/g/XboxGames/Atomfall"),
        ];
        assert_eq!(super::find_game(&all, "atomfall"), Ok(2));
        assert_eq!(super::find_game(&all, "LS25"), Ok(1));
        assert!(super::find_game(&all, "Farming Simulator 25 (PC)").is_err());
        assert!(super::find_game(&all, "Nope").is_err());
    }

    #[test]
    fn third_party_licenses_parse() {
        let rows = super::license_rows();
        assert!(rows.len() > 5);
        let mit = rows
            .iter()
            .find(|r| r.name.contains("(MIT)"))
            .expect("MIT is used");
        assert!(mit.count > 100);
        assert!(mit.text.contains("Permission is hereby granted"));
    }
}
