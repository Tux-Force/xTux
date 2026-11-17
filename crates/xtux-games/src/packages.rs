//! Installed add-ons (DLC) of a running game, as the bridge reports them to xgameruntime.
//!
//! An add-on is a folder of the library in the same layout as a game whose `MicrosoftGame.config` names the game in
//! `<AllowedProducts>` (see library.rs). Its install is complete once its package header (the file named by its
//! content id) is in place: the install renames it there as its last step, so a half installed add-on is never
//! reported.
//!
//! Library folders may be shared with other users, and the game itself can write anywhere, so an add-on is only
//! reported when its folder, its file folder and its package header are real (no symbolic links), belong to this
//! user and are not writable by everyone. Add-ons from a library that fails this (for example an NTFS drive mounted
//! with mode 777) are left out with a warning.
//!
//! The list is plain text, one add-on per line with tab separated fields:
//!
//! ```text
//! <generation>
//! <content id>\t<store id>\t<version>\t<Windows path of the files>\t<name>\t<publisher>
//! ```
//!
//! The generation is a hash of the add-on lines; it changes when an add-on comes, goes or is updated, and is what a
//! waiting runtime sends back to be told about the next change.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, watch};

use crate::library::Game;

/// How often the library folders are looked at while a runtime waits.
const POLL: Duration = Duration::from_secs(2);
/// A waiting request is answered after this time even without a change, before xgameruntime's 30 second timeout.
pub const MAX_WAIT: Duration = Duration::from_secs(20);

/// Which add-ons belong to the running game.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// Library folders to look in.
    pub library_dirs: Vec<PathBuf>,
    /// Store IDs an add-on may name as its game: the game's own and the products it belongs to.
    pub store_ids: Vec<String>,
}

/// One installed add-on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// Content ID in its usual hyphenated upper case form; it is also the package identifier the game sees.
    pub content_id: String,
    pub store_id: String,
    pub version: String,
    /// Folder with the add-on's files.
    pub root: PathBuf,
    pub name: String,
    pub publisher: String,
}

/// Whether `path` is a real file or folder of this user that only this user (and its group) can change.
fn trusted(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let ok = !meta.file_type().is_symlink()
        && meta.uid() == rustix::process::getuid().as_raw()
        && meta.mode() & 0o002 == 0;
    if !ok {
        tracing::warn!(
            "add-on file {} is left out: a symbolic link, someone else's, or writable by everyone",
            path.display()
        );
    }
    ok
}

/// A path Wine reads as the same path: no `\` or `:` in its parts, which the Windows form would read differently.
fn plain_path(path: &Path) -> bool {
    path.to_str()
        .is_some_and(|p| !p.contains('\\') && !p.contains(':'))
}

impl Package {
    fn from_game(game: Game) -> Option<Package> {
        let layout = crate::layout::detect(&game.path)?;
        if !(trusted(&game.path) && trusted(&layout.root) && trusted(&layout.package)) {
            return None;
        }
        let root = std::fs::canonicalize(&layout.root).ok()?;
        if !plain_path(&root) {
            return None;
        }
        let content_id = layout
            .package
            .file_name()?
            .to_str()
            .filter(|n| crate::layout::is_content_id(n))?
            .to_uppercase();
        Some(Package {
            content_id,
            store_id: game.store_id.filter(|s| crate::art::is_store_id(s))?,
            version: game.version.unwrap_or_else(|| "0.0.0.0".into()),
            root,
            name: game.name.chars().take(200).collect(),
            publisher: game.publisher.unwrap_or_default(),
        })
    }
}

/// The installed add-ons of the game, sorted by content id.
pub fn list(scope: &Scope) -> Vec<Package> {
    if scope.store_ids.is_empty() {
        return Vec::new();
    }
    let mut packages: Vec<Package> =
        crate::library::addons_of(&scope.library_dirs, &scope.store_ids)
            .into_iter()
            .filter_map(Package::from_game)
            .collect();
    packages.sort_by(|a, b| a.content_id.cmp(&b.content_id));
    packages.dedup_by(|a, b| a.content_id == b.content_id);
    packages
}

/// `Z:\...` for a Unix path: Wine's Z: drive is the root of the file system.
fn windows_path(path: &Path) -> String {
    format!("Z:{}", path.to_string_lossy().replace('/', "\\"))
}

/// A field without the separators of the format.
fn field(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The list as text (see the module documentation).
pub fn encode(packages: &[Package]) -> String {
    use sha1::Digest;
    let lines: String = packages
        .iter()
        .map(|p| {
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}\n",
                p.content_id,
                p.store_id,
                field(&p.version),
                field(&windows_path(&p.root)),
                field(&p.name),
                field(&p.publisher),
            )
        })
        .collect();
    let hash = sha1::Sha1::digest(lines.as_bytes());
    let generation: String = hash.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("{generation}\n{lines}")
}

/// The generation line of an encoded list.
pub fn generation(encoded: &str) -> &str {
    encoded.lines().next().unwrap_or_default()
}

/// Watches the add-ons of one game for all waiting requests: one look at the library every [`POLL`], however many
/// requests wait, and only while one does.
pub struct Watcher {
    scope: Scope,
    current: Mutex<Option<watch::Receiver<String>>>,
}

