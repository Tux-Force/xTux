//! `xtux-cli download`: lets the user pick package files and downloads them with
//! [`xtux_games::download`].

use std::path::Path;
use std::process::ExitCode;

use indicatif::{ProgressBar, ProgressStyle};
use inquire::MultiSelect;
use inquire::validator::Validation;
use xodus::models::packagespc::PackageFile;
use xodus::tokens::TokenManager;
use xtux_games::CancellationToken;
use xtux_games::download::download_package_file;

use crate::package::{get_content_id, get_packages};

pub async fn run(
    client: &reqwest::Client,
    tokens: &TokenManager,
    product: String,
    market: Option<String>,
    dry_run: bool,
) -> ExitCode {
    let content_id = match get_content_id(client, product, market).await {
        Ok(content_id) => content_id,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };

    let package = match get_packages(client, tokens, content_id.clone()).await {
        Ok(package) => package,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };

    let Ok(files) = MultiSelect::new("Select files to download", package.package_files)
        .with_page_size(30)
        .with_validator(|input: &[inquire::list_option::ListOption<&PackageFile>]| {
            if !input.is_empty() {
                Ok(Validation::Valid)
            } else {
                Ok(Validation::Invalid(
                    "At least one item has to be selected".into(),
                ))
            }
        })
        .prompt()
    else {
        tracing::error!("Selection failed");
        return ExitCode::FAILURE;
    };
    println!();
    let cancel = CancellationToken::new();
    for file in files {
        if dry_run {
            let Some(first_url) = file.download_urls().into_iter().next() else {
                eprintln!("{}: the server returned no CDN for this file", file.file_name);
                return ExitCode::FAILURE;
            };
            println!("{first_url}");
            continue;
        }

        let bar = Bar(ProgressBar::new(u64::try_from(file.file_size).unwrap_or(0)).with_style(
            ProgressStyle::with_template("[{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}) ({eta})")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("#>-"),
        ));
        if let Err(err) = download_package_file(client, &file, Path::new("."), &bar, &cancel).await {
            bar.0.abandon();
            eprintln!("{}: {err}", file.file_name);
            return ExitCode::FAILURE;
        }
        bar.0.finish();
    }

    println!("ContentID: {content_id}");

    ExitCode::SUCCESS
}

/// Drives one indicatif bar from the library's byte progress.
struct Bar(ProgressBar);

impl xtux_games::Progress for Bar {
    fn message(&self, text: &str) {
        self.0.set_message(text.to_owned());
    }

    fn bytes(&self, done: u64, total: Option<u64>) {
        if let Some(total) = total {
            self.0.set_length(total);
        }
        self.0.set_position(done);
    }
}
