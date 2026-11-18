//! User settings shared by xTux clients: `$XDG_CONFIG_HOME/xtux/settings.toml` (`~/.config/xtux/settings.toml`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    /// Folders with one folder per game (xTux installs, Windows `XboxGames` folders).
    pub library_dirs: Vec<PathBuf>,
    /// Wine binary used to start games; empty uses the Tux-Force Wine xTux installs (see `wine.rs`).
    pub wine: Option<String>,
    /// Wine prefix used for games without their own.
    pub prefix: Option<PathBuf>,
    /// Store market (for example `DE`); taken from the system language when not set.
    pub market: Option<String>,
    /// How PlayStation controllers reach games.
    pub controller: ControllerMode,
    /// Look of the launcher.
    pub ui: UiSettings,
    /// Settings of single games, by game folder.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub games: BTreeMap<String, GameSettings>,
}

/// How Wine hands PlayStation controllers (DualShock 4, DualSense) to games.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ControllerMode {
    /// As an Xbox gamepad, the only kind XInput and GameInput games know (through SDL).
    #[default]
    Xbox,
    /// As the raw PlayStation device (hidraw), for games with their own PlayStation support.
    Native,
}

impl ControllerMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Xbox => "xbox",
            Self::Native => "native",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "xbox" => Some(Self::Xbox),
            "native" => Some(Self::Native),
            _ => None,
        }
    }
}

/// Look of the launcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    /// `system`, `brand`, `anthracite` or `light`.
    pub theme: String,
    /// Dark theme used when following a dark system: `brand` or `anthracite`.
    pub dark_variant: String,
    /// `lime`, `emerald`, `cyan`, `violet` or `amber`.
    pub accent: String,
    /// Library view: `grid` or `list`.
    pub view: String,
    /// Load cover and hero images from the Microsoft Store catalog (sends the Store IDs of installed games).
    pub online_art: bool,
    /// Window background from game art: `dynamic` (follows the game under the mouse), `static` or `off`.
    pub backdrop: String,
    /// App icon: `mascot`, `mark`, `symbol` or `mono`.
    pub icon: String,
    /// Icon colours: `brand` or `accent`.
    pub icon_colour: String,
    /// Look games up on AreWeAntiCheatYet: its public list is downloaded from GitHub once a week (no game names
    /// are sent). Installed games are checked by their files regardless.
    pub anticheat_list: bool,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            dark_variant: "brand".into(),
            accent: "lime".into(),
            view: "grid".into(),
            online_art: true,
            backdrop: "dynamic".into(),
            icon: "mascot".into(),
            icon_colour: "brand".into(),
            anticheat_list: true,
        }
    }
}

/// Settings of one game; empty values fall back to the general settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GameSettings {
    /// Wine binary for this game.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wine: Option<String>,
    /// Wine prefix for this game.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<PathBuf>,
    /// Executable to start, package relative (for example `x64\Game.exe`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    /// Command line arguments for the game.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra environment variables for the game.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// How PlayStation controllers reach this game.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controller: Option<ControllerMode>,
}

/// The same folder gives the same key however it was written (`./Game`, `Game/`, through a link).
fn game_key(game_dir: &Path) -> String {
    std::fs::canonicalize(game_dir)
        .unwrap_or_else(|_| game_dir.to_path_buf())
        .to_string_lossy()
        .into_owned()
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

/// `XboxGames` folders of the Windows Xbox app on mounted drives (`/mnt`, `/media`, `/run/media/$USER`, the home folder).
pub fn detect_windows_libraries() -> Vec<PathBuf> {
    let user = std::env::var("USER").unwrap_or_default();
    let mut roots = vec![PathBuf::from("/mnt"), PathBuf::from("/media")];
    // Windows partitions mounted below the home folder, for example ~/winGames/XboxGames.
    roots.extend(home());
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
            // Hidden folders (caches, Wine prefixes) are not where libraries live.
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let candidate = entry.path().join("XboxGames");
            if candidate.is_dir() && !found.contains(&candidate) {
                if writable_by_others(&candidate) {
                    tracing::warn!(
                        "{} is writable by other users and not added automatically; add it to the settings if you trust it",
                        candidate.display()
                    );
                    continue;
                }
                found.push(candidate);
            }
        }
    }
    found
}

