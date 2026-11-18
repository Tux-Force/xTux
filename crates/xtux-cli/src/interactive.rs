//! `xtux-cli` without a command: a home screen with the state of the setup and a menu, and `xtux-cli setup`, the
//! guided first setup (sign-in, Tux-Force Wine, prefix, library folders, controllers). Every step checks what is
//! already in place, so running the setup again only does what is missing or what the user wants to change.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use inquire::ui::{Color, RenderConfig, StyleSheet, Styled};
use inquire::{Confirm, InquireError, Select, Text};
use xodus::tokens::TokenManager;
use xtux_games::settings::{ControllerMode, Settings};
use xtux_games::setup::SetupState;

use crate::{banner, commands};

/// Prompts in the brand colour; plain when colours are off.
fn apply_theme() {
    if std::env::var_os("NO_COLOR").is_some() {
        inquire::set_global_render_config(RenderConfig::empty());
        return;
    }
    let lime = Color::rgb(163, 230, 53);
    let config = RenderConfig::default_colored()
        .with_prompt_prefix(Styled::new("◆").with_fg(lime))
        .with_answered_prompt_prefix(Styled::new("✓").with_fg(lime))
        .with_highlighted_option_prefix(Styled::new("›").with_fg(lime))
        .with_selected_option(Some(StyleSheet::new().with_fg(lime)))
        .with_help_message(StyleSheet::new().with_fg(Color::DarkGrey));
    inquire::set_global_render_config(config);
}

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn ok(text: impl std::fmt::Display) {
    println!("  {} {text}", style("✓").green().bold());
}

fn missing(text: impl std::fmt::Display) {
    println!("  {} {text}", style("•").yellow().bold());
}

fn heading(step: usize, total: usize, text: &str) {
    println!();
    println!(
        "{} {}",
        style(format!("[{step}/{total}]")).dim(),
        style(text).bold()
    );
}

/// Cancelled prompts (Esc, Ctrl+C) end the flow instead of being errors.
fn answered<T>(result: Result<T, InquireError>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => None,
        Err(err) => {
            eprintln!("{err}");
            None
        }
    }
}

/// Reports library progress on a spinner that turns into a bar once byte counts arrive.
struct Bar(ProgressBar);

