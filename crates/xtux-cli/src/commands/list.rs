//! `xtux-cli list` and game lookup by name for `run` (with shell completion).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap_complete::CompletionCandidate;
use clap_complete::engine::{PathCompleter, ValueCompleter};
use xtux_games::library::{self, Game, GameKind};
use xtux_games::settings::Settings;

pub(crate) fn games() -> Vec<Game> {
    let settings = Settings::load().unwrap_or_default();
    library::scan(&settings.library_dirs)
}

fn folder_name(game: &Game) -> String {
    game.path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Prints the games of the configured library folders.
pub fn run(paths: bool) -> ExitCode {
    let games = games();
    if games.is_empty() {
        let file = xtux_games::settings::path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        println!("No games found. Library folders are set in {file}.");
        return ExitCode::SUCCESS;
    }
    if paths {
        for game in &games {
            println!("{}", game.path.display());
        }
        return ExitCode::SUCCESS;
    }
    // The copy xtux-service downloaded; the list does not go online.
    let reports = Settings::load()
        .unwrap_or_default()
        .ui
        .anticheat_list
        .then(xtux_games::awacy::Database::cached)
        .flatten();
    let width = games
        .iter()
        .map(|g| g.name.chars().count())
        .max()
        .unwrap_or(0);
    for game in &games {
        let kind = match game.kind {
            GameKind::Installed => "xTux",
            GameKind::Windows => "Xbox app",
        };
        let mut license = String::new();
        if xtux_games::license::problems::of(&game.path).is_some() {
            license.push_str("  (license check failed)");
        }
        if let Some(anticheat) = &game.anticheat {
            license.push_str(&if anticheat.blocks {
                format!("  (Windows only: kernel anti-cheat {})", anticheat.name)
            } else {
                format!("  (anti-cheat {}, may not run)", anticheat.name)
            });
        }
        println!(
            "{:<width$}  {:<8}  {:<8}  {}{license}",
            game.name,
            kind,
            game.title_id.as_deref().unwrap_or("-"),
            game.path.display(),
        );
        if let Some(report) = reports.as_ref().and_then(|db| db.find(&game.name)) {
            println!("{:<width$}  AreWeAntiCheatYet: {}", "", report.summary());
        }
    }
    ExitCode::SUCCESS
}

/// A game folder from a path or a game name (display name, else folder name, case-insensitive).
pub fn resolve(source: &str) -> Result<PathBuf, String> {
    if Path::new(source).is_dir() {
        return Ok(PathBuf::from(source));
    }
    let games = games();
    let mut matches: Vec<&Game> = games
        .iter()
        .filter(|g| g.name.eq_ignore_ascii_case(source))
        .collect();
    if matches.is_empty() {
        matches = games
            .iter()
            .filter(|g| folder_name(g).eq_ignore_ascii_case(source))
            .collect();
    }
    match matches.as_slice() {
        [game] => Ok(game.path.clone()),
        [] => Err(format!(
            "{source:?} is neither a game folder nor an installed game (see xtux-cli list)"
        )),
        several => Err(format!(
            "{source:?} is installed more than once, run one by its folder:\n{}",
            several
                .iter()
                .map(|g| format!("  {}", g.path.display()))
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
}

/// Shell completion for `run`: names of installed games, then folders.
pub fn complete_game(current: &OsStr) -> Vec<CompletionCandidate> {
    let typed = current;
    let current = current.to_string_lossy().to_lowercase();
    let games = games();
    let mut candidates: Vec<CompletionCandidate> = games
        .iter()
        .filter(|g| g.name.to_lowercase().starts_with(&current))
        .map(|g| {
            // A name installed twice is ambiguous: complete to the folder instead.
            if games.iter().filter(|o| o.name == g.name).count() > 1 {
                return CompletionCandidate::new(g.path.as_os_str())
                    .help(Some(g.name.clone().into()));
            }
            CompletionCandidate::new(g.name.clone()).help(Some(g.path.display().to_string().into()))
        })
        .collect();
    // Paths still work, so offer them too when the input looks like one.
    if current.contains('/') || current.starts_with('.') {
        candidates.extend(PathCompleter::dir().complete(typed));
    }
    candidates
}
