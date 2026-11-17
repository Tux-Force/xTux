//! Deleting an installed game.

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Checks that `game_dir` is a game folder directly inside one of `library_dirs` and returns its canonical path.
/// Nothing else is ever deleted: no library folder itself, no folder outside the libraries, no non-game folder.
pub fn check(game_dir: &Path, library_dirs: &[PathBuf]) -> Result<PathBuf> {
    let meta = std::fs::symlink_metadata(game_dir)?;
    if !meta.is_dir() {
        return Err(Error::msg(format!(
            "{} is not a folder",
            game_dir.display()
        )));
    }
    let game = std::fs::canonicalize(game_dir)?;
    crate::layout::detect(&game)
        .ok_or_else(|| Error::msg(format!("{} is not a game folder", game.display())))?;
    let parent = game.parent().ok_or("a game folder has a parent")?;
    let in_library = library_dirs
        .iter()
        .filter_map(|dir| std::fs::canonicalize(dir).ok())
        .any(|dir| dir == parent);
    if !in_library {
        return Err(Error::msg(format!(
            "{} is not in a library folder; only games of the library can be deleted",
            game.display()
        )));
    }
    // A game folder that is a mount point would take a whole filesystem with it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata(&game)?.dev() != std::fs::metadata(parent)?.dev() {
            return Err(Error::msg(format!(
                "{} is a mount point; not deleted",
                game.display()
            )));
        }
    }
    Ok(game)
}

/// Deletes a game folder of the library with everything in it (see [`check`]).
pub fn remove(game_dir: &Path, library_dirs: &[PathBuf]) -> Result<()> {
    let game = check(game_dir, library_dirs)?;
    let _access = crate::access::acquire(&game, true)?;
    // Renamed first inside its library: the game disappears at once and as a whole, and what is deleted is the
    // folder that was checked, even if a path is swapped afterwards.
    let name = game
        .file_name()
        .ok_or("a game folder has a name")?
        .to_string_lossy();
    let doomed = game.with_file_name(format!(".xtux-deleting-{}-{name}", std::process::id()));
    std::fs::rename(&game, &doomed).map_err(|e| {
        // A Windows drive mounted read-only (Fast Startup, hibernation) ends here.
        Error::msg(format!("could not delete {}: {e}", game.display()))
    })?;
    if !std::fs::symlink_metadata(&doomed)?.is_dir() {
        return Err(Error::msg(format!(
            "{} changed while it was deleted",
            game.display()
        )));
    }
    std::fs::remove_dir_all(&doomed).map_err(|e| {
        Error::msg(format!(
            "{} is gone from the library, but deleting its files stopped: {e}; the rest is in {}",
            game.display(),
            doomed.display()
        ))
    })?;
    if let Some(log) = crate::logs::game_log(&game) {
        let _ = std::fs::remove_file(log);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(dir: &Path) {
        std::fs::create_dir_all(dir.join("Content")).unwrap();
        std::fs::write(dir.join("FD1A3647-8CBF-439E-B7F9-B67AF942BA4A"), b"").unwrap();
    }

    #[test]
    fn deletes_only_games_of_the_library() {
        let root = tempfile::tempdir().unwrap();
        let library = root.path().join("lib");
        let inside = library.join("Game");
        game(&inside);
        let outside = root.path().join("Other");
        game(&outside);
        let not_a_game = library.join("Stuff");
        std::fs::create_dir_all(&not_a_game).unwrap();
        let libraries = vec![library.clone()];

        assert!(remove(&outside, &libraries).is_err());
        assert!(remove(&not_a_game, &libraries).is_err());
        assert!(remove(&library, &libraries).is_err());
        assert!(remove(&inside.join("Content"), &libraries).is_err());
        assert!(outside.exists() && not_a_game.exists());

        remove(&inside, &libraries).unwrap();
        assert!(!inside.exists());
        assert!(library.exists());
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_a_link_to_a_game() {
        let root = tempfile::tempdir().unwrap();
        let library = root.path().join("lib");
        std::fs::create_dir_all(&library).unwrap();
        let elsewhere = root.path().join("Elsewhere");
        game(&elsewhere);
        let link = library.join("Link");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        assert!(remove(&link, &[library]).is_err());
        assert!(elsewhere.exists());
    }
}
