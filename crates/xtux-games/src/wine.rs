//! The Tux-Force Wine build, managed by xTux: found in the releases of
//! [Tux-Force/wine](https://github.com/Tux-Force/wine), checked against its SHA-256 file and unpacked into
//! `$XDG_DATA_HOME/xtux/wine/<tag>`. `current` links to the version in use, so the settings need no Wine path.

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::{Error, Progress, Result};

pub const REPO: &str = "Tux-Force/wine";
const ASSET: &str = "wine-tux-force-x86_64.tar.xz";
/// Top folder of the archive.
const TREE: &str = "wine-tux-force-x86_64";

/// A release of the Tux-Force Wine build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WineRelease {
    /// For example `wine-20261118-1d9aa74`.
    pub tag: String,
    pub url: String,
    pub sha256_url: String,
    pub size: u64,
}

/// The installed version xTux uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledWine {
    pub tag: String,
    /// The `wine` binary.
    pub wine: PathBuf,
}

/// `$XDG_DATA_HOME/xtux/wine` (normally `~/.local/share/xtux/wine`).
pub fn root() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("xtux/wine"))
}

/// The version `current` links to, when it is complete.
pub fn installed() -> Option<InstalledWine> {
    installed_in(&root()?)
}

fn installed_in(root: &Path) -> Option<InstalledWine> {
    let target = std::fs::read_link(root.join("current")).ok()?;
    let tag = target.file_name()?.to_string_lossy().into_owned();
    let wine = root.join(&tag).join("bin/wine");
    wine.is_file().then_some(InstalledWine { tag, wine })
}

/// The newest release that has the archive and its SHA-256 file.
pub async fn latest(client: &reqwest::Client) -> Result<WineRelease> {
    let releases: serde_json::Value = client
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases?per_page=20"
        ))
        .header("User-Agent", "xtux")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| Error::msg(format!("{REPO}: {e}")))?
        .json()
        .await
        .map_err(|e| Error::msg(format!("{REPO}: {e}")))?;
    pick_release(&releases).ok_or_else(|| {
        Error::msg(format!(
            "{REPO} has no release with {ASSET} yet; set a Wine binary in the settings instead"
        ))
    })
}

/// GitHub lists releases newest first; drafts are only visible to maintainers.
fn pick_release(releases: &serde_json::Value) -> Option<WineRelease> {
    releases.as_array()?.iter().find_map(|release| {
        if release["draft"].as_bool() == Some(true) {
            return None;
        }
        let tag = release["tag_name"].as_str()?;
        let assets = release["assets"].as_array()?;
        let asset = |name: &str| assets.iter().find(|a| a["name"].as_str() == Some(name));
        let archive = asset(ASSET)?;
        let checksum = asset(&format!("{ASSET}.sha256"))?;
        Some(WineRelease {
            tag: tag.to_string(),
            url: archive["browser_download_url"].as_str()?.to_string(),
            sha256_url: checksum["browser_download_url"].as_str()?.to_string(),
            size: archive["size"].as_u64().unwrap_or(0),
        })
    })
}

/// Downloads, checks and unpacks `release`, then makes it the current version. Returns the `wine` binary.
pub async fn install(
    client: &reqwest::Client,
    release: &WineRelease,
    progress: &dyn Progress,
) -> Result<InstalledWine> {
    let root = root().ok_or("cannot determine the data folder")?;
    install_into(client, release, &root, progress).await
}

async fn install_into(
    client: &reqwest::Client,
    release: &WineRelease,
    root: &Path,
    progress: &dyn Progress,
) -> Result<InstalledWine> {
    if !valid_tag(&release.tag) {
        return Err(Error::msg(format!(
            "unexpected release name {}",
            release.tag
        )));
    }
    tokio::fs::create_dir_all(root).await?;
    if let Some(done) = installed_in(root).filter(|w| w.tag == release.tag) {
        return Ok(done);
    }

    progress.message(&format!("Checking {}", release.tag));
    let expected = client
        .get(&release.sha256_url)
        .header("User-Agent", "xtux")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| Error::msg(format!("{ASSET}.sha256: {e}")))?
        .text()
        .await
        .map_err(|e| Error::msg(format!("{ASSET}.sha256: {e}")))?;
    let expected = expected
        .split_whitespace()
        .next()
        .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or("the SHA-256 file of the release is malformed")?
        .to_ascii_lowercase();

    progress.message(&format!("Downloading Tux-Force Wine {}", release.tag));
    let work = tempfile::Builder::new()
        .prefix(".download-")
        .tempdir_in(root)?;
    let archive = work.path().join(ASSET);
    let response = client
        .get(&release.url)
        .header("User-Agent", "xtux")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| Error::msg(format!("{ASSET}: {e}")))?;
    let total = response
        .content_length()
        .or(Some(release.size).filter(|s| *s > 0));
    let mut file = tokio::fs::File::create(&archive).await?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| Error::msg(format!("{ASSET}: {e}")))?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        done += chunk.len() as u64;
        progress.bytes(done, total);
    }
    file.flush().await?;
    drop(file);
    let actual: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != expected {
        return Err(Error::msg(format!(
            "{ASSET} does not match its SHA-256 file (got {actual}); nothing was installed"
        )));
    }

    progress.message("Unpacking Wine");
    let status = Command::new("tar")
        .arg("-xJf")
        .arg(&archive)
        .arg("-C")
        .arg(work.path())
        .status()
        .await
        .map_err(|e| Error::msg(format!("could not run tar: {e}")))?;
    if !status.success() {
        return Err(Error::msg(format!(
            "unpacking {ASSET} failed (is xz installed?)"
        )));
    }
    let tree = work.path().join(TREE);
    if !tree.join("bin/wine").is_file() {
        return Err(Error::msg(format!("{ASSET} has no {TREE}/bin/wine")));
    }
    let target = root.join(&release.tag);
    if target.exists() {
        tokio::fs::remove_dir_all(&target).await?;
    }
    tokio::fs::rename(&tree, &target).await?;

    // Switched by renaming a new link over the old one, so `current` always names a complete version.
    let link = root.join("current");
    let partial = root.join("current.partial");
    let _ = std::fs::remove_file(&partial);
    std::os::unix::fs::symlink(&release.tag, &partial)?;
    std::fs::rename(&partial, &link)?;
    remove_old_versions(root, &release.tag);
    progress.message(&format!("Tux-Force Wine {} is ready", release.tag));
    installed_in(root).ok_or_else(|| Error::msg("the installed Wine is incomplete"))
}