impl Watcher {
    pub fn new(scope: Scope) -> Arc<Watcher> {
        Arc::new(Watcher {
            scope,
            current: Mutex::new(None),
        })
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// The encoded list once its generation differs from `known`, or the current one after [`MAX_WAIT`].
    pub async fn wait(self: &Arc<Self>, known: &str) -> String {
        let mut receiver = self.receiver().await;
        let known = known.trim().to_string();
        let answer = match tokio::time::timeout(
            MAX_WAIT,
            receiver.wait_for(|encoded| generation(encoded) != known),
        )
        .await
        {
            Ok(Ok(encoded)) => Some(encoded.clone()),
            _ => None,
        };
        answer.unwrap_or_else(|| receiver.borrow().clone())
    }

    /// A receiver of the poller, started when none runs.
    async fn receiver(self: &Arc<Self>) -> watch::Receiver<String> {
        let mut current = self.current.lock().await;
        if let Some(receiver) = current.as_ref().filter(|r| r.has_changed().is_ok()) {
            return receiver.clone();
        }
        let scope = self.scope.clone();
        let first = tokio::task::spawn_blocking(move || encode(&list(&scope)))
            .await
            .unwrap_or_default();
        let (sender, receiver) = watch::channel(first);
        let scope = self.scope.clone();
        tokio::spawn(async move {
            // Ends when no request has waited for a whole round.
            let mut idle = 0;
            while idle < 2 {
                tokio::time::sleep(POLL).await;
                if sender.receiver_count() == 0 {
                    idle += 1;
                    continue;
                }
                idle = 0;
                let scope = scope.clone();
                let Ok(encoded) = tokio::task::spawn_blocking(move || encode(&list(&scope))).await
                else {
                    break;
                };
                sender.send_if_modified(|old| {
                    let changed = *old != encoded;
                    if changed {
                        *old = encoded;
                    }
                    changed
                });
            }
        });
        *current = Some(receiver.clone());
        receiver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "F5C7AFFF-C1EE-4A6D-A5E0-96B6DA5EC721";

    fn addon(library: &Path, folder: &str, allowed: &str) -> PathBuf {
        let dir = library.join(folder);
        let content = dir.join(crate::layout::CONTENT_DIR);
        std::fs::create_dir_all(&content).unwrap();
        std::fs::write(
            content.join("MicrosoftGame.config"),
            format!(
                r#"<Game><Identity Name="D" Version="1.0.1.0"/><ShellVisuals DefaultDisplayName="Rally	Pack" PublisherDisplayName="Studio"/>
                <StoreId>9PCR7TSM1S96</StoreId><AllowedProducts><AllowedProduct>{allowed}</AllowedProduct></AllowedProducts></Game>"#
            ),
        )
        .unwrap();
        dir
    }

    #[test]
    fn lists_complete_addons_of_the_game() {
        let root = tempfile::tempdir().unwrap();
        let dir = addon(root.path(), "Rally", "9NNX1VVR3KNQ");
        addon(root.path(), "Other", "9NXN8GF8N9ZQ");
        let scope = Scope {
            library_dirs: vec![root.path().to_path_buf()],
            store_ids: vec!["9NKX70BBCDRN".into(), "9NNX1VVR3KNQ".into()],
        };
        // Not complete yet: no package header.
        assert!(list(&scope).is_empty());
        let empty = encode(&[]);

        std::fs::write(dir.join(ID.to_lowercase()), b"").unwrap();
        let packages = list(&scope);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].content_id, ID);
        assert_eq!(packages[0].version, "1.0.1.0");
        let encoded = encode(&packages);
        assert_ne!(generation(&encoded), generation(&empty));
        let line = encoded.lines().nth(1).unwrap();
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 6);
        assert_eq!(fields[0], ID);
        assert!(fields[3].starts_with("Z:\\") && fields[3].ends_with("\\Content"));
        // The tab in the name does not break the format (control characters are dropped from names).
        assert_eq!(fields[4], "RallyPack");
    }

    #[test]
    fn leaves_out_paths_wine_reads_differently() {
        assert!(plain_path(Path::new("/home/a/Games/Rally/Content")));
        assert!(!plain_path(Path::new("/home/a/x\\..\\y/Content")));
        assert!(!plain_path(Path::new("/home/a/C:/Content")));
    }

    #[test]
    fn leaves_out_symlinked_addons() {
        let root = tempfile::tempdir().unwrap();
        let real = addon(root.path(), "real", "9NKX70BBCDRN");
        std::fs::write(real.join(ID), b"").unwrap();
        let library = root.path().join("library");
        std::fs::create_dir(&library).unwrap();
        std::os::unix::fs::symlink(&real, library.join("Rally")).unwrap();
        let scope = Scope {
            library_dirs: vec![library],
            store_ids: vec!["9NKX70BBCDRN".into()],
        };
        assert!(list(&scope).is_empty());
    }

    #[tokio::test]
    async fn wait_answers_a_change_at_once() {
        let root = tempfile::tempdir().unwrap();
        let dir = addon(root.path(), "Rally", "9NKX70BBCDRN");
        std::fs::write(dir.join(ID), b"").unwrap();
        let scope = Scope {
            library_dirs: vec![root.path().to_path_buf()],
            store_ids: vec!["9NKX70BBCDRN".into()],
        };
        let empty = encode(&[]);
        let started = std::time::Instant::now();
        let answer = Watcher::new(scope).wait(generation(&empty)).await;
        assert!(started.elapsed() < POLL);
        assert_eq!(answer.lines().count(), 2);
    }
}
