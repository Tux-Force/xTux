pub mod clep;
pub mod download;
#[cfg(unix)]
pub mod downloads;
pub mod extract;
pub mod extract_eappx;
pub mod license;
#[cfg(unix)]
pub mod link;
pub mod list;
pub mod login;
pub mod logout;
#[cfg(unix)]
pub mod run;
pub mod splicense;
pub mod streaming;

/// Prints library progress messages on the terminal.
pub struct Print;

impl xtux_games::Progress for Print {
    fn message(&self, text: &str) {
        println!("{text}");
    }
}

/// `XTUX_DIRECT=1` runs commands in this process instead of through xtux-service.
pub fn direct_mode() -> bool {
    std::env::var("XTUX_DIRECT").is_ok_and(|v| v == "1")
}

#[cfg(unix)]
pub async fn prefix_init(
    client: &reqwest::Client,
    options: xtux_games::prefix::InitOptions,
) -> std::process::ExitCode {
    use std::process::ExitCode;

    let result = if direct_mode() {
        xtux_games::prefix::init(client, options, &Print)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    } else {
        prefix_init_daemon(options).await
    };
    match result {
        Ok(()) => {
            println!("Done. `xtux-cli run` uses this prefix setup when WINEPREFIX points here.");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("prefix init failed: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Lets the daemon prepare the prefix; paths are resolved here, where WINEPREFIX and the working directory apply.
#[cfg(unix)]
async fn prefix_init_daemon(options: xtux_games::prefix::InitOptions) -> Result<(), String> {
    let prefix = xtux_games::prefix::resolve_prefix(options.prefix.as_deref())
        .ok_or("cannot determine the prefix, pass --prefix")?;
    let absolute = |p: &str| {
        std::fs::canonicalize(p)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| p.to_string())
    };
    let request = xtux_protocol::Request::PrefixInit {
        wine: if options.wine.contains('/') {
            absolute(&options.wine)
        } else {
            options.wine
        },
        prefix: Some(
            std::path::absolute(&prefix)
                .unwrap_or(prefix)
                .to_string_lossy()
                .into_owned(),
        ),
        from_proton: options.from_proton.as_deref().map(absolute),
        skip_graphics: options.skip_graphics,
    };
    let mut daemon = xtux_protocol::Client::connect_or_spawn()
        .await
        .map_err(|e| format!("{e} (set XTUX_DIRECT=1 to run without the daemon)"))?;
    daemon
        .request(request, |event| {
            if let xtux_protocol::Event::Message { text } = event {
                println!("{text}");
            }
        })
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}