/// Release names become folder names: only `wine-` followed by letters, digits, dots and dashes.
fn valid_tag(tag: &str) -> bool {
    tag.starts_with("wine-")
        && tag.len() < 80
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// Keeps the current version only; games that still run keep their files open until they exit.
fn remove_old_versions(root: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != keep && valid_tag(&name) && entry.path().is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn picks_the_newest_complete_release() {
        let releases = serde_json::json!([
            {"tag_name": "wine-20261119-aaaaaaa", "draft": true, "assets": []},
            {"tag_name": "wine-20261118-bbbbbbb", "draft": false, "assets": [
                {"name": ASSET, "browser_download_url": "https://x/a", "size": 5}
            ]},
            {"tag_name": "wine-20261117-ccccccc", "draft": false, "assets": [
                {"name": ASSET, "browser_download_url": "https://x/b", "size": 7},
                {"name": "wine-tux-force-x86_64.tar.xz.sha256", "browser_download_url": "https://x/b.sha256"}
            ]}
        ]);
        let release = pick_release(&releases).unwrap();
        assert_eq!(release.tag, "wine-20261117-ccccccc");
        assert_eq!(release.sha256_url, "https://x/b.sha256");
        assert_eq!(release.size, 7);
        assert_eq!(pick_release(&serde_json::json!([])), None);
    }

    #[test]
    fn release_names_are_safe_folder_names() {
        assert!(valid_tag("wine-20261118-1d9aa74"));
        assert!(!valid_tag("wine-../../x"));
        assert!(!valid_tag("other-1"));
    }

    /// Serves `files` (path → body) on a local port for the given number of requests.
    fn serve(files: Vec<(&'static str, Vec<u8>)>, requests: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().take(requests) {
                let mut stream = stream.unwrap();
                let mut request = [0u8; 2048];
                let n = stream.read(&mut request).unwrap();
                let line = String::from_utf8_lossy(&request[..n]);
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let body = files
                    .iter()
                    .find(|(p, _)| *p == path)
                    .map(|(_, b)| b.clone())
                    .unwrap_or_default();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        address
    }

    fn fake_archive() -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(TREE).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("wine"), "#!/bin/sh\n").unwrap();
        let out = dir.path().join("a.tar.xz");
        let status = std::process::Command::new("tar")
            .arg("-cJf")
            .arg(&out)
            .arg("-C")
            .arg(dir.path())
            .arg(TREE)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::read(out).unwrap()
    }

    #[tokio::test]
    async fn installs_a_checked_release_and_switches_current() {
        let archive = fake_archive();
        let hash: String = Sha256::digest(&archive)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let address = serve(
            vec![
                ("/a", archive.clone()),
                ("/a.sha256", format!("{hash}  {ASSET}\n").into_bytes()),
            ],
            2,
        );
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("wine-20200101-old0000/bin")).unwrap();
        let release = WineRelease {
            tag: "wine-20261118-1d9aa74".into(),
            url: format!("{address}/a"),
            sha256_url: format!("{address}/a.sha256"),
            size: archive.len() as u64,
        };
        let wine = install_into(
            &reqwest::Client::new(),
            &release,
            root.path(),
            &crate::Silent,
        )
        .await
        .unwrap();
        assert_eq!(wine.tag, release.tag);
        assert!(wine.wine.ends_with("wine-20261118-1d9aa74/bin/wine"));
        assert_eq!(installed_in(root.path()), Some(wine));
        assert!(!root.path().join("wine-20200101-old0000").exists());
    }

    #[tokio::test]
    async fn refuses_an_archive_that_does_not_match_its_checksum() {
        let archive = fake_archive();
        let address = serve(
            vec![
                ("/a", archive.clone()),
                (
                    "/a.sha256",
                    format!("{}  {ASSET}\n", "0".repeat(64)).into_bytes(),
                ),
            ],
            2,
        );
        let root = tempfile::tempdir().unwrap();
        let release = WineRelease {
            tag: "wine-20261118-1d9aa74".into(),
            url: format!("{address}/a"),
            sha256_url: format!("{address}/a.sha256"),
            size: 0,
        };
        let err = install_into(
            &reqwest::Client::new(),
            &release,
            root.path(),
            &crate::Silent,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("does not match"));
        assert_eq!(installed_in(root.path()), None);
        assert!(!root.path().join("wine-20261118-1d9aa74").exists());
    }
}
