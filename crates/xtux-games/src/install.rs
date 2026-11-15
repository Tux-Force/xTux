//! Streaming install of a game: reads the `.msixvc` package metadata (from the CDN or a local
//! file), fetches the license and downloads and decrypts only the files that changed since the
//! last install into the destination folder.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fs2::available_space;
use futures_util::{TryStreamExt, stream};
use msixvc::streaming;
use msixvc::xvd::{SegmentFile, XvdFile};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use xodus::tokens::TokenManager;

use crate::cancel;
use crate::download::safe_join;
use crate::license::get_license;
use crate::package::get_packages;
use crate::{Error, Progress, Result};

/// Package metadata cached while the install runs.
const CACHE_FILE: &str = ".xodus-streaming-tmp.msixvc";
/// Files finished by an install that did not complete yet, one name per line; an interrupted or paused install
/// skips them when it runs again. Removed when the install completes.
const FINISHED_FILE: &str = ".xtux-finished-files";

/// Where the `.msixvc` package comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A local package file.
    File(PathBuf),
    /// A direct `http://` or `https://` URL of the package.
    Url(String),
    /// A content id; the package URL is looked up for the signed in user.
    ContentId(String),
}

impl Source {
    /// Parses the source argument of the CLI: `file://<path>`, `http(s)://<url>` or a content id.
    /// Product ids have to be resolved to a content id first (see [`crate::package::find_content_id`]).
    pub fn parse(source: &str) -> Self {
        if let Some(path) = source.strip_prefix("file://") {
            Source::File(PathBuf::from(path))
        } else if source.starts_with("http://") || source.starts_with("https://") {
            Source::Url(source.to_owned())
        } else {
            Source::ContentId(source.to_owned())
        }
    }
}

/// Options of [`install`].
#[derive(Debug, Clone)]
pub struct InstallOptions {
    /// Folder the game is installed into; created when missing.
    pub destination: PathBuf,
    /// Skip downloading the NTFS metadata when the package has segment metadata. Faster, but
    /// files only listed in the NTFS metadata are missed.
    pub try_skip_ntfs: bool,
    /// Number of files downloaded at the same time (at least 1).
    pub parallel: usize,
    /// Store market for the license, `neutral` when `None`.
    pub market: Option<String>,
}

impl InstallOptions {
    pub fn new(destination: impl Into<PathBuf>) -> Self {
        Self {
            destination: destination.into(),
            try_skip_ntfs: false,
            parallel: 4,
            market: None,
        }
    }
}

/// URL of the `.msixvc` package of `content_id`, as offered to the signed in user.
pub async fn find_msixvc_url(
    client: &reqwest::Client,
    tokens: &TokenManager,
    content_id: String,
) -> Result<String> {
    let package = get_packages(client, tokens, content_id).await?;
    let file = package
        .package_files
        .iter()
        .find(|p| p.file_name.ends_with(".msixvc"))
        .ok_or("No .msixvc file found")?;
    file.download_urls()
        .into_iter()
        .next()
        .ok_or_else(|| Error::msg(format!("The server returned no CDN for {}", file.file_name)))
}

/// Installs or updates the game from `source` into `options.destination`.
///
/// Steps are reported with [`Progress::message`], the overall byte progress with
/// [`Progress::bytes`] and each file with [`Progress::file`] and [`Progress::file_finished`]. `cancel` stops the install between
/// chunks and files and returns [`Error::Cancelled`]; files of the previous install that were
/// not touched stay valid and the next run picks up where this one stopped.
pub async fn install(
    client: &reqwest::Client,
    tokens: &TokenManager,
    source: Source,
    options: &InstallOptions,
    progress: &dyn Progress,
    cancel: &CancellationToken,
) -> Result<()> {
    progress.message("Initializing");
    let counter = Counter::new(progress);
    match source {
        Source::File(path) => {
            let file = File::open(&path)
                .await
                .map_err(|e| Error::msg(format!("could not open {}: {e}", path.display())))?;
            let len = file
                .metadata()
                .await
                .map_err(|e| {
                    Error::msg(format!(
                        "could not read metadata for {}: {e}",
                        path.display()
                    ))
                })?
                .len();
            counter.set_total(len);
            let from = Origin::File(&path);
            install_from(client, tokens, options, file, len, from, &counter, cancel).await
        }
        Source::Url(url) => install_http(client, tokens, options, &url, &counter, cancel).await,
        Source::ContentId(content_id) => {
            let url = cancel::until_cancelled(cancel, find_msixvc_url(client, tokens, content_id))
                .await?;
            install_http(client, tokens, options, &url, &counter, cancel).await
        }
    }
}

