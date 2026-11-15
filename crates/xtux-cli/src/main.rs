use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use xodus::tokens::TokenManager;

mod banner;
mod commands;
mod package;
mod webview;

#[derive(Subcommand)]
enum SubCommand {
    #[command(about = "Download msixvc or xsp files for a given game")]
    Download {
        product: String,
        #[arg(short, long)]
        market: Option<String>,
        #[arg(
            long,
            default_value_t = false,
            help = "Display download URLs instead of downloading"
        )]
        dry_run: bool,
    },
    #[command(about = "Dump CIKs for use with XvdTool")]
    License {
        #[clap(help = "Content Id of a license")]
        content_id: String,
        #[clap(help = "A path where to dump CIKs")]
        ciks: String,
        #[arg(short, long)]
        market: Option<String>,
    },
    #[command(about = "Extract locally stored msixvc file")]
    Extract {
        path: String,
        destination: String,
        #[arg(short, long)]
        market: Option<String>,
    },
    #[command(
        about = "Extract a locally stored EAppx/EMSIX package (research task, see issue #91)"
    )]
    ExtractEappx {
        path: String,
        destination: String,
        #[arg(short, long, help = "Path to a keyfile with content decryption keys")]
        key_file: Option<String>,
    },
    Login,
    Logout {
        #[arg(long, default_value_t = false, help = "Remove device license")]
        device: bool,
    },
    #[command(about = "Download and extract the game through streaming algorithm")]
    Streaming {
        source: String,
        destination: String,
        #[arg(
            long,
            default_value_t = false,
            help = "Attempt to skip downloading NTFS metadata to be faste while missing some files"
        )]
        try_skip_ntfs: bool,
        #[arg(short, long)]
        parallel: Option<usize>,
        #[arg(short, long)]
        market: Option<String>,
    },
    #[cfg(unix)]
    #[command(
        about = "Link a game installed by the Windows Xbox app so it can be run from Linux without copying"
    )]
    Link {
        /// Game folder of the Xbox app, for example XboxGames/Atomfall
        game_dir: String,
        /// Folder for the links, defaults to ~/.local/share/xtux/links/<game>
        dest: Option<String>,
    },
    #[cfg(unix)]
    #[command(about = "Run a game with the Tux-Force Wine build")]
    Run {
        /// Game name (see `xtux-cli list`) or game folder
        #[arg(add = clap_complete::ArgValueCompleter::new(commands::list::complete_game))]
        source: String,
        /// Wine binary, defaults to the one in the settings
        wine: Option<String>,
        /// Executable to start, package relative; overrides the game settings
        #[arg(short, long)]
        exe: Option<String>,
        #[arg(short, long)]
        market: Option<String>,
        /// Arguments for the game, after `--`; replace those of the game settings
        #[arg(last = true)]
        args: Vec<String>,
    },
    #[cfg(unix)]
    #[command(about = "List the installed games of the library folders")]
    List {
        /// Print only the game folders
        #[arg(long, default_value_t = false)]
        paths: bool,
    },
    #[cfg(unix)]
    #[command(about = "Move an install from the old Xodus layout into the Windows Xbox app layout")]
    Migrate {
        /// Game folder with a .xodus-streaming.msixvc
        game_dir: String,
    },
    #[cfg(unix)]
    #[command(
        about = "Prepare a Wine prefix for GDK games (DXVK, vkd3d-proton, root certificates)"
    )]
    Prefix {
        #[command(subcommand)]
        action: PrefixAction,
    },
    #[command(about = "Generate or decrypt base64-encoded CLEP challenge data")]
    Clep {
        #[command(subcommand)]
        action: ClepAction,
    },
    #[command(about = "Decode SPLicenseBlock")]
    SpLicense {
        block: String,
    },
}

#[cfg(unix)]
#[derive(Subcommand)]
enum PrefixAction {
    #[command(about = "Create or update a prefix: DXVK, vkd3d-proton, Microsoft root certificates")]
    Init {
        /// Wine binary to use, for example the Tux-Force build
        wine: String,
        /// Prefix path, defaults to WINEPREFIX or ~/Games/xtux-prefix
        #[arg(long)]
        prefix: Option<String>,
        /// Copy DXVK and vkd3d-proton from a local Proton build instead of downloading them
        #[arg(long)]
        from_proton: Option<String>,
        /// Leave Wine's own Direct3D DLLs in place
        #[arg(long, default_value_t = false)]
        skip_graphics: bool,
    },
}

#[derive(Subcommand)]
enum ClepAction {
    #[command(
        about = "Generate a base64-encoded CLEP challenge (V2 and V4) from SMBIOS/disk serial data"
    )]
    Generate {
        #[arg(
            long,
            help = "Base64-encoded SMBIOS data (up to 256 bytes, zero-padded)"
        )]
        smbios: Option<String>,
        #[arg(
            long,
            help = "Base64-encoded disk serial (up to 64 bytes, zero-padded)"
        )]
        disk_serial: Option<String>,
    },
    #[command(about = "Decrypt a base64-encoded CLEP challenge back into its plaintext fields")]
    Decrypt {
        #[clap(help = "Base64-encoded, obfuscated CLEP challenge data (2048 bytes)")]
        data: String,
    },
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct CliArgs {
    #[command(subcommand)]
    command: SubCommand,
}

/// Commands that only read local files and never need the keychain.
fn local_only(command: &SubCommand) -> bool {
    #[cfg(unix)]
    if matches!(
        command,
        SubCommand::List { .. } | SubCommand::Migrate { .. }
    ) {
        return true;
    }
    let _ = command;
    false
}

