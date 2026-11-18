//! Works through the download queue (xtux_games::queue): one install at a time, in the background, also while no
//! client is connected. The queue is saved, so a restarted service carries on.

use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::Notify;
use xodus::tokens::TokenManager;
use xtux_games::queue::{Queue, State};
use xtux_games::settings::Settings;
use xtux_games::{CancellationToken, Progress};
use xtux_protocol::{DownloadCommand, DownloadItem, RateSample};

/// Seconds of rate history kept.
const HISTORY: usize = 300;

/// Bytes this process read from and wrote to storage so far (`/proc/self/io`).
fn disk_io() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/self/io").ok()?;
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.trim().parse::<u64>().ok())
    };
    Some((field("read_bytes:")?, field("write_bytes:")?))
}

/// The rate history, oldest first.
pub fn history() -> Vec<RateSample> {
    get()
        .and_then(|d| d.history.lock().ok().map(|h| h.iter().copied().collect()))
        .unwrap_or_default()
}

/// Samples network and disk rates every second while an install runs.
async fn sampler(downloads: Arc<Downloads>) {
    let mut last_disk = disk_io();
    let mut last_done: Option<(u64, u64)> = None;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let running = downloads
            .current
            .lock()
            .ok()
            .and_then(|c| c.as_ref().map(|r| r.id));
        let disk = disk_io();
        // Idle seconds are recorded as zero, so the graph keeps a real time axis.
        let network = match running {
            Some(id) => {
                let done = downloads
                    .queue
                    .lock()
                    .ok()
                    .and_then(|q| q.items.iter().find(|i| i.id == id).map(|i| i.done));
                let network = match (last_done, done) {
                    (Some((last_id, before)), Some(now)) if last_id == id => {
                        now.saturating_sub(before)
                    }
                    _ => 0,
                };
                last_done = done.map(|d| (id, d));
                network
            }
            None => {
                last_done = None;
                0
            }
        };
        let (disk_read, disk_write) = match (last_disk, disk) {
            (Some((r0, w0)), Some((r1, w1))) => (r1.saturating_sub(r0), w1.saturating_sub(w0)),
            _ => (0, 0),
        };
        last_disk = disk;
        if let Ok(mut history) = downloads.history.lock() {
            history.push_back(RateSample {
                network,
                disk_write,
                disk_read,
            });
            while history.len() > HISTORY {
                history.pop_front();
            }
        }
    }
}

use crate::activity::Activity;

pub struct Downloads {
    /// Network and disk rates while installs run, newest last.
    history: Mutex<std::collections::VecDeque<RateSample>>,
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
        history: Mutex::new(std::collections::VecDeque::new()),
        queue: Mutex::new(Queue::load()),
        wake: Notify::new(),
        current: Mutex::new(None),
    });
    if DOWNLOADS.set(downloads.clone()).is_err() {
        return;
    }
    tokio::spawn(sampler(downloads.clone()));
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
    /// Last speed sample: when, and how much was done then.
    sample: Mutex<Option<(std::time::Instant, u64)>>,
    /// Byte progress counts only from the download step on; before it the bar keeps what the last run reached,
    /// instead of jumping around with the metadata.
    downloading: std::sync::atomic::AtomicBool,
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
        if text == xtux_games::install::DOWNLOAD_STEP {
            self.downloading
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.update(|item| item.message = text.to_string());
    }

    fn bytes(&self, done: u64, total: Option<u64>) {
        if !self.downloading.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        // Speed from samples at least a second apart, smoothed so it does not jump.
        let mut speed = None;
        if let Ok(mut sample) = self.sample.lock() {
            let now = std::time::Instant::now();
            match *sample {
                Some((when, then)) if now.duration_since(when).as_secs_f64() >= 1.0 => {
                    let seconds = now.duration_since(when).as_secs_f64();
                    speed = Some((done.saturating_sub(then) as f64 / seconds) as u64);
                    *sample = Some((now, done));
                }
                None => *sample = Some((now, done)),
                _ => {}
            }
        }
        self.update(|item| {
            item.done = done;
            item.total = total;
            if let Some(speed) = speed {
                item.speed = if item.speed == 0 {
                    speed
                } else {
                    (item.speed * 7 + speed * 3) / 10
                };
            }
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
            entry.speed = 0;
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
            sample: Mutex::new(None),
            downloading: std::sync::atomic::AtomicBool::new(false),
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
                        entry.message = if entry.update { "Updated" } else { "Installed" }.into();
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
            update: i.update,
            state: match i.state {
                State::Queued => "queued",
                State::Running => "running",
                State::Paused => "paused",
                State::Pausing => "pausing",
                State::Removing => "removing",
                State::Failed => "failed",
                State::Done => "done",
            }
            .into(),
            done: i.done,
            total: i.total,
            message: i.message.clone(),
            speed: if i.state == State::Running {
                i.speed
            } else {
                0
            },
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
            DownloadCommand::Update { path } => {
                queue.update(&path, &library_dirs())?;
                queue.save();
                downloads.wake.notify_one();
            }
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
                if cancel_current(id, false) {
                    // Shown until the install has stopped writing.
                    if let Some(item) = queue.get_mut(id) {
                        item.state = State::Pausing;
                        item.message = "Pausing…".into();
                    }
                } else {
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
                if cancel_current(id, true) {
                    if let Some(item) = queue.get_mut(id) {
                        item.state = State::Removing;
                        item.message = "Stopping and removing…".into();
                    }
                } else {
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
