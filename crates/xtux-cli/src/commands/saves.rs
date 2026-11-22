//! `xtux-cli saves list|pull|push|resolve <game> [cloud|local]`: the game's saves in the Xbox cloud.

use std::process::ExitCode;

use xodus::tokens::TokenManager;
use xtux_games::cloudsave::{Prefer, Saves};
use xtux_games::settings::Settings;

pub async fn run(
    client: &reqwest::Client,
    tokens: &TokenManager,
    action: &str,
    source: &str,
    side: Option<&str>,
) -> ExitCode {
    let source = match super::list::resolve(source) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    // The same prefix as `run`: the game's own, then WINEPREFIX, then the general setting.
    let settings = Settings::load().unwrap_or_default();
    let Some(prefix) = settings
        .game(&source)
        .prefix
        .or_else(|| std::env::var_os("WINEPREFIX").map(Into::into))
        .or_else(|| settings.prefix.clone())
        .or_else(|| xtux_games::prefix::resolve_prefix(None))
    else {
        eprintln!("No Wine prefix yet: run `xtux-cli setup`");
        return ExitCode::FAILURE;
    };
    let saves = match Saves::open(client, tokens, &source, &prefix).await {
        Ok(Some(saves)) => saves,
        Ok(None) => {
            eprintln!(
                "This game has no Xbox Live identity (MSA app id and title id), so it has no cloud saves."
            );
            return ExitCode::FAILURE;
        }
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "{}: saves of Xbox user {}",
        saves.game.name, saves.cloud.xuid
    );
    let result = match action {
        "list" => saves.list().await.map(|spaces| {
            for (scid, dir) in saves.save_dirs() {
                println!("local {scid}: {}", dir.display());
            }
            for (scid, containers) in spaces {
                println!("{scid}: {} container(s)", containers.len());
                for c in containers {
                    println!(
                        "  {}  {} bytes  {}  {}",
                        c.name().unwrap_or(&c.file_name),
                        c.size,
                        c.client_file_time.as_deref().unwrap_or("-"),
                        c.display_name.as_deref().unwrap_or("")
                    );
                }
            }
        }),
        "pull" => saves.pull().await.map(|r| println!("{}", r.summary())),
        "push" => saves.push().await.map(|r| println!("{}", r.summary())),
        "resolve" => match side {
            Some("cloud") => saves
                .resolve(Prefer::Cloud)
                .await
                .map(|r| println!("{}", r.summary())),
            Some("local") => saves
                .resolve(Prefer::Local)
                .await
                .map(|r| println!("{}", r.summary())),
            _ => Err("resolve needs the side to keep: cloud or local".to_string()),
        },
        other => Err(format!(
            "unknown action {other}: use list, pull, push or resolve"
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}
