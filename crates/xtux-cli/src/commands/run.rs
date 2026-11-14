use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use xodus::tokens::TokenManager;
use xtux_games::run::{LaunchOptions, launch};
use xtux_protocol::{Client, Event, Request, Response};

use super::{Print, direct_mode};

pub async fn run(
    client: &reqwest::Client,
    tokens: &TokenManager,
    source: String,
    wine: String,
    exe: Option<String>,
    market: Option<String>,
    extra_env: Vec<(String, String)>,
) -> ExitCode {
    if direct_mode() {
        return run_direct(client, tokens, source, wine, exe, market, extra_env).await;
    }

    // The daemon has its own working directory and environment: send absolute paths and ours.
    let source = match std::fs::canonicalize(&source) {
        Ok(path) => path,
        Err(err) => {
            eprintln!("Failed to resolve {source:?}: {err}");
            return ExitCode::FAILURE;
        }
    };
    let wine = absolute_command(&wine);
    let request = Request::Launch {
        source,
        wine,
        exe,
        market,
        env: std::env::vars().chain(extra_env).collect(),
    };

    let mut daemon = match Client::connect_or_spawn().await {
        Ok(daemon) => daemon,
        Err(err) => {
            eprintln!("{err} (set XTUX_DIRECT=1 to run without the daemon)");
            return ExitCode::FAILURE;
        }
    };

    // Ctrl+C asks the game to quit like on Windows instead of killing it.
    let pid = Arc::new(AtomicU32::new(0));
    let handler_pid = pid.clone();
    if let Err(err) = ctrlc::set_handler(move || {
        let pid = handler_pid.load(Ordering::SeqCst);
        if pid > 0 {
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGINT);
        }
    }) {
        eprintln!("could not install the Ctrl+C handler: {err}");
    }

    let result = daemon
        .request(request, |event| match event {
            Event::Message { text } => println!("{text}"),
            Event::GameStarted { pid: game } => pid.store(game, Ordering::SeqCst),
            Event::Bytes { .. } | Event::File { .. } | Event::FileFinished { .. } => {}
        })
        .await;
    match result {
        Ok(Response::GameExited { code }) => ExitCode::from(code.map(|c| c as u8).unwrap_or(0)),
        Ok(other) => {
            eprintln!("unexpected answer from xtux-service: {other:?}");
            ExitCode::FAILURE
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

/// Paths relative to our working directory would be wrong for the daemon.
fn absolute_command(command: &str) -> String {
    if command.contains('/') {
        std::fs::canonicalize(command)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| command.to_string())
    } else {
        command.to_string()
    }
}

/// Runs the game in this process, without the daemon (`XTUX_DIRECT=1`).
async fn run_direct(
    client: &reqwest::Client,
    tokens: &TokenManager,
    source: String,
    wine: String,
    exe: Option<String>,
    market: Option<String>,
    extra_env: Vec<(String, String)>,
) -> ExitCode {
    let options = LaunchOptions {
        source: PathBuf::from(source),
        wine,
        exe,
        market,
        env: extra_env,
    };
    let game = match launch(client, tokens, options, &Print).await {
        Ok(game) => game,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };

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
