//! `xtux-cli downloads`: the download queue of xtux-service.

use std::path::PathBuf;
use std::process::ExitCode;

use xtux_protocol::{Client, DownloadCommand, DownloadItem, Request, Response};

async fn send(request: Request) -> Result<Response, String> {
    let mut daemon = Client::connect_or_spawn()
        .await
        .map_err(|e| e.to_string())?;
    daemon
        .request(request, |_| {})
        .await
        .map_err(|e| e.to_string())
}

fn print(items: &[DownloadItem]) {
    if items.is_empty() {
        println!("The download queue is empty.");
    }
    for item in items {
        let amount = match item.total {
            Some(total) if total > 0 => format!(
                "{:>5.1}% of {:.1} GB",
                item.done as f64 * 100.0 / total as f64,
                total as f64 / (1u64 << 30) as f64
            ),
            _ => String::new(),
        };
        let amount = if item.speed > 0 {
            format!(
                "{amount} {:.1} MB/s",
                item.speed as f64 / (1u64 << 20) as f64
            )
        } else {
            amount
        };
        println!(
            "{:>3}  {:<8} {:<40} {amount:<20} {}",
            item.id,
            item.state,
            item.name.chars().take(40).collect::<String>(),
            item.message
        );
    }
}

/// `action`: list, add <Store ID> [library], pause/resume/remove <id>, clear.
pub async fn run(action: Vec<String>) -> ExitCode {
    let words: Vec<&str> = action.iter().map(String::as_str).collect();
    let command = match words.as_slice() {
        [] | ["list"] => DownloadCommand::List,
        ["clear"] => DownloadCommand::ClearDone,
        [verb @ ("pause" | "resume" | "remove" | "up" | "down"), id] => {
            let Ok(id) = id.parse() else {
                eprintln!("{id:?} is not a queue number (see xtux-cli downloads)");
                return ExitCode::FAILURE;
            };
            match *verb {
                "pause" => DownloadCommand::Pause { id },
                "resume" => DownloadCommand::Resume { id },
                "remove" => DownloadCommand::Remove { id },
                "up" => DownloadCommand::Move { id, up: true },
                _ => DownloadCommand::Move { id, up: false },
            }
        }
        ["add", store_id, rest @ ..] => {
            // Name and content ID come from the catalog.
            let titles = match send(Request::Catalog {
                market: None,
                refresh: false,
            })
            .await
            {
                // Games and add-ons alike: an add-on installs into a folder of its own.
                Ok(Response::Catalog { titles, addons }) => {
                    titles.into_iter().chain(addons).collect::<Vec<_>>()
                }
                Ok(other) => {
                    eprintln!("unexpected answer: {other:?}");
                    return ExitCode::FAILURE;
                }
                Err(err) => {
                    eprintln!("{err}");
                    return ExitCode::FAILURE;
                }
            };
            let Some(title) = titles
                .iter()
                .find(|t| t.product_id.eq_ignore_ascii_case(store_id))
            else {
                eprintln!("{store_id} is not in the store catalog (Game Pass or owned)");
                return ExitCode::FAILURE;
            };
            let (true, Some(content_id)) = (title.installable, title.content_id.clone()) else {
                eprintln!("{} has no GDK package xTux can install", title.name);
                return ExitCode::FAILURE;
            };
            let library = match rest.first() {
                Some(dir) => PathBuf::from(dir),
                None => match xtux_games::settings::Settings::load()
                    .ok()
                    .and_then(|s| s.library_dirs.into_iter().next())
                {
                    Some(dir) => dir,
                    None => {
                        eprintln!("No library folder in the settings");
                        return ExitCode::FAILURE;
                    }
                },
            };
            DownloadCommand::Add {
                product_id: title.product_id.clone(),
                name: title.name.clone(),
                content_id,
                library,
            }
        }
        _ => {
            eprintln!(
                "usage: xtux-cli downloads [list | add <Store ID> [library] | pause|resume|remove|up|down <n> | clear]"
            );
            return ExitCode::FAILURE;
        }
    };
    match send(Request::Downloads(command)).await {
        Ok(Response::Downloads { items, .. }) => {
            print(&items);
            ExitCode::SUCCESS
        }
        Ok(other) => {
            eprintln!("unexpected answer: {other:?}");
            ExitCode::FAILURE
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}
