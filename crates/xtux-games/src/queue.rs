//! The download queue: games waiting to be installed, one after another.
//!
//! Kept in `$XDG_STATE_HOME/xtux/downloads.json` so it survives restarts; xtux-service works through it (see
//! `xtux-service/src/downloads.rs`), clients only read and change it through the service.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Queued,
    Running,
    Paused,
    /// Pause asked for; the install is stopping.
    Pausing,
    /// Remove asked for; the install is stopping, then the item goes.
    Removing,
    Failed,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: u64,
    /// Store ID.
    pub product_id: String,
    pub name: String,
    /// Content ID of the PC package.
    pub content_id: String,
    /// Game folder the install goes into.
    pub destination: PathBuf,
    #[serde(default)]
    pub update: bool,
    /// The queue created the folder (it did not exist before), so removing an unfinished item may delete it.
    pub created_folder: bool,
    pub state: State,
    pub done: u64,
    pub total: Option<u64>,
    /// Last step or error.
    pub message: String,
    /// Download speed in bytes per second while running (not saved).
    #[serde(skip)]
    pub speed: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Queue {
    pub items: Vec<Item>,
    next_id: u64,
}

fn file() -> Option<PathBuf> {
    Some(crate::logs::state_dir()?.join("downloads.json"))
}

/// A folder name for a game: letters, digits and a few signs; never empty, never hidden.
pub fn folder_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || " -_.()".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim().to_string();
    if cleaned.is_empty() {
        "Game".into()
    } else {
        cleaned.chars().take(100).collect()
    }
}

impl Queue {
    /// The saved queue; an item that was running when the service stopped waits again.
    pub fn load() -> Self {
        let mut queue: Queue = file()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        for item in &mut queue.items {
            match item.state {
                State::Running => item.state = State::Queued,
                State::Pausing | State::Removing => item.state = State::Paused,
                _ => {}
            }
        }
        queue
    }

    pub fn save(&self) {
        let Some(file) = file() else { return };
        let Some(dir) = file.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        use std::io::Write;
        let Ok(mut temp) = tempfile::NamedTempFile::new_in(dir) else {
            return;
        };
        if temp
            .write_all(&serde_json::to_vec_pretty(self).unwrap_or_default())
            .is_ok()
        {
            let _ = temp.persist(&file);
        }
    }

    /// Adds a game to the end of the queue, into `library/<name>`; `library` must be one of `library_dirs`.
    pub fn add(
        &mut self,
        product_id: &str,
        name: &str,
        content_id: &str,
        library: &Path,
        library_dirs: &[PathBuf],
    ) -> Result<u64> {
        if !crate::art::is_store_id(product_id) {
            return Err(Error::msg(format!("{product_id:?} is not a Store ID")));
        }
        if !crate::layout::is_content_id(content_id) {
            return Err(Error::msg(format!("{content_id:?} is not a content ID")));
        }
        let library_ok = library_dirs.iter().any(|dir| dir == library);
        if !library_ok {
            return Err(Error::msg(format!(
                "{} is not a library folder",
                library.display()
            )));
        }
        if self
            .items
            .iter()
            .any(|i| i.product_id == product_id && i.state != State::Done)
        {
            return Err(Error::msg(format!(
                "{name} is in the download queue already"
            )));
        }
        // A folder of its own: never one that exists or that another item uses (names like "Game®" and
        // "Game™" give the same folder name).
        let base = folder_name(name);
        let destination = (1..1000)
            .map(|n| {
                if n == 1 {
                    library.join(&base)
                } else {
                    library.join(format!("{base} ({n})"))
                }
            })
            .find(|dir| {
                std::fs::symlink_metadata(dir).is_err()
                    && !self.items.iter().any(|i| &i.destination == dir)
            })
            .ok_or("no free folder name for this game")?;
        // Decided when the worker creates the folder (Queue::claim_folder), not now.
        let created_folder = false;
        self.next_id += 1;
        let id = self.next_id;
        self.items.push(Item {
            id,
            product_id: product_id.into(),
            name: name.into(),
            content_id: content_id.to_uppercase(),
            destination,
            update: false,
            created_folder,
            state: State::Queued,
            done: 0,
            total: None,
            message: "Waiting".into(),
            speed: 0,
        });
        Ok(id)
    }

