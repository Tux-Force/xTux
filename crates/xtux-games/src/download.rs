//! Downloading package files (`.msixvc`, `.xsp`, ...) from the Xbox CDNs.

use std::path::{Component, Path, PathBuf};

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use xodus::models::packagespc::PackageFile;

use crate::cancel;
use crate::{Error, Progress, Result};

/// Downloads `file` into `dest_dir`, trying each CDN in turn until one answers.
///
/// The file is written as `dest_dir/<file name>`. Byte progress goes to `progress`; `cancel`
/// stops the download between chunks, removes the partial file and returns [`Error::Cancelled`].
/// Returns the path of the written file.
pub async fn download_package_file(
    client: &reqwest::Client,
    file: &PackageFile,
    dest_dir: &Path,
    progress: &dyn Progress,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    cancel::check(cancel)?;
    let target = safe_join(dest_dir, &file.file_name)?;
    if target.parent() != Some(dest_dir) {
        return Err(Error::msg(format!("{:?} is not a plain file name", file.file_name)));
    }
    let expected = u64::try_from(file.file_size).ok().filter(|size| *size > 0);

    let urls = file.download_urls();
    if urls.is_empty() {
        return Err(Error::msg(format!(
            "{}: the server returned no CDN for this file",
            file.file_name
        )));
    }

    progress.message(&format!("Downloading {}", file.file_name));
    let response = cancel::until_cancelled(cancel, first_response(client, &urls)).await?;

    let written = cancel::until_cancelled(
        cancel,
        write_response(response, &target, expected, progress, cancel),
    )
    .await;
    match written {
        Ok(()) => Ok(target),
        Err(err) => {
            // Do not leave a truncated file that looks complete.
            if let Err(remove_err) = tokio::fs::remove_file(&target).await
                && remove_err.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!("could not remove the partial file {}: {remove_err}", target.display());
            }
            Err(err)
        }
    }
}

/// Requests the first URL that answers with a success status.
async fn first_response(client: &reqwest::Client, urls: &[String]) -> Result<reqwest::Response> {
    for url in urls {
        match client.get(url).send().await.and_then(|r| r.error_for_status()) {
            Ok(response) => return Ok(response),
            // Log the host only: CDN URLs can carry signed query parameters.
            Err(err) => tracing::warn!("{}: {}; trying the next CDN", url_host(url), err.without_url()),
        }
    }
    Err(Error::msg("Failed to request the download from any CDN"))
}

async fn write_response(
    response: reqwest::Response,
    target: &Path,
    expected: Option<u64>,
    progress: &dyn Progress,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut out = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(target)
        .await?;
    let mut stream = response.bytes_stream();
    let mut done: u64 = 0;
    progress.bytes(0, expected);

    while let Some(chunk) = stream.next().await {
        cancel::check(cancel)?;
        let chunk = chunk.map_err(|e| Error::Http(e.without_url()))?;
        done = done.saturating_add(chunk.len() as u64);
        if let Some(expected) = expected
            && done > expected
        {
            return Err(Error::msg(format!(
                "the server sent more than the announced {expected} bytes"
            )));
        }
        out.write_all(&chunk).await?;
        progress.bytes(done, expected);
    }
    out.flush().await?;

    if let Some(expected) = expected
        && done != expected
    {
        return Err(Error::msg(format!(
            "the download ended after {done} of {expected} bytes"
        )));
    }
    Ok(())
}

fn url_host(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| "<invalid url>".to_owned())
}

/// Joins a relative path from untrusted package data (for example `Content\\bin\\game.exe`) to
/// `base`, accepting `/` and `\\` as separators.
///
/// Rejects anything that could leave `base`: `..`, drive prefixes (`C:`) and NUL bytes.
/// Empty and `.` components are skipped, so leading separators (`/etc`, `\\\\server`) stay
/// relative to `base`.
pub fn safe_join(base: &Path, name: &str) -> Result<PathBuf> {
    let reject = || Error::msg(format!("refusing unsafe path {name:?} from the package"));
    if name.contains('\0') {
        return Err(reject());
    }
    let mut path = base.to_path_buf();
    let mut parts = 0;
    for part in name.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        // A component with a drive letter (`C:`) or anything that is not a normal name.
        if part.contains(':') {
            return Err(reject());
        }
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(_)), None) => path.push(part),
            _ => return Err(reject()),
        }
        parts += 1;
    }
    if parts == 0 {
        return Err(reject());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::safe_join;

    #[test]
    fn joins_windows_and_unix_separators() {
        let base = Path::new("/games/x");
        assert_eq!(
            safe_join(base, "Content\\bin/game.exe").unwrap(),
            Path::new("/games/x/Content/bin/game.exe")
        );
        assert_eq!(safe_join(base, "./a//b").unwrap(), Path::new("/games/x/a/b"));
    }

    #[test]
    fn rejects_escapes() {
        let base = Path::new("/games/x");
        for bad in [
            "..",
            "../etc/passwd",
            "a\\..\\..\\b",
            "C:\\Windows\\x",
            "a\0b",
            "",
            "./",
        ] {
            assert!(safe_join(base, bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn absolute_and_unc_stay_inside_base() {
        // Leading separators are dropped, so the result always stays inside the base.
        let base = Path::new("/games/x");
        assert_eq!(safe_join(base, "/etc/passwd").unwrap(), Path::new("/games/x/etc/passwd"));
        assert_eq!(
            safe_join(base, "\\\\server\\share\\x").unwrap(),
            Path::new("/games/x/server/share/x")
        );
    }
}