async fn install_http(
    client: &reqwest::Client,
    tokens: &TokenManager,
    options: &InstallOptions,
    url: &str,
    counter: &Counter<'_>,
    cancel: &CancellationToken,
) -> Result<()> {
    // Metadata bytes read through the CDN count towards the overall progress.
    let mut pos = 0;
    let http_file = cancel::until_cancelled(cancel, async {
        streaming::HttpRead::open(
            client.clone(),
            url,
            Some(move |c: u64, _| {
                counter.advance(c.saturating_sub(pos));
                pos = c;
            }),
        )
        .await
        .map_err(|e| Error::msg(format!("could not open the package: {e}")))
    })
    .await?;
    let len = http_file.len();
    counter.set_total(len);
    install_from(
        client,
        tokens,
        options,
        http_file,
        len,
        Origin::Http(url),
        counter,
        cancel,
    )
    .await
}

/// Where the file contents are read from once the metadata is parsed.
#[derive(Clone, Copy)]
enum Origin<'a> {
    File(&'a Path),
    Http(&'a str),
}

/// One file to download.
struct Job {
    name: String,
    target: PathBuf,
    content: SegmentFile,
    /// Where a finished file is noted for a later run.
    journal: PathBuf,
    /// A partly downloaded file may continue at its last whole page (first installs only, never updates).
    resume: bool,
}

/// Names of the files an earlier, unfinished run completed.
fn read_finished(journal: &Path) -> std::collections::HashSet<String> {
    std::fs::read_to_string(journal)
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Notes a finished file; a single appended line, so concurrent jobs do not mix their writes.
fn note_finished(journal: &Path, name: &str) {
    use std::io::Write;
    if name.contains('\n') {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal)
    {
        let _ = file.write_all(format!("{name}\n").as_bytes());
    }
}

#[allow(clippy::too_many_arguments)]
async fn install_from<Reader>(
    client: &reqwest::Client,
    tokens: &TokenManager,
    options: &InstallOptions,
    reader: Reader,
    len: u64,
    origin: Origin<'_>,
    counter: &Counter<'_>,
    cancel: &CancellationToken,
) -> Result<()>
where
    Reader: AsyncRead + Unpin,
{
    let progress = counter.progress;
    let out: &Path = &options.destination;
    std::fs::create_dir_all(out)
        .map_err(|e| Error::msg(format!("could not create {}: {e}", out.display())))?;

    // Updates of Xbox app installs would replace its own package header; not done (yet).
    let existing = crate::layout::detect(out);
    if existing.as_ref().is_some_and(|l| l.windows_app) {
        return Err(Error::msg(format!(
            "{} is an install of the Windows Xbox app; update it from Windows",
            out.display()
        )));
    }

    let cache_path = out.join(CACHE_FILE);
    let (remote_xvd, rfiles) = cancel::until_cancelled(
        cancel,
        read_remote_metadata(reader, len, &cache_path, options.try_skip_ntfs, progress),
    )
    .await?;

    // New installs use the Windows layout (Content/ plus the content id named header, see
    // layout.rs); existing installs keep theirs.
    let remote_id = remote_xvd.content_id().hyphenated().to_string();
    let (root, final_path) = match existing {
        // The Xodus header has a fixed name; a Windows layout header is named by its content id.
        Some(layout)
            if layout.package.ends_with(crate::layout::XODUS_PACKAGE)
                || layout
                    .package
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(&remote_id)) =>
        {
            (layout.root, layout.package)
        }
        Some(layout) => {
            return Err(Error::msg(format!(
                "{} holds another package ({}); pick an empty folder",
                out.display(),
                layout.package.display()
            )));
        }
        None => (
            out.join(crate::layout::CONTENT_DIR),
            out.join(
                remote_xvd
                    .content_id()
                    .hyphenated()
                    .to_string()
                    .to_uppercase(),
            ),
        ),
    };
    let lfiles = read_local_metadata(&final_path).await;

    let market = options
        .market
        .clone()
        .unwrap_or_else(|| "neutral".to_owned());
    let (key, game_splicense) = cancel::until_cancelled(
        cancel,
        get_license(client, tokens, remote_xvd.content_id().to_string(), market),
    )
    .await?;
    if game_splicense.content_keys.len() != 1 {
        return Err(Error::msg(format!(
            "unexpected number of content keys {}",
            game_splicense.content_keys.len()
        )));
    }
    let (_, content_key) = game_splicense
        .content_keys
        .into_iter()
        .next()
        .ok_or("the license has no content key")?;
    let full_key = *content_key
        .unpack(&key)
        .map_err(|e| Error::msg(format!("failed to unpack the content key: {e}")))?;

    // Files whose hashes differ from the last install, or that have no hashes at all; files an interrupted run
    // of this install finished (with their full size) are kept.
    let journal = out.join(FINISHED_FILE);
    let finished = read_finished(&journal);
    let mut jobs = Vec::new();
    let mut total_size: u64 = 0;
    let mut kept: u64 = 0;
    for (name, remote) in &rfiles {
        let unchanged = lfiles.get(name).is_some_and(|local| {
            remote.data_hashs == local.data_hashs && !remote.data_hashs.is_empty()
        });
        if unchanged {
            continue;
        }
        let target = safe_join(&root, name)?;
        if finished.contains(name)
            && std::fs::symlink_metadata(&target)
                .is_ok_and(|m| m.is_file() && m.len() == remote.length)
        {
            kept = kept.saturating_add(remote.length);
            continue;
        }
        if target == cache_path || target == final_path || target == journal {
            return Err(Error::msg(format!(
                "refusing to overwrite {name:?} from the package"
            )));
        }
        total_size = total_size
            .checked_add(remote.length)
            .ok_or("the package announces more data than fits in 64 bits")?;
        jobs.push(Job {
            name: name.clone(),
            target,
            content: SegmentFile {
                offset: remote.offset,
                length: remote.length,
                data_hashs: vec![],
                keep_encrypted: remote.keep_encrypted,
            },
            journal: journal.clone(),
            resume: lfiles.is_empty(),
        });
    }

    let available_free_space = available_space(out).map_err(|e| {
        Error::msg(format!(
            "failed to determine available space for {}: {e}",
            out.display()
        ))
    })?;
    if available_free_space < total_size {
        return Err(Error::msg(format!(
            "not enough free disk space on {}: need {} bytes, have {} bytes (files: {})",
            out.display(),
            total_size,
            available_free_space,
            total_size
        )));
    }

    progress.message("Downloading");
    // Kept files count as done, so a resumed install shows its real progress.
    counter.advance(kept);
    counter.set_remaining(total_size);

    let remote_xvd = &remote_xvd;
    stream::iter(jobs.into_iter().map(Ok::<Job, Error>))
        .try_for_each_concurrent(options.parallel.max(1), |job| async move {
            cancel::check(cancel)?;
            cancel::until_cancelled(
                cancel,
                download_job(client, remote_xvd, origin, &job, full_key, counter),
            )
            .await
        })
        .await?;

    // Only a finished install replaces the metadata of the last one, so an interrupted run
    // downloads the files it did not finish again.
    std::fs::remove_file(&journal).ok();
    std::fs::remove_file(&final_path).ok();
    std::fs::rename(&cache_path, &final_path).map_err(|e| {
        Error::msg(format!(
            "could not move {} to {}: {e}",
            cache_path.display(),
            final_path.display()
        ))
    })?;
    Ok(())
}

/// Parses the remote package and collects its files, from the segment metadata and, unless
/// skipped, the NTFS metadata.
async fn read_remote_metadata<Reader>(
    reader: Reader,
    len: u64,
    cache_path: &Path,
    try_skip_ntfs: bool,
    progress: &dyn Progress,
) -> Result<(XvdFile, HashMap<String, SegmentFile>)>
where
    Reader: AsyncRead + Unpin,
{
    let mut remote_file = streaming::PrefixCacheFile::new(reader, len, cache_path)
        .await
        .map_err(|e| Error::msg(format!("could not create {}: {e}", cache_path.display())))?;
    let remote_xvd = XvdFile::parse(&mut remote_file)
        .await
        .map_err(|e| Error::msg(format!("could not parse the package: {e}")))?;

    let mut rfiles: HashMap<String, SegmentFile> = HashMap::new();
    let files = remote_xvd
        .parse_user_package_files(&mut remote_file)
        .await
        .map_err(|e| Error::msg(format!("could not read the package files: {e}")))?;
    if let Some(segment_metadata) = files.get("SegmentMetadata.bin") {
        rfiles = remote_xvd
            .parse_segment_metadata(&mut remote_file, segment_metadata)
            .await
            .map_err(|e| Error::msg(format!("could not read the segment metadata: {e}")))?;
    }

    if !try_skip_ntfs || rfiles.is_empty() {
        progress.message("Downloading ntfs...");
        let sfiles = remote_xvd
            .parse_ntfs_segment_metadata(&mut remote_file, !rfiles.is_empty())
            .await
            .map_err(|e| Error::msg(format!("could not read the NTFS metadata: {e}")))?;
        rfiles.extend(sfiles);
    }
    Ok((remote_xvd, rfiles))
}

/// Files of the last finished install. A missing or unreadable file means "nothing installed",
/// so every file is downloaded.
async fn read_local_metadata(final_path: &Path) -> HashMap<String, SegmentFile> {
    let Ok(mut file) = OpenOptions::new().read(true).open(final_path).await else {
        return HashMap::new();
    };
    let xvd = match XvdFile::parse(&mut file).await {
        Ok(xvd) => xvd,
        Err(err) => {
            tracing::warn!("ignoring unreadable {}: {err}", final_path.display());
            return HashMap::new();
        }
    };

    // Errors of msixvc are not `Send`, turn them into strings before the next await.
    let mut lfiles = HashMap::new();
    let files = xvd
        .parse_user_package_files(&mut file)
        .await
        .map_err(|e| e.to_string());
    match files {
        Ok(files) => {
            if let Some(segment_metadata) = files.get("SegmentMetadata.bin") {
                let sfiles = xvd
                    .parse_segment_metadata(&mut file, segment_metadata)
                    .await
                    .map_err(|e| e.to_string());
                match sfiles {
                    Ok(sfiles) => lfiles = sfiles,
                    Err(err) => tracing::warn!("ignoring the local segment metadata: {err}"),
                }
            }
        }
        Err(err) => tracing::warn!("ignoring the local package files: {err}"),
    }
    if let Ok(sfiles) = xvd
        .parse_ntfs_segment_metadata(&mut file, !lfiles.is_empty())
        .await
    {
        lfiles.extend(sfiles);
    }
    lfiles
}

async fn download_job(
    client: &reqwest::Client,
    remote_xvd: &XvdFile,
    origin: Origin<'_>,
    job: &Job,
    full_key: [u8; 32],
    counter: &Counter<'_>,
) -> Result<()> {
    let write_err =
        |e: std::io::Error| Error::msg(format!("could not write {}: {e}", job.target.display()));
    if let Some(folder) = job.target.parent() {
        std::fs::create_dir_all(folder).map_err(write_err)?;
    }
    // Pages are encrypted one by one, so a partly downloaded file continues at its last whole page.
    let page = msixvc::layout::PAGE_SIZE as u64;
    let skip = if job.resume {
        std::fs::symlink_metadata(&job.target)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len().min(job.content.length) / page * page)
            .unwrap_or(0)
    } else {
        0
    };
    let mut fout = if skip > 0 {
        let file = OpenOptions::new()
            .write(true)
            .open(&job.target)
            .await
            .map_err(write_err)?;
        file.set_len(skip).await.map_err(write_err)?;
        let mut file = file;
        use tokio::io::AsyncSeekExt;
        file.seek(std::io::SeekFrom::Start(skip))
            .await
            .map_err(write_err)?;
        file
    } else {
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&job.target)
            .await
            .map_err(write_err)?
    };
    let content = SegmentFile {
        offset: job.content.offset + skip,
        length: job.content.length - skip,
        data_hashs: vec![],
        keep_encrypted: job.content.keep_encrypted,
    };

    let total = job.content.length;
    let progress = counter.progress;
    // The kept part of the file counts as done.
    counter.advance(skip);
    progress.file(&job.name, skip, total);
    let mut last = 0;
    let on_progress = |pos: u64, _| {
        counter.advance(pos.saturating_sub(last));
        last = pos;
        progress.file(&job.name, (skip + pos).min(total), total);
    };

    let result = match origin {
        Origin::File(path) => {
            let mut input = File::open(path)
                .await
                .map_err(|e| Error::msg(format!("could not open {}: {e}", path.display())))?;
            remote_xvd
                .extract_file(&mut input, &mut fout, &content, full_key, on_progress)
                .await
        }
        Origin::Http(url) => {
            remote_xvd
                .download_file_http(client, url, &mut fout, &content, full_key, on_progress)
                .await
        }
    };
    result.map_err(|e| Error::msg(format!("{}: {e}", job.name)))?;
    fout.flush().await.map_err(write_err)?;
    progress.file(&job.name, total, total);
    progress.file_finished(&job.name);
    note_finished(&job.journal, &job.name);
    Ok(())
}