    /// Updates use exactly the checked existing folder; cancellation must never delete it.
    pub fn update(&mut self, path: &Path, library_dirs: &[PathBuf]) -> Result<u64> {
        let installed = crate::updates::installed(path, library_dirs)?;
        if self
            .items
            .iter()
            .any(|i| i.destination == installed.path && i.state != State::Done)
        {
            return Err(Error::msg(
                "This installation is already in the download queue",
            ));
        }
        let library = library_dirs
            .iter()
            .find(|dir| std::fs::canonicalize(dir).ok().as_deref() == installed.path.parent())
            .ok_or("the update target is not in a library")?;
        let id = self.add(
            &installed.product_id,
            &installed.name,
            &installed.content_id,
            library,
            library_dirs,
        )?;
        let item = self.get_mut(id).ok_or("queued update missing")?;
        item.destination = installed.path;
        item.update = true;
        item.created_folder = false;
        item.message = "Waiting to update".into();
        Ok(id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Item> {
        self.items.iter_mut().find(|i| i.id == id)
    }

    /// The next item to install.
    pub fn next_queued(&self) -> Option<&Item> {
        self.items.iter().find(|i| i.state == State::Queued)
    }

    /// Creates the item's folder right before its install; only a folder made here may be deleted later.
    pub fn claim_folder(item: &mut Item) -> Result<()> {
        if item.update {
            let meta = std::fs::symlink_metadata(&item.destination)?;
            crate::layout::detect(&item.destination)
                .ok_or("the update target is no longer installed")?;
            if !meta.is_dir() || meta.is_symlink() {
                return Err(Error::msg("the update target changed; not modified"));
            }
            let parent = item
                .destination
                .parent()
                .ok_or("the update target has no library")?
                .to_path_buf();
            let installed = crate::updates::installed(&item.destination, &[parent])?;
            if !installed.content_id.eq_ignore_ascii_case(&item.content_id)
                || !installed.product_id.eq_ignore_ascii_case(&item.product_id)
            {
                return Err(Error::msg(
                    "another package replaced the queued update target; not modified",
                ));
            }
            return Ok(());
        }
        match std::fs::create_dir(&item.destination) {
            Ok(()) => {
                item.created_folder = true;
                Ok(())
            }
            // Resumed: made by this item before, or by someone else in between.
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if !item.created_folder && crate::layout::detect(&item.destination).is_none() {
                    let empty = std::fs::read_dir(&item.destination)?.next().is_none();
                    if !empty {
                        return Err(Error::msg(format!(
                            "{} exists and is not a game folder",
                            item.destination.display()
                        )));
                    }
                }
                Ok(())
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Moves an item one place up or down.
    pub fn shift(&mut self, id: u64, up: bool) {
        let Some(pos) = self.items.iter().position(|i| i.id == id) else {
            return;
        };
        let other = if up {
            pos.checked_sub(1)
        } else {
            Some(pos + 1)
        };
        if let Some(other) = other.filter(|o| *o < self.items.len()) {
            self.items.swap(pos, other);
        }
    }

    /// Takes an item out of the queue and returns it.
    pub fn remove(&mut self, id: u64) -> Option<Item> {
        let pos = self.items.iter().position(|i| i.id == id)?;
        Some(self.items.remove(pos))
    }

    /// Drops finished items.
    pub fn clear_done(&mut self) {
        self.items.retain(|i| i.state != State::Done);
    }
}

/// Deletes the folder of an unfinished download the queue created itself, inside one of `library_dirs`.
pub fn delete_partial(item: &Item, library_dirs: &[PathBuf]) -> Result<()> {
    if item.update {
        return Ok(());
    }
    if !item.created_folder || item.state == State::Done || !item.destination.exists() {
        return Ok(());
    }
    let meta = std::fs::symlink_metadata(&item.destination)?;
    if !meta.is_dir() {
        return Err(Error::msg("the download folder is not a folder"));
    }
    let folder = std::fs::canonicalize(&item.destination)?;
    let parent = folder.parent().ok_or("a download folder has a parent")?;
    let in_library = library_dirs
        .iter()
        .filter_map(|d| std::fs::canonicalize(d).ok())
        .any(|d| d == parent);
    if !in_library {
        return Err(Error::msg("the download folder is not in a library folder"));
    }
    std::fs::remove_dir_all(&folder)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names_are_safe() {
        assert_eq!(
            folder_name("Farming Simulator 25 (PC)"),
            "Farming Simulator 25 (PC)"
        );
        assert_eq!(folder_name("Call of Duty®: MW"), "Call of Duty__ MW");
        assert_eq!(folder_name("../../etc"), "_.._etc");
        assert_eq!(folder_name("..."), "Game");
        assert!(!folder_name("a/b").contains('/'));
    }

    #[test]
    fn cancelling_an_update_keeps_the_existing_installation() {
        let root = tempfile::tempdir().unwrap();
        // Queue paths are canonical; the temporary folder is a link on macOS (/var -> /private/var).
        let root_path = std::fs::canonicalize(root.path()).unwrap();
        let game = root_path.join("Game");
        std::fs::create_dir_all(game.join("Content")).unwrap();
        std::fs::write(game.join("Content/MicrosoftGame.config"),
            "<Game><StoreId>9AAAAAAAAAAA</StoreId><ShellVisuals DefaultDisplayName=\"Game\"/></Game>").unwrap();
        let id = "FD1A3647-8CBF-439E-B7F9-B67AF942BA4A";
        let mut header = [0u8; 4096];
        header[512..520].copy_from_slice(b"msft-xvd");
        header[0x210..0x218].copy_from_slice(&133000000000000000u64.to_le_bytes());
        header[0x218..0x220].copy_from_slice(&4096u64.to_le_bytes());
        std::fs::write(game.join(id), header).unwrap();
        std::fs::write(game.join("Content/keep.dat"), b"saved installation").unwrap();
        let libraries = vec![root_path.clone()];
        let mut queue = Queue::default();
        let n = queue.update(&game, &libraries).unwrap();
        assert_eq!(queue.items[0].destination, game);
        assert!(queue.items[0].update && !queue.items[0].created_folder);
        assert!(queue.update(&game, &libraries).is_err());
        Queue::claim_folder(queue.get_mut(n).unwrap()).unwrap();
        let mut item = queue.remove(n).unwrap();
        // Even a stale created_folder flag cannot turn an update cancellation into an uninstall.
        item.created_folder = true;
        delete_partial(&item, &libraries).unwrap();
        assert_eq!(
            std::fs::read(game.join("Content/keep.dat")).unwrap(),
            b"saved installation"
        );
        // Installs of the Windows Xbox app are updated as well.
        std::fs::write(game.join(format!("{id}.xvi")), b"").unwrap();
        let windows = queue.update(&game, &libraries).unwrap();
        Queue::claim_folder(queue.get_mut(windows).unwrap()).unwrap();
        std::fs::remove_dir_all(&game).unwrap();
        assert!(Queue::claim_folder(&mut item).is_err());
        assert!(!game.exists());
    }

    #[test]
    fn adds_moves_and_removes() {
        let root = tempfile::tempdir().unwrap();
        let library = root.path().join("lib");
        std::fs::create_dir(&library).unwrap();
        let libs = vec![library.clone()];
        let mut queue = Queue::default();
        let id = "FD1A3647-8CBF-439E-B7F9-B67AF942BA4A";
        let a = queue
            .add("9PM709J61828", "Game A", id, &library, &libs)
            .unwrap();
        let b = queue
            .add("9NPDN9R45JX4", "Game B", id, &library, &libs)
            .unwrap();
        assert!(
            queue
                .add("9PM709J61828", "Game A", id, &library, &libs)
                .is_err()
        );
        assert!(
            queue
                .add("9AAAAAAAAAAA", "X", id, root.path(), &libs)
                .is_err()
        );
        assert!(queue.add("bad", "X", id, &library, &libs).is_err());
        assert_eq!(queue.next_queued().unwrap().id, a);
        queue.shift(b, true);
        assert_eq!(queue.items[0].id, b);
        assert_eq!(queue.items[0].destination, library.join("Game B"));
        // Same folder name: an own folder.
        let c = queue
            .add("9CCCCCCCCCCC", "Game B®", id, &library, &libs)
            .unwrap();
        let d = queue
            .add("9DDDDDDDDDDD", "Game B™", id, &library, &libs)
            .unwrap();
        let dest = |id| {
            queue
                .items
                .iter()
                .find(|i| i.id == id)
                .unwrap()
                .destination
                .clone()
        };
        assert_ne!(dest(c), dest(d));

        // Only a folder the queue created itself is deleted on removal.
        std::fs::create_dir_all(library.join("Game B")).unwrap();
        let mut item = queue.remove(b).unwrap();
        delete_partial(&item, &libs).unwrap();
        assert!(library.join("Game B").exists());
        std::fs::remove_dir(library.join("Game B")).unwrap();
        Queue::claim_folder(&mut item).unwrap();
        assert!(item.created_folder);
        std::fs::create_dir_all(library.join("Game B/Content")).unwrap();
        delete_partial(&item, &libs).unwrap();
        assert!(!library.join("Game B").exists());
        assert!(library.exists());
    }
}
