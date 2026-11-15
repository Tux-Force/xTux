//! The local game library: games found in the configured library folders.
//!
//! A library folder holds one folder per game, either streamed or downloaded by xTux (with a `.xodus-streaming.msixvc`
//! package file) or installed by the Windows Xbox app (with `Content/` and a package file named by a GUID).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GameKind {
    /// Streamed or downloaded by xTux.
    Installed,
    /// Installed by the Windows Xbox app, run through a link folder.
    Windows,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Game {
    /// Display name from `MicrosoftGame.config`, or the folder name.
    pub name: String,
    /// Game folder; what `run` takes as source.
    pub path: PathBuf,
    pub kind: GameKind,
    pub title_id: Option<String>,
    pub store_id: Option<String>,
}

/// Finds the games in `dirs`, sorted by name. Unreadable folders are skipped.
pub fn scan(dirs: &[PathBuf]) -> Vec<Game> {
    let mut games = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            tracing::debug!("library folder {} is not readable", dir.display());
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(game) = inspect(&path) {
                games.push(game);
            }
        }
    }
    games.sort_by_key(|g| g.name.to_lowercase());
    games.dedup_by(|a, b| a.path == b.path);
    games
}

/// The game in `path`, if it is one.
pub fn inspect(path: &Path) -> Option<Game> {
    if !path.is_dir() {
        return None;
    }
    let layout = crate::layout::detect(path)?;
    let kind = if layout.windows_app {
        GameKind::Windows
    } else {
        GameKind::Installed
    };
    let content = layout.root;
    let config = read_config(&content);
    let folder_name = path.file_name()?.to_string_lossy().into_owned();
    let name = config
        .as_deref()
        .and_then(|c| attribute(c, "ShellVisuals", "DefaultDisplayName"))
        .map(|n| n.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|n| !n.is_empty() && !n.starts_with("ms-resource:"))
        .unwrap_or(folder_name);
    Some(Game {
        name,
        path: path.to_path_buf(),
        kind,
        title_id: config.as_deref().and_then(|c| element(c, "TitleId")),
        store_id: config.as_deref().and_then(|c| element(c, "StoreId")),
    })
}

/// Largest `MicrosoftGame.config` read; real ones are a few kilobytes.
const MAX_CONFIG_SIZE: u64 = 1024 * 1024;

/// `MicrosoftGame.config` of a game folder; the file name's case differs between games.
pub(crate) fn read_config(dir: &Path) -> Option<String> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case("MicrosoftGame.config")
        {
            // A regular file of sane size only: library folders may be shared with other users.
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).ok()?;
            if !meta.is_file() || meta.len() > MAX_CONFIG_SIZE {
                return None;
            }
            return std::fs::read_to_string(path).ok();
        }
    }
    None
}

/// Text of the first `<tag>...</tag>`.
fn element(xml: &str, tag: &str) -> Option<String> {
    let start = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    Some(xml[start..end].trim().to_string())
}

/// Value of `attr` on the first `<tag ...>`.
pub(crate) fn attribute(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let start = xml.find(&format!("<{tag}"))?;
    let end = xml[start..].find('>')? + start;
    let head = &xml[start..end];
    let key = format!("{attr}=\"");
    let value_start = head.find(&key)? + key.len();
    let value_end = head[value_start..].find('"')? + value_start;
    Some(head[value_start..value_end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"<Game><Identity Name="X" /><TitleId>604cc91b</TitleId><StoreId>9NXN8GF8N9ZQ</StoreId>
        <ShellVisuals DefaultDisplayName="Farming Simulator 25" StoreLogo="a.png" /></Game>"#;

    #[test]
    fn reads_config_fields() {
        assert_eq!(element(CONFIG, "TitleId").as_deref(), Some("604cc91b"));
        assert_eq!(element(CONFIG, "StoreId").as_deref(), Some("9NXN8GF8N9ZQ"));
        assert_eq!(
            attribute(CONFIG, "ShellVisuals", "DefaultDisplayName").as_deref(),
            Some("Farming Simulator 25")
        );
        assert_eq!(element(CONFIG, "Missing"), None);
    }

    #[test]
    fn finds_streamed_games() {
        let root = tempfile::tempdir().unwrap();
        let game = root.path().join("Game A");
        std::fs::create_dir(&game).unwrap();
        std::fs::write(game.join(crate::run::PACKAGE_FILE), b"").unwrap();
        std::fs::write(game.join("MicrosoftGame.config"), CONFIG).unwrap();
        std::fs::create_dir(root.path().join("not a game")).unwrap();

        let games = scan(&[root.path().to_path_buf()]);
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].name, "Farming Simulator 25");
        assert_eq!(games[0].kind, GameKind::Installed);
        assert_eq!(games[0].title_id.as_deref(), Some("604cc91b"));
    }
}
