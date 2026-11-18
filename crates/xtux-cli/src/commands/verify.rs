//! `xtux-cli verify`: checks the files of an installed game.

use std::path::Path;
use std::process::ExitCode;

use xodus::tokens::TokenManager;

pub async fn run(
    client: &reqwest::Client,
    tokens: &TokenManager,
    game_dir: String,
    market: Option<String>,
) -> ExitCode {
    let settings = xtux_games::settings::Settings::load().unwrap_or_default();
    let market = market.or(settings.market);
    match xtux_games::install::verify(client, tokens, Path::new(&game_dir), market, &super::Print)
        .await
    {
        Ok(report) => {
            for name in &report.damaged {
                println!("not confirmed: {name}");
            }
            println!(
                "{} files intact, {} missing, different or without page hashes (unencrypted part of the package)",
                report.intact,
                report.damaged.len()
            );
            if report.damaged.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(err) => {
            eprintln!("verify failed: {err}");
            ExitCode::FAILURE
        }
    }
}
