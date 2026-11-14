//! User settings shared by xTux clients: `$XDG_CONFIG_HOME/xtux/settings.toml` (`~/.config/xtux/settings.toml`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    /// Folders with one folder per game (xTux installs, Windows `XboxGames` folders).
    pub library_dirs: Vec<PathBuf>,
    /// Wine binary used to start games.
    pub wine: Option<String>,
    /// Wine prefix used for games without their own.
    pub prefix: Option<PathBuf>,
    /// Store market (for example `DE`); taken from the system language when not set.
    pub market: Option<String>,
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `$XDG_CONFIG_HOME/xtux/settings.toml`.
pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".config")))?;
    Some(base.join("xtux/settings.toml"))
}

/// Where xTux installs games by default: `$XDG_DATA_HOME/xtux/games`.
pub fn default_games_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".local/share")))?;
    Some(base.join("xtux/games"))
}

/// `XboxGames` folders of the Windows Xbox app on mounted drives.
pub fn detect_windows_libraries() -> Vec<PathBuf> {
    let user = std::env::var("USER").unwrap_or_default();
    let mut roots = vec![PathBuf::from("/mnt"), PathBuf::from("/media")];
    if !user.is_empty() {
        roots.push(Path::new("/run/media").join(&user));
        roots.push(Path::new("/media").join(&user));
    }
    let mut found = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let candidate = entry.path().join("XboxGames");
            if candidate.is_dir() && !found.contains(&candidate) {
                found.push(candidate);
            }
        }
    }
    found
}

/// Market from the system language, for example `de_DE.UTF-8` gives `DE`.
pub fn market_from_locale() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.is_empty() && v != "C" && v != "POSIX")
        .and_then(|locale| {
            let region = locale.split(['.', '@']).next()?.split('_').nth(1)?;
            (region.len() == 2).then(|| region.to_ascii_uppercase())
        })
}

impl Settings {
    /// Reads the settings file; missing values get their defaults.
    pub fn load() -> Result<Self> {
        let mut settings = match path() {
            Some(file) if file.exists() => {
                let text = std::fs::read_to_string(&file)?;
                toml::from_str(&text).map_err(|e| Error::msg(format!("{}: {e}", file.display())))?
            }
            _ => Settings::default(),
        };
        if settings.library_dirs.is_empty() {
            settings.library_dirs.extend(default_games_dir());
            settings.library_dirs.extend(detect_windows_libraries());
        }
        if settings.market.is_none() {
            settings.market = market_from_locale();
        }
        Ok(settings)
    }

    pub fn save(&self) -> Result<()> {
        let file = path().ok_or("cannot determine the settings folder")?;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| Error::msg(e.to_string()))?;
        std::fs::write(&file, text)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let settings = Settings {
            library_dirs: vec!["/games".into()],
            wine: Some("/opt/wine/bin/wine".into()),
            prefix: None,
            market: Some("DE".into()),
        };
        let text = toml::to_string_pretty(&settings).unwrap();
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), settings);
        assert_eq!(toml::from_str::<Settings>("").unwrap(), Settings::default());
    }
}
