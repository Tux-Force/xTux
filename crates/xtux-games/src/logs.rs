//! Log files of xTux: the daemon log and one log per game, overwritten on each start.
//!
//! `$XDG_STATE_HOME/xtux/` (`~/.local/state/xtux/`): `service.log` and `logs/<game folder>.log`.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// `$XDG_STATE_HOME/xtux`.
pub fn state_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("xtux"))
}

/// Log of xtux-service.
pub fn service_log() -> Option<PathBuf> {
    Some(state_dir()?.join("service.log"))
}

/// Log of the last start of the game in `game_dir` (Wine's output).
pub fn game_log(game_dir: &Path) -> Option<PathBuf> {
    let name: String = game_dir
        .file_name()?
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || "-_. ()".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(state_dir()?.join("logs").join(format!("{name}.log")))
}

/// The last `max` bytes of a log file as text, starting at a line start; empty when it does not exist.
pub fn tail(path: &Path, max: u64) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    match (start > 0, text.find('\n')) {
        (true, Some(cut)) => text[cut + 1..].to_string(),
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tails_from_a_line_start() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.log");
        std::fs::write(&file, "first line\nsecond line\nthird\n").unwrap();
        assert_eq!(tail(&file, 100), "first line\nsecond line\nthird\n");
        assert_eq!(tail(&file, 15), "third\n");
        assert_eq!(tail(&dir.path().join("missing"), 10), "");
    }

    #[test]
    fn game_log_names_are_file_safe() {
        let log = game_log(Path::new("/games/Call of Duty- MW2/../x:y")).unwrap();
        assert_eq!(log.file_name().unwrap(), "x_y.log");
    }
}
