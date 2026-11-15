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
    /// The queue created the folder (it did not exist before), so removing an unfinished item may delete it.
    pub created_folder: bool,
    pub state: State,
    pub done: u64,
    pub total: Option<u64>,
    /// Last step or error.
    pub message: String,
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
            if item.state == State::Running {
                item.state = State::Queued;
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
            created_folder,
            state: State::Queued,
            done: 0,
            total: None,
            message: "Waiting".into(),
        });
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