impl Bar {
    fn new() -> Self {
        let bar = ProgressBar::new_spinner();
        bar.set_style(
            ProgressStyle::with_template("  {spinner:.green} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        bar.enable_steady_tick(Duration::from_millis(90));
        Self(bar)
    }

    fn finish(&self, text: impl std::fmt::Display) {
        self.0.finish_and_clear();
        ok(text);
    }

    fn fail(&self, text: impl std::fmt::Display) {
        self.0.finish_and_clear();
        println!("  {} {text}", style("✗").red().bold());
    }
}

impl xtux_games::Progress for Bar {
    fn message(&self, text: &str) {
        self.0.set_message(text.to_string());
    }

    fn bytes(&self, done: u64, total: Option<u64>) {
        if let Some(total) = total
            && self.0.length() != Some(total)
        {
            self.0.set_length(total);
            self.0.set_style(
                ProgressStyle::with_template(
                    "  {msg}\n  [{bar:30.green/dim}] {bytes}/{total_bytes} {bytes_per_sec} {eta}",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("━━─"),
            );
        }
        self.0.set_position(done);
    }
}

fn signed_in_user(tokens: &TokenManager) -> Option<String> {
    let user = tokens.get_user().ok()?;
    tokens.get_user_sts_token().ok()?;
    Some(user.username)
}

fn has_display() -> bool {
    std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

fn show_state(settings: &Settings, state: &SetupState, user: Option<&str>) {
    match user {
        Some(user) => ok(format!("Signed in as {}", style(user).bold())),
        None => missing("Not signed in"),
    }
    match (&state.wine, &state.managed_wine, &state.missing_wine) {
        (Some(_), Some(managed), _) if settings.wine.is_none() => {
            ok(format!("Tux-Force Wine {}", style(&managed.tag).dim()))
        }
        (Some(wine), _, _) => ok(format!("Wine {}", style(wine).dim())),
        (None, _, Some(wine)) => missing(format!("Wine not found: {wine}")),
        (None, _, None) => missing("No Wine yet"),
    }
    if state.prefix_ready {
        ok(format!("Prefix {}", style(state.prefix.display()).dim()));
    } else {
        missing(format!("Prefix {} not prepared", state.prefix.display()));
    }
    let games = xtux_games::library::scan(&settings.library_dirs).len();
    let folders = settings.library_dirs.len();
    ok(format!(
        "{games} game{} in {folders} library folder{}",
        if games == 1 { "" } else { "s" },
        if folders == 1 { "" } else { "s" }
    ));
}

/// `xtux-cli` without a command.
pub async fn home(client: &reqwest::Client, tokens: &TokenManager) -> ExitCode {
    if !interactive() {
        // Scripts and pipes get the plain overview.
        let settings = Settings::load().unwrap_or_default();
        let state = xtux_games::setup::state(&settings);
        show_state(&settings, &state, signed_in_user(tokens).as_deref());
        println!("\nRun `xtux-cli --help` for the commands.");
        return ExitCode::SUCCESS;
    }
    apply_theme();
    banner::show();
    println!(
        "  {}  {}",
        style("xTux").green().bold(),
        style("Xbox PC games on Linux").dim()
    );
    println!();

    let settings = Settings::load().unwrap_or_default();
    let state = xtux_games::setup::state(&settings);
    let user = signed_in_user(tokens);
    show_state(&settings, &state, user.as_deref());
    if user.is_none() || !state.complete() {
        println!();
        println!(
            "  {}",
            style("A few things are still missing. The setup takes care of them.").yellow()
        );
        let start = answered(
            Confirm::new("Start the setup now?")
                .with_default(true)
                .prompt(),
        );
        if start != Some(true) {
            return ExitCode::SUCCESS;
        }
        if setup_steps(client, tokens).await.is_none() {
            return ExitCode::SUCCESS;
        }
    }
    menu(client, tokens).await
}

const PLAY: &str = "▶  Play a game";
const UPDATES: &str = "↻  Check for updates";
const DOWNLOADS: &str = "⬇  Downloads";
const SETUP: &str = "⚙  Setup";
const QUIT: &str = "✕  Quit";

async fn menu(client: &reqwest::Client, tokens: &TokenManager) -> ExitCode {
    loop {
        println!();
        let Some(choice) = answered(
            Select::new(
                "What would you like to do?",
                vec![PLAY, UPDATES, DOWNLOADS, SETUP, QUIT],
            )
            .with_help_message("↑↓ to move, Enter to choose, Esc to quit")
            .prompt(),
        ) else {
            return ExitCode::SUCCESS;
        };
        match choice {
            PLAY => play(client, tokens).await,
            UPDATES => {
                let _ = commands::downloads::run(vec!["check".into()]).await;
            }
            DOWNLOADS => {
                let _ = commands::downloads::run(vec!["list".into()]).await;
            }
            SETUP => {
                setup_steps(client, tokens).await;
            }
            _ => return ExitCode::SUCCESS,
        }
    }
}

async fn play(client: &reqwest::Client, tokens: &TokenManager) {
    let settings = Settings::load().unwrap_or_default();
    let mut games = xtux_games::library::scan(&settings.library_dirs);
    games.sort_by_key(|g| g.name.to_lowercase());
    if games.is_empty() {
        missing("No games in the library folders yet. Install one with the launcher's Store.");
        return;
    }
    let names: Vec<String> = games.iter().map(|g| g.name.clone()).collect();
    let Some(name) = answered(
        Select::new("Which game?", names.clone())
            .with_page_size(12)
            .with_help_message("type to filter")
            .prompt(),
    ) else {
        return;
    };
    let Some(game) = games.iter().find(|g| g.name == name) else {
        return;
    };
    match commands::run::prepare(
        game.path.to_string_lossy().into_owned(),
        None,
        None,
        None,
        Vec::new(),
    ) {
        Ok(options) => {
            let _ = commands::run::run(client, tokens, options).await;
        }
        Err(err) => missing(err),
    }
}

/// `xtux-cli setup`.
pub async fn setup(client: &reqwest::Client, tokens: &TokenManager) -> ExitCode {
    if !interactive() {
        eprintln!("The setup asks questions; run it in a terminal.");
        return ExitCode::FAILURE;
    }
    apply_theme();
    banner::show();
    match setup_steps(client, tokens).await {
        Some(()) => ExitCode::SUCCESS,
        None => ExitCode::FAILURE,
    }
}

const STEPS: usize = 5;

/// The guided setup; `None` when the user stopped it or a step failed.
async fn setup_steps(client: &reqwest::Client, tokens: &TokenManager) -> Option<()> {
    println!(
        "  {}",
        style("Let's get you playing. Steps that are already done are skipped.").bold()
    );
    let mut settings = match Settings::load() {
        Ok(settings) => settings,
        Err(err) => {
            eprintln!("The settings file has an error, fix or remove it first: {err}");
            return None;
        }
    };

    // 1. Account
    heading(1, STEPS, "Microsoft account");
    match signed_in_user(tokens) {
        Some(user) => ok(format!("Signed in as {user}")),
        None => {
            println!(
                "  Games need the licenses of your account. Microsoft's sign-in opens in a window."
            );
            if !has_display() {
                missing(
                    "No graphical session found; sign in later with `xtux-cli login` on the desktop.",
                );
            } else if answered(Confirm::new("Sign in now?").with_default(true).prompt())? {
                if commands::login::run(client, tokens).await != ExitCode::SUCCESS {
                    missing("Not signed in; you can do it later with `xtux-cli login`.");
                } else if let Some(user) = signed_in_user(tokens) {
                    ok(format!("Signed in as {user}"));
                }
            }
        }
    }

    // 2. Wine
    heading(2, STEPS, "Wine");
    let state = xtux_games::setup::state(&settings);
    let wine = wine_step(client, &mut settings, &state).await?;

    // 3. Prefix
    heading(3, STEPS, "Wine prefix");
    let prefix = prefix_step(client, &mut settings, &state, &wine).await?;

    // 4. Library folders
    heading(4, STEPS, "Game library");
    library_step(&mut settings)?;

    // 5. Controllers
    heading(5, STEPS, "Controllers");
    let choice = answered(
        Select::new(
            "How should PlayStation controllers reach games?",
            vec![
                "As Xbox gamepad (recommended, works in every Xbox game)",
                "As PlayStation controller (only for games with their own support)",
            ],
        )
        .with_starting_cursor(usize::from(settings.controller == ControllerMode::Native))
        .prompt(),
    )?;
    settings.controller = if choice.starts_with("As Xbox") {
        ControllerMode::Xbox
    } else {
        ControllerMode::Native
    };
    if let Err(err) =
        xtux_games::prefix::apply_controller_mode(&wine, &prefix, settings.controller).await
    {
        missing(format!("Could not set the controller mode yet: {err}"));
    }

    if let Err(err) = settings.save() {
        eprintln!("Could not save the settings: {err}");
        return None;
    }
    println!();
    println!(
        "  {} {}",
        style("All set.").green().bold(),
        style("Pick a game and play.").bold()
    );
    Some(())
}

async fn wine_step(
    client: &reqwest::Client,
    settings: &mut Settings,
    state: &SetupState,
) -> Option<String> {
    const DOWNLOAD: &str = "Download Tux-Force Wine (recommended)";
    const OWN: &str = "Use a Wine I already have";
    const KEEP: &str = "Keep the current one";

    let bar_check = Bar::new();
    bar_check
        .0
        .set_message("Looking for the newest Tux-Force Wine");
    let latest = xtux_games::wine::latest(client).await;
    bar_check.0.finish_and_clear();

    let mut options = Vec::new();
    if let Some(wine) = &state.wine {
        match (&state.managed_wine, &latest) {
            (Some(managed), Ok(release))
                if settings.wine.is_none() && managed.tag != release.tag =>
            {
                missing(format!(
                    "Tux-Force Wine {} is installed, {} is available",
                    managed.tag, release.tag
                ));
                options.push(DOWNLOAD);
                options.push(KEEP);
            }
            _ => {
                ok(format!("Using {wine}"));
                return Some(wine.clone());
            }
        }
    } else {
        if let Some(missing_wine) = &state.missing_wine {
            missing(format!("{missing_wine} from the settings does not exist"));
        }
        match &latest {
            Ok(_) => options.push(DOWNLOAD),
            Err(err) => missing(format!(
                "Tux-Force Wine cannot be downloaded right now: {err}"
            )),
        }
        options.push(OWN);
    }

    let choice = answered(Select::new("Which Wine should start your games?", options).prompt())?;
    match choice {
        KEEP => state.wine.clone(),
        DOWNLOAD => {
            let release = latest.ok()?;
            let bar = Bar::new();
            match xtux_games::wine::install(client, &release, &bar).await {
                Ok(installed) => {
                    bar.finish(format!("Tux-Force Wine {} installed", installed.tag));
                    // No path in the settings: xTux keeps using its current Tux-Force Wine.
                    settings.wine = None;
                    Some(installed.wine.to_string_lossy().into_owned())
                }
                Err(err) => {
                    bar.fail(err);
                    None
                }
            }
        }
        _ => {
            let path = answered(
                Text::new("Path to the wine binary:")
                    .with_help_message("for example ~/wine-tux-force/bin/wine")
                    .with_validator(|input: &str| {
                        let path = expand_home(input);
                        Ok(if xtux_games::setup::wine_exists(&path) {
                            inquire::validator::Validation::Valid
                        } else {
                            inquire::validator::Validation::Invalid("not found".into())
                        })
                    })
                    .prompt(),
            )?;
            let path = expand_home(&path);
            settings.wine = Some(path.clone());
            ok(format!("Using {path}"));
            Some(path)
        }
    }
}

async fn prefix_step(
    client: &reqwest::Client,
    settings: &mut Settings,
    state: &SetupState,
    wine: &str,
) -> Option<PathBuf> {
    println!(
        "  The prefix is the Windows folder games run in. It gets DXVK and vkd3d-proton for graphics and the"
    );
    println!("  certificates some games check their servers with.");
    let path = if state.prefix_ready {
        ok(format!("{} is prepared", state.prefix.display()));
        let again = answered(
            Confirm::new("Prepare it again (updates DXVK and vkd3d-proton)?")
                .with_default(false)
                .prompt(),
        )?;
        if !again {
            settings.prefix = Some(state.prefix.clone());
            return Some(state.prefix.clone());
        }
        state.prefix.clone()
    } else {
        let default = state.prefix.to_string_lossy().into_owned();
        PathBuf::from(expand_home(&answered(
            Text::new("Where should the prefix be?")
                .with_default(&default)
                .prompt(),
        )?))
    };
    let bar = Bar::new();
    let options = xtux_games::prefix::InitOptions {
        wine: wine.to_string(),
        prefix: Some(path.to_string_lossy().into_owned()),
        from_proton: None,
        skip_graphics: false,
    };
    match xtux_games::prefix::init(client, options, &bar).await {
        Ok(prefix) => {
            bar.finish(format!("Prefix {} is ready", prefix.display()));
            settings.prefix = Some(prefix.clone());
            Some(prefix)
        }
        Err(err) => {
            bar.fail(format!("Preparing the prefix failed: {err}"));
            None
        }
    }
}

fn library_step(settings: &mut Settings) -> Option<()> {
    for dir in xtux_games::settings::detect_windows_libraries() {
        if !settings.library_dirs.contains(&dir) {
            settings.library_dirs.push(dir);
        }
    }
    for dir in &settings.library_dirs {
        let games = xtux_games::library::scan(std::slice::from_ref(dir)).len();
        ok(format!("{} ({games} games)", dir.display()));
    }
    while answered(
        Confirm::new("Add another folder with games (for example an XboxGames folder)?")
            .with_default(false)
            .prompt(),
    )? {
        let dir = answered(Text::new("Folder:").prompt())?;
        let dir = PathBuf::from(expand_home(&dir));
        if dir.is_dir() {
            ok(format!("{} added", dir.display()));
            settings.library_dirs.push(dir);
        } else {
            missing(format!("{} is not a folder", dir.display()));
        }
    }
    Some(())
}

fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home)
            .join(rest)
            .to_string_lossy()
            .into_owned(),
        _ => path.trim().to_string(),
    }
}
