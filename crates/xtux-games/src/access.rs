//! Cross-process guards keep a running game separate from writes to its installation.
use crate::{Error, Result};
use sha1::{Digest, Sha1};
use std::fs::File;
use std::path::Path;

pub struct Guard {
    _file: File,
}

fn open(path: &Path, write: bool) -> Result<Guard> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let result = if write {
        fs2::FileExt::try_lock_exclusive(&file)
    } else {
        fs2::FileExt::try_lock_shared(&file)
    };
    result.map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            Error::msg(
                "This installation is running or being modified; finish that operation first",
            )
        } else {
            e.into()
        }
    })?;
    Ok(Guard { _file: file })
}

pub fn acquire(game: &Path, write: bool) -> Result<Guard> {
    let canonical = std::fs::canonicalize(game).unwrap_or_else(|_| {
        if game.is_absolute() {
            game.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(game)
        }
    });
    let dir = crate::logs::state_dir()
        .ok_or("no state directory")?
        .join("locks");
    std::fs::create_dir_all(&dir)?;
    let id = format!(
        "{:x}",
        Sha1::digest(canonical.as_os_str().as_encoded_bytes())
    );
    open(&dir.join(id), write)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn running_readers_and_install_writers_exclude_each_other() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lock");
        let a = open(&path, false).unwrap();
        let b = open(&path, false).unwrap();
        assert!(open(&path, true).is_err());
        drop(a);
        drop(b);
        let writer = open(&path, true).unwrap();
        assert!(open(&path, false).is_err());
        drop(writer);
        assert!(open(&path, true).is_ok());
    }
}
