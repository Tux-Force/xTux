//! `xtux-cli streaming`: terminal front end for [`xtux_games::install`].

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Mutex;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use uuid::Uuid;
use xodus::tokens::TokenManager;
use xtux_games::CancellationToken;
use xtux_games::install::{self, InstallOptions, Source};

use crate::package::get_content_id;

const TEMPLATE: &str =
    "{msg:30!} {bytes:>12}/{total_bytes:>12} {bytes_per_sec:>12} [{bar:40.cyan/blue}] {percent:>3}%";

pub async fn run(
    client: &reqwest::Client,
    tokens: &TokenManager,
    source: String,
    destination: String,
    try_skip_ntfs: bool,
    parallel: Option<usize>,
    market: Option<String>,
) -> ExitCode {
    let source = match Source::parse(&source) {
        // A product id (for example 9NXN8GF8N9ZQ) is resolved here, the user may have to pick an edition.
        Source::ContentId(id) if Uuid::try_parse(&id).is_err() => {
            match get_content_id(client, id, market.clone()).await {
                Ok(content_id) => Source::ContentId(content_id),
                Err(err) => {
                    eprintln!("{err}");
                    return ExitCode::FAILURE;
                }
            }
        }
        source => source,
    };

    let mut options = InstallOptions::new(destination);
    options.try_skip_ntfs = try_skip_ntfs;
    options.parallel = parallel.unwrap_or(4);
    options.market = market;

    let bars = Bars::new();
    let result = install::install(client, tokens, source, &options, &bars, &CancellationToken::new()).await;
    bars.total.abandon();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn style() -> ProgressStyle {
    ProgressStyle::with_template(TEMPLATE)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("#>-")
}

/// One overall bar plus one bar per file that is being downloaded.
struct Bars {
    multi: MultiProgress,
    total: ProgressBar,
    files: Mutex<HashMap<String, ProgressBar>>,
}

impl Bars {
    fn new() -> Self {
        let multi = MultiProgress::new();
        let total = multi.add(ProgressBar::new(0).with_style(style()));
        Self {
            multi,
            total,
            files: Mutex::new(HashMap::new()),
        }
    }
}

/// Shortens a path to the 30 columns of the message, keeping its end.
fn shown(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() > 30 {
        format!("...{}", chars[chars.len() - 27..].iter().collect::<String>())
    } else {
        name.to_owned()
    }
}

impl xtux_games::Progress for Bars {
    fn message(&self, text: &str) {
        self.total.set_message(text.to_owned());
    }

    fn bytes(&self, done: u64, total: Option<u64>) {
        if let Some(total) = total {
            self.total.set_length(total);
        }
        self.total.set_position(done);
    }

    fn file(&self, name: &str, done: u64, total: u64) {
        let Ok(mut files) = self.files.lock() else {
            return;
        };
        let bar = files.entry(name.to_owned()).or_insert_with(|| {
            let bar = self.multi.add(ProgressBar::new(total).with_style(style()));
            bar.set_message(shown(name));
            bar
        });
        bar.set_position(done);
    }

    fn file_finished(&self, name: &str) {
        if let Ok(mut files) = self.files.lock()
            && let Some(bar) = files.remove(name)
        {
            bar.finish_and_clear();
        }
    }
}
