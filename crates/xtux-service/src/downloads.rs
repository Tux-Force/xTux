//! Works through the download queue (xtux_games::queue): one install at a time, in the background, also while no
//! client is connected. The queue is saved, so a restarted service carries on.

use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::Notify;
use xodus::tokens::TokenManager;
use xtux_games::queue::{Queue, State};
use xtux_games::settings::Settings;
use xtux_games::{CancellationToken, Progress};
use xtux_protocol::{DownloadCommand, DownloadItem};

use crate::activity::Activity;

pub struct Downloads {
    queue: Mutex<Queue>,
    wake: Notify,
    /// The running item, what cancels it, and whether to remove it once stopped.
    current: Mutex<Option<Running>>,
}

struct Running {
    id: u64,
    cancel: CancellationToken,
    remove_after_stop: bool,
}

static DOWNLOADS: OnceLock<Arc<Downloads>> = OnceLock::new();

/// Loads the queue and starts the worker; once per service.
pub fn start(client: reqwest::Client, tokens: Arc<TokenManager>, activity: Activity) {
    let downloads = Arc::new(Downloads {
        queue: Mutex::new(Queue::load()),
        wake: Notify::new(),
        current: Mutex::new(None),
    });
    if DOWNLOADS.set(downloads.clone()).is_err() {
        return;
    }
    tokio::spawn(worker(downloads, client, tokens, activity));
}

fn get() -> Option<&'static Arc<Downloads>> {
    DOWNLOADS.get()
}

fn library_dirs() -> Vec<std::path::PathBuf> {
    Settings::load().unwrap_or_default().library_dirs
}

/// Reports install progress into the queue item.
struct ItemProgress {
    downloads: Arc<Downloads>,
    id: u64,
}

impl ItemProgress {
    fn update(&self, change: impl FnOnce(&mut xtux_games::queue::Item)) {
        if let Ok(mut queue) = self.downloads.queue.lock() {
            if let Some(item) = queue.get_mut(self.id) {
                change(item);
            }
        }
    }
}

impl Progress for ItemProgress {
    fn message(&self, text: &str) {
        self.update(|item| item.message = text.to_string());
    }

    fn bytes(&self, done: u64, total: Option<u64>) {
        self.update(|item| {
            item.done = done;
            item.total = total;
        });
    }
}

async fn worker(
    downloads: Arc<Downloads>,
    client: reqwest::Client,
    tokens: Arc<TokenManager>,
    activity: Activity,
) {
    loop {
        // Pick the next item, mark it running and make its folder in one step, so a Pause or Remove either
        // comes before (the item is not picked) or sees it running.
        let cancel = CancellationToken::new();
        let picked = downloads.queue.lock().ok().and_then(|mut queue| {
            let id = queue.next_queued()?.id;
            let entry = queue.get_mut(id)?;
            if let Err(err) = Queue::claim_folder(entry) {
                entry.state = State::Failed;
                entry.message = err.to_string();
                queue.save();
                return Some(None);
            }
            entry.state = State::Running;
            entry.message = "Starting".into();
            let item = entry.clone();
            if let Ok(mut current) = downloads.current.lock() {
                *current = Some(Running {
                    id,
                    cancel: cancel.clone(),
                    remove_after_stop: false,
                });
            }
            queue.save();
            Some(Some(item))
        });
        let item = match picked {
            Some(Some(item)) => item,
            // Failed before starting; look at the next one.
            Some(None) => continue,
            None => {
                downloads.wake.notified().await;
                continue;
            }
        };
        // Keeps the service alive while it installs.
        let _busy = activity.busy();
        tracing::info!(
            "installing {} into {}",
            item.name,
            item.destination.display()
        );

        let mut options = xtux_games::install::InstallOptions::new(item.destination.clone());
        options.market = Settings::load().ok().and_then(|s| s.market);
        let progress = ItemProgress {
            downloads: downloads.clone(),
            id: item.id,
        };
        let result = xtux_games::install::install(
            &client,
            &tokens,
            xtux_games::install::Source::ContentId(item.content_id.clone()),
            &options,
            &progress,
            &cancel,
        )
        .await;

        let running = downloads.current.lock().ok().and_then(|mut c| c.take());
        let remove = running.is_some_and(|r| r.remove_after_stop);
        let mut removed = None;
        if let Ok(mut queue) = downloads.queue.lock() {
            if remove {
                removed = queue.remove(item.id);
            } else if let Some(entry) = queue.get_mut(item.id) {
                match &result {
                    Ok(()) => {
                        entry.state = State::Done;
                        entry.message = "Installed".into();
                    }
                    // Paused: the state was set by the pause.
                    Err(xtux_games::Error::Cancelled) => {
                        entry.state = State::Paused;
                        entry.message = "Paused".into();
                    }
                    Err(err) => {
                        entry.state = State::Failed;
                        entry.message = err.to_string();
                    }
                }
            }
            queue.save();
        }
        if let Some(removed) = removed {
            delete_in_background(removed);
        }
        if let Err(err) = &result {
            tracing::warn!("install of {} stopped: {err}", item.name);
        }
    }
}