/// Whether users other than us (and root) could place games in `dir`.
fn writable_by_others(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = std::fs::metadata(dir) else {
            return true;
        };
        let me = rustix::process::getuid().as_raw();
        let owner_ok = meta.uid() == me || meta.uid() == 0;
        let world_writable = meta.mode() & 0o002 != 0;
        // Group write is fine on our own folder, not on one owned by root.
        let group_writable = meta.mode() & 0o020 != 0 && meta.uid() != me;
        !owner_ok || world_writable || group_writable
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        false
    }
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

    /// The own settings of the game in `game_dir` (empty when it has none).
    pub fn game(&self, game_dir: &Path) -> GameSettings {
        self.games
            .get(&game_key(game_dir))
            .cloned()
            .unwrap_or_default()
    }

    /// Stores the settings of a game; empty settings remove its entry.
    pub fn set_game(&mut self, game_dir: &Path, game: GameSettings) {
        if game == GameSettings::default() {
            self.games.remove(&game_key(game_dir));
        } else {
            self.games.insert(game_key(game_dir), game);
        }
    }

    /// The settings a game starts with: its own, then the general ones for Wine and the prefix.
    pub fn effective(&self, game_dir: &Path) -> GameSettings {
        let mut game = self.game(game_dir);
        game.wine = game.wine.or_else(|| self.wine.clone());
        // Without a Wine of its own choice, the user gets the Tux-Force Wine xTux installed.
        #[cfg(unix)]
        {
            game.wine = game.wine.or_else(|| {
                crate::wine::installed().map(|w| w.wine.to_string_lossy().into_owned())
            });
        }
        game.prefix = game.prefix.or_else(|| self.prefix.clone());
        game.controller = game.controller.or(Some(self.controller));
        game
    }

    pub fn save(&self) -> Result<()> {
        let file = path().ok_or("cannot determine the settings folder")?;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| Error::msg(e.to_string()))?;
        // Written next to it and renamed, so a crash never leaves half a file.
        let partial = file.with_extension("toml.partial");
        std::fs::write(&partial, text)?;
        std::fs::rename(&partial, &file)?;
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
            controller: ControllerMode::Native,
            ui: UiSettings::default(),
            games: BTreeMap::new(),
        };
        let text = toml::to_string_pretty(&settings).unwrap();
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), settings);
        assert_eq!(toml::from_str::<Settings>("").unwrap(), Settings::default());
    }

    #[test]
    fn game_settings_override_the_general_ones() {
        let mut settings = Settings {
            wine: Some("/opt/wine".into()),
            prefix: Some("/prefix".into()),
            ..Settings::default()
        };
        let game = Path::new("/games/LS25");
        settings.set_game(
            game,
            GameSettings {
                prefix: Some("/ls25-prefix".into()),
                args: vec!["-skipIntro".into()],
                env: BTreeMap::from([("DXVK_HUD".into(), "fps".into())]),
                ..GameSettings::default()
            },
        );
        let text = toml::to_string_pretty(&settings).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back, settings);
        let effective = back.effective(game);
        assert_eq!(effective.wine.as_deref(), Some("/opt/wine"));
        assert_eq!(effective.prefix, Some(PathBuf::from("/ls25-prefix")));
        assert_eq!(
            back.effective(Path::new("/other")).args,
            Vec::<String>::new()
        );

        assert_eq!(effective.controller, Some(ControllerMode::Xbox));
        settings.set_game(
            game,
            GameSettings {
                controller: Some(ControllerMode::Native),
                ..GameSettings::default()
            },
        );
        assert_eq!(
            settings.effective(game).controller,
            Some(ControllerMode::Native)
        );
        assert!(
            toml::to_string_pretty(&settings)
                .unwrap()
                .contains("controller = \"native\"")
        );

        settings.set_game(game, GameSettings::default());
        assert!(settings.games.is_empty());
    }
}