#[tokio::main]
async fn main() -> ExitCode {
    // Answers shell completion requests (`source <(COMPLETE=zsh xtux-cli)`) and exits.
    clap_complete::CompleteEnv::with_factory(CliArgs::command).complete();

    // Show library progress (game bridge, links) unless XODUS_LOG chooses something else.
    let filter = tracing_subscriber::EnvFilter::try_from_env("XODUS_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,xtux_games=info"));
    let registry =
        tracing_subscriber::registry().with(tracing_subscriber::fmt::layer().with_filter(filter));

    #[cfg(feature = "tokio_console")]
    {
        use tracing::level_filters::LevelFilter;
        use tracing_subscriber::filter::Targets;

        let console_filter = Targets::new()
            .with_target("tokio", LevelFilter::TRACE)
            .with_target("runtime", LevelFilter::TRACE);
        let console_layer = console_subscriber::spawn().with_filter(console_filter);
        registry.with(console_layer).init();
    }
    #[cfg(not(feature = "tokio_console"))]
    {
        registry.init();
    }
    let client = reqwest::ClientBuilder::new()
        .user_agent(format!("xtux-cli/{}", env!("CARGO_PKG_VERSION")))
        .connection_verbose(true)
        .build()
        .unwrap();
    let args = CliArgs::parse();

    xodus::secrets::init_secrets().expect("Unable to initialize credentials");
    let tokens = TokenManager::with_keychain_and_memory();

    // Clep/SpLicense are pure local data transforms and Logout only removes
    // stored credentials - none of them need a device identity, so don't
    // force provisioning (network + keychain access) just to run them. This
    // matters in practice: on a session with no usable secret-service
    // keychain, provisioning fails outright, which previously meant even
    // these fully offline commands were unusable.
    let needs_device_credentials = !matches!(
        args.command,
        SubCommand::Clep { .. } | SubCommand::SpLicense { .. } | SubCommand::Logout { .. }
    ) && !local_only(&args.command);
    if needs_device_credentials {
        xodus::tokens::device::ensure_device_credentials(&client, &tokens).await;
    }

    // The mascot greets long running commands.
    #[cfg(unix)]
    if matches!(
        args.command,
        SubCommand::Login
            | SubCommand::Streaming { .. }
            | SubCommand::Prefix { .. }
            | SubCommand::Run { .. }
    ) {
        banner::show();
    }
    let code = match args.command {
        SubCommand::Download {
            product,
            market,
            dry_run,
        } => commands::download::run(&client, &tokens, product, market, dry_run).await,
        SubCommand::License {
            content_id,
            market,
            ciks,
        } => {
            commands::license::run(
                &client,
                &tokens,
                content_id,
                market.unwrap_or("neutral".to_string()),
                ciks,
            )
            .await
        }
        SubCommand::Login => commands::login::run(&client, &tokens).await,
        SubCommand::Logout { device } => commands::logout::run(&tokens, device).await,
        SubCommand::Extract {
            path,
            destination,
            market,
        } => {
            commands::extract::run(
                &client,
                &tokens,
                path,
                destination,
                market.unwrap_or("neutral".to_string()),
            )
            .await
        }
        SubCommand::ExtractEappx {
            path,
            destination,
            key_file,
        } => commands::extract_eappx::run(path, destination, key_file).await,
        SubCommand::Streaming {
            source,
            destination,
            try_skip_ntfs,
            market,
            parallel,
        } => {
            commands::streaming::run(
                &client,
                &tokens,
                source,
                destination,
                try_skip_ntfs,
                parallel,
                market,
            )
            .await
        }
        #[cfg(unix)]
        SubCommand::Link { game_dir, dest } => commands::link::run(game_dir, dest),
        #[cfg(unix)]
        SubCommand::List { paths } => commands::list::run(paths),
        #[cfg(unix)]
        SubCommand::Migrate { game_dir } => {
            match xtux_games::layout::migrate_xodus(std::path::Path::new(&game_dir)).await {
                Ok(package) => {
                    println!(
                        "Moved to the Windows layout; package header: {}",
                        package.display()
                    );
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("{err}");
                    ExitCode::FAILURE
                }
            }
        }
        #[cfg(unix)]
        SubCommand::Prefix {
            action:
                PrefixAction::Init {
                    wine,
                    prefix,
                    from_proton,
                    skip_graphics,
                },
        } => {
            commands::prefix_init(
                &client,
                xtux_games::prefix::InitOptions {
                    wine,
                    prefix,
                    from_proton,
                    skip_graphics,
                },
            )
            .await
        }
        #[cfg(unix)]
        SubCommand::Run {
            source,
            wine,
            exe,
            market,
            args,
        } => {
            // Windows Xbox app installs and xTux installs are read in place (xtux_games::layout).
            match commands::run::prepare(source, wine, exe, market, args) {
                Ok(options) => commands::run::run(&client, &tokens, options).await,
                Err(err) => {
                    eprintln!("{err}");
                    ExitCode::FAILURE
                }
            }
        }
        SubCommand::Clep { action } => match action {
            ClepAction::Generate {
                smbios,
                disk_serial,
            } => commands::clep::generate(smbios, disk_serial),
            ClepAction::Decrypt { data } => commands::clep::decrypt(data),
        },
        SubCommand::SpLicense { block } => commands::splicense::run(block),
    };

    xodus::secrets::destroy_secrets();

    code
}
