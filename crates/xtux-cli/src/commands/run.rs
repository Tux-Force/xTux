use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use xodus::tokens::TokenManager;
use xtux_games::run::{LaunchOptions, launch};
use xtux_games::settings::Settings;
use xtux_protocol::{Client, Event, Request, Response};

use super::{Print, direct_mode};

/// Launch options from the command line, the game's own settings and the general settings, in that order.
pub fn prepare(
    source: String,
    wine: Option<String>,
    exe: Option<String>,
    market: Option<String>,
    args: Vec<String>,
) -> Result<LaunchOptions, String> {
    let settings = Settings::load().unwrap_or_default();
    let source = super::list::resolve(&source)?;
    if let Some(anticheat) = xtux_games::library::inspect(&source).and_then(|g| g.anticheat) {
        if anticheat.blocks {
            eprintln!(
                "Warning: this game uses the kernel anti-cheat {}, which Wine cannot load; it runs on Windows only.",
                anticheat.name
            );
        } else {
            eprintln!(
                "Note: this game uses {}, which works on Linux only if the developer allows it.",
                anticheat.name
            );
        }
    }
    let game = settings.game(&source);
    let effective = settings.effective(&source);
    let wine = wine
        .or(effective.wine)
        .ok_or("No Wine binary given and none set in the settings")?;
    let mut env: Vec<(String, String)> = effective.env.into_iter().collect();
    // A prefix of the game wins over WINEPREFIX, which wins over the general setting.
    let prefix = game.prefix.or_else(|| {
        std::env::var_os("WINEPREFIX")
            .is_none()
            .then_some(settings.prefix.clone())
            .flatten()
    });
    if let Some(prefix) = prefix {
        env.push(("WINEPREFIX".into(), prefix.to_string_lossy().into_owned()));
    }
    Ok(LaunchOptions {
        source,
        wine,
        exe: exe.or(effective.exe),
        market: market.or(settings.market),
        env,
        args: if args.is_empty() {
            effective.args
        } else {
            args
        },
        log: None,
    })
}

pub async fn run(
    client: &reqwest::Client,
    tokens: &TokenManager,
    options: LaunchOptions,
) -> ExitCode {
    // Only a warning: the game starts anyway.
    if let Some(warning) = xtux_games::gpu::status().await.low_memory_warning() {
        eprintln!("Warning: {warning}");
    }
    if direct_mode() {
        return run_direct(client, tokens, options).await;
    }

    // The daemon has its own working directory and environment: send absolute paths and ours.
    let source = match std::fs::canonicalize(&options.source) {
        Ok(path) => path,
        Err(err) => {
            eprintln!("Failed to resolve {:?}: {err}", options.source);
            return ExitCode::FAILURE;
        }
    };
    let source_dir = source.clone();
    let request = Request::Launch {
        source,
        wine: absolute_command(&options.wine),
        exe: options.exe,
        market: options.market,
        env: std::env::vars().chain(options.env).collect(),
        args: options.args,
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
        Ok(Response::GameExited { code, crash }) => {
            if let Some(crash) = crash {
                eprintln!("The game crashed: {}", crash.reason);
                eprintln!("{}", crash.hint);
                if let Some(log) = xtux_games::logs::game_log(&source_dir) {
                    eprintln!("Log: {}", log.display());
                }
            }
            ExitCode::from(code.map(|c| c as u8).unwrap_or(0))
        }
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
    options: LaunchOptions,
) -> ExitCode {
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