/// Deletes the folder of a removed, unfinished download off the async threads and outside the queue lock.
fn delete_in_background(item: xtux_games::queue::Item) {
    tokio::task::spawn_blocking(move || {
        if let Err(err) = xtux_games::queue::delete_partial(&item, &library_dirs()) {
            tracing::warn!(
                "could not delete the partial download {}: {err}",
                item.destination.display()
            );
        }
    });
}

fn items(queue: &Queue) -> Vec<DownloadItem> {
    queue
        .items
        .iter()
        .map(|i| DownloadItem {
            id: i.id,
            product_id: i.product_id.clone(),
            name: i.name.clone(),
            destination: i.destination.clone(),
            state: match i.state {
                State::Queued => "queued",
                State::Running => "running",
                State::Paused => "paused",
                State::Failed => "failed",
                State::Done => "done",
            }
            .into(),
            done: i.done,
            total: i.total,
            message: i.message.clone(),
        })
        .collect()
}

/// Carries out a queue command and returns the queue.
pub fn command(command: DownloadCommand) -> xtux_games::Result<Vec<DownloadItem>> {
    let downloads = get().ok_or("the download queue is not running")?;
    let cancel_current = |id: u64, remove: bool| -> bool {
        let Ok(mut current) = downloads.current.lock() else {
            return false;
        };
        match current.as_mut() {
            Some(running) if running.id == id => {
                running.remove_after_stop |= remove;
                running.cancel.cancel();
                true
            }
            _ => false,
        }
    };
    {
        let mut queue = downloads
            .queue
            .lock()
            .map_err(|_| "the download queue is broken")?;
        match command {
            DownloadCommand::List => {}
            DownloadCommand::Add {
                product_id,
                name,
                content_id,
                library,
            } => {
                queue.add(&product_id, &name, &content_id, &library, &library_dirs())?;
                queue.save();
                downloads.wake.notify_one();
            }
            DownloadCommand::Pause { id } => {
                if !cancel_current(id, false) {
                    if let Some(item) = queue.get_mut(id).filter(|i| i.state == State::Queued) {
                        item.state = State::Paused;
                        item.message = "Paused".into();
                    }
                }
                queue.save();
            }
            DownloadCommand::Resume { id } => {
                if let Some(item) = queue
                    .get_mut(id)
                    .filter(|i| matches!(i.state, State::Paused | State::Failed))
                {
                    item.state = State::Queued;
                    item.message = "Waiting".into();
                }
                queue.save();
                downloads.wake.notify_one();
            }
            DownloadCommand::Remove { id } => {
                // A running item is removed by the worker once it has stopped writing.
                if !cancel_current(id, true) {
                    if let Some(item) = queue.remove(id) {
                        delete_in_background(item);
                    }
                    queue.save();
                }
            }
            DownloadCommand::Move { id, up } => {
                queue.shift(id, up);
                queue.save();
            }
            DownloadCommand::ClearDone => {
                queue.clear_done();
                queue.save();
            }
        }
    }
    let queue = downloads
        .queue
        .lock()
        .map_err(|_| "the download queue is broken")?;
    Ok(items(&queue))
}
