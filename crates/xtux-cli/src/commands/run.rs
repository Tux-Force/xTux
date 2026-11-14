use std::path::PathBuf;
use std::process::ExitCode;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use xodus::tokens::TokenManager;
use xtux_games::run::{LaunchOptions, launch};

use super::Print;

pub async fn run(
    client: &reqwest::Client,
    tokens: &TokenManager,
    source: String,
    wine: String,
    exe: Option<String>,
    market: Option<String>,
) -> ExitCode {
    let options = LaunchOptions {
        source: PathBuf::from(source),
        wine,
        exe,
        market,
    };
    let game = match launch(client, tokens, options, &Print).await {
        Ok(game) => game,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };

    // Ctrl+C asks the game to quit like on Windows instead of killing it.
    let pid = game.pid;
    if let Err(err) = ctrlc::set_handler(move || {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGINT);
    }) {
        eprintln!("could not install the Ctrl+C handler: {err}");
    }

    match game.wait().await {
        Ok(status) => ExitCode::from(status.code().map(|c| c as u8).unwrap_or(0)),
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}
