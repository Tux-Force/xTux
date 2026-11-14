pub mod clep;
pub mod download;
pub mod extract;
pub mod extract_eappx;
pub mod license;
#[cfg(unix)]
pub mod link;
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

#[cfg(unix)]
pub async fn prefix_init(client: &reqwest::Client, options: xtux_games::prefix::InitOptions) -> std::process::ExitCode {
    match xtux_games::prefix::init(client, options, &Print).await {
        Ok(_) => {
            println!("Done. `xtux-cli run` uses this prefix setup when WINEPREFIX points here.");
            std::process::ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("prefix init failed: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}