/// Overall byte progress, shared by the concurrent downloads.
struct Counter<'p> {
    progress: &'p dyn Progress,
    done: AtomicU64,
    total: AtomicU64,
}

impl<'p> Counter<'p> {
    fn new(progress: &'p dyn Progress) -> Self {
        Self {
            progress,
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
        }
    }

    fn report(&self) {
        let done = self.done.load(Ordering::Relaxed);
        let total = self.total.load(Ordering::Relaxed);
        self.progress.bytes(done, Some(total.max(done)));
    }

    fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
        self.report();
    }

    /// The overall total becomes what is done so far plus `remaining`.
    fn set_remaining(&self, remaining: u64) {
        let done = self.done.load(Ordering::Relaxed);
        self.set_total(done.saturating_add(remaining));
    }

    fn advance(&self, delta: u64) {
        if delta == 0 {
            return;
        }
        self.done.fetch_add(delta, Ordering::Relaxed);
        self.report();
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{CancellationToken, InstallOptions, Progress, Source, TokenManager, install};

    /// The daemon runs installs on its own tasks, so the futures have to stay `Send`.
    #[allow(dead_code)]
    fn futures_are_send(
        client: &reqwest::Client,
        tokens: &TokenManager,
        options: &InstallOptions,
        file: &xodus::models::packagespc::PackageFile,
        progress: &dyn Progress,
        cancel: &CancellationToken,
    ) {
        fn is_send<T: Send>(_: T) {}
        is_send(install(
            client,
            tokens,
            Source::Url(String::new()),
            options,
            progress,
            cancel,
        ));
        is_send(crate::download::download_package_file(
            client,
            file,
            std::path::Path::new("."),
            progress,
            cancel,
        ));
    }

    #[test]
    fn parses_sources() {
        assert_eq!(
            Source::parse("file:///tmp/a.msixvc"),
            Source::File(PathBuf::from("/tmp/a.msixvc"))
        );
        assert_eq!(
            Source::parse("https://assets1.xboxlive.com/x.msixvc"),
            Source::Url("https://assets1.xboxlive.com/x.msixvc".into())
        );
        assert_eq!(
            Source::parse("a6a4b3a2-0000-4000-8000-000000000000"),
            Source::ContentId("a6a4b3a2-0000-4000-8000-000000000000".into())
        );
    }
}
