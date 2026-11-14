//! xTux Launcher: a thin graphical client of `xtux-service`.
//!
//! The Slint event loop runs on the main thread; daemon requests run on a tokio runtime and hand their results back
//! with `slint::invoke_from_event_loop`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};
use xtux_games::settings::Settings;
use xtux_protocol::{Client, ClientError, Event, GameInfo, Request, Response};

slint::include_modules!();

/// Games currently shown, in list order, so a Play click can find its folder.
type Shown = Arc<Mutex<Vec<GameInfo>>>;

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
    let market = settings.market.clone();

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
        let market = market.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let wine = ui.get_wine().trim().to_string();
            let prefix = ui.get_prefix().trim().to_string();
            let settings = Settings {
                library_dirs: ui
                    .get_library_dirs()
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(expand_home)
                    .collect(),
                wine: (!wine.is_empty()).then(|| expand_home(&wine).to_string_lossy().into_owned()),
                prefix: (!prefix.is_empty()).then(|| expand_home(&prefix)),
                market: market.clone(),
            };
            match settings.save() {
                Ok(()) => {
                    ui.set_status("Settings saved".into());
                    let weak = weak.clone();
                    let shown = shown.clone();
                    handle.spawn(refresh_library(weak, shown));
                }
                Err(err) => ui.set_status(format!("Could not save the settings: {err}").into()),
            }
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
            let wine = ui.get_wine().trim().to_string();
            if wine.is_empty() {
                ui.set_status("Set the Wine binary in Settings first".into());
                ui.set_page(1);
                return;
            }
            let mut env: Vec<(String, String)> = std::env::vars().collect();
            let prefix = ui.get_prefix().trim().to_string();
            if !prefix.is_empty() {
                env.retain(|(k, _)| k != "WINEPREFIX");
                env.push((
                    "WINEPREFIX".into(),
                    expand_home(&prefix).to_string_lossy().into_owned(),
                ));
            }
            let request = Request::Launch {
                source: game.path.clone(),
                wine: expand_home(&wine).to_string_lossy().into_owned(),
                exe: None,
                market: market.clone(),
                env,
            };
            ui.set_busy(true);
            ui.set_status(format!("Starting {}…", game.name).into());
            let weak = weak.clone();
            handle.spawn(async move {
                let name = game.name.clone();
                let events = weak.clone();
                let result = ask(request, move |event| match event {
                    Event::Message { text } => set_status(&events, format!("{name}: {text}"), None),
                    Event::GameStarted { .. } => {
                        set_status(&events, format!("{name} is running"), None)
                    }
                    _ => {}
                })
                .await;
                match result {
                    Ok(Response::GameExited { code }) => set_status(
                        &weak,
                        format!("{} exited (code {})", game.name, code.unwrap_or(0)),
                        Some(false),
                    ),
                    Ok(other) => {
                        set_status(&weak, format!("Unexpected answer: {other:?}"), Some(false))
                    }
                    Err(err) => set_status(&weak, err.to_string(), Some(false)),
                }
            });
        }
    });

    ui.run().expect("the window failed");
}
