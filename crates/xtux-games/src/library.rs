//! The local game library: games found in the configured library folders.
//!
//! A library folder holds one folder per game, either streamed or downloaded by xTux (with a `.xodus-streaming.msixvc`
//! package file) or installed by the Windows Xbox app (with `Content/` and a package file named by a GUID).
//!
//! Add-ons (DLC) use the same layout in a folder of their own. Their `MicrosoftGame.config` names the Store IDs of
//! the games they belong to in `<AllowedProducts>`; that is how they are told apart from games and matched to them.

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
    /// Anti-cheat found in the game folder.
    pub anticheat: Option<crate::anticheat::AntiCheat>,
    /// For an add-on: Store IDs of the games it belongs to (`<AllowedProducts>`); empty for games.
    #[serde(default)]
    pub allowed_products: Vec<String>,
    /// `Version` of `<Identity>`, for example `1.0.2.0`.
    #[serde(default)]
    pub version: Option<String>,
    /// `PublisherDisplayName` of `<ShellVisuals>`.
    #[serde(default)]
    pub publisher: Option<String>,
}

impl Game {
    pub fn is_addon(&self) -> bool {
        !self.allowed_products.is_empty()
    }
}

/// Finds the games in `dirs` (without add-ons), sorted by name. Unreadable folders are skipped.
pub fn scan(dirs: &[PathBuf]) -> Vec<Game> {
    let mut games = scan_all(dirs);
    games.retain(|g| !g.is_addon());
    games
}

/// Finds the add-ons in `dirs`, sorted by name.
pub fn scan_addons(dirs: &[PathBuf]) -> Vec<Game> {
    let mut addons = scan_all(dirs);
    addons.retain(Game::is_addon);
    addons
}

/// The installed add-ons of the game with these Store IDs (its own and the products it belongs to).
pub fn addons_of(dirs: &[PathBuf], store_ids: &[String]) -> Vec<Game> {
    let mut addons = scan_addons(dirs);
    addons.retain(|a| {
        a.allowed_products
            .iter()
            .any(|p| store_ids.iter().any(|s| s.eq_ignore_ascii_case(p)))
    });
    addons
}

/// Games and add-ons in `dirs`, sorted by name.
pub fn scan_all(dirs: &[PathBuf]) -> Vec<Game> {
    let mut games = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            tracing::debug!("library folder {} is not readable", dir.display());
            continue;
        };
        for entry in entries.flatten() {
            // Leftovers of an interrupted delete are not games any more.
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(".xtux-deleting-")
            {
                continue;
            }
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
    let anticheat = crate::anticheat::detect(&content);
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
        anticheat,
        allowed_products: config
            .as_deref()
            .and_then(|c| section(c, "AllowedProducts"))
            .map(|s| {
                elements(s, "AllowedProduct")
                    .into_iter()
                    .filter(|id| crate::art::is_store_id(id))
                    .collect()
            })
            .unwrap_or_default(),
        version: config
            .as_deref()
            .and_then(|c| attribute(c, "Identity", "Version"))
            .filter(|v| v.len() <= 32 && v.chars().all(|c| c.is_ascii_digit() || c == '.')),
        publisher: config
            .as_deref()
            .and_then(|c| attribute(c, "ShellVisuals", "PublisherDisplayName"))
            .map(|n| {
                n.chars()
                    .filter(|c| !c.is_control())
                    .take(200)
                    .collect::<String>()
            })
            .filter(|n| !n.is_empty() && !n.starts_with("ms-resource:")),
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

/// Inner text of the first `<tag>...</tag>`, untrimmed.
fn section<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let start = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    Some(&xml[start..end])
}

/// Texts of all `<tag>...</tag>`.
fn elements(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut found = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        rest = &rest[start + open.len()..];
        let Some(end) = rest.find(&close) else { break };
        found.push(rest[..end].trim().to_string());
        rest = &rest[end + close.len()..];
    }
    found
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

    const ADDON_CONFIG: &str = r#"<Game configVersion="1"><Identity Name="Dlc" Publisher="CN=X" Version="1.2.0.0"/>
        <ShellVisuals DefaultDisplayName="Rally" PublisherDisplayName="Studio"/><StoreId>9PCR7TSM1S96</StoreId>
        <AllowedProducts>
          <AllowedProduct>9NNX1VVR3KNQ</AllowedProduct>
          <AllowedProduct> 9NKX70BBCDRN </AllowedProduct>
          <AllowedProduct>../bad</AllowedProduct>
        </AllowedProducts></Game>"#;

    #[test]
    fn tells_addons_from_games() {
        let root = tempfile::tempdir().unwrap();
        for (name, config) in [("Game A", CONFIG), ("Rally", ADDON_CONFIG)] {
            let dir = root.path().join(name);
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join(crate::run::PACKAGE_FILE), b"").unwrap();
            std::fs::write(dir.join("MicrosoftGame.config"), config).unwrap();
        }
        let dirs = [root.path().to_path_buf()];
        assert_eq!(scan(&dirs).len(), 1);
        let addons = scan_addons(&dirs);
        assert_eq!(addons.len(), 1);
        assert_eq!(
            addons[0].allowed_products,
            vec!["9NNX1VVR3KNQ", "9NKX70BBCDRN"]
        );
        assert_eq!(addons[0].version.as_deref(), Some("1.2.0.0"));
        assert_eq!(addons[0].publisher.as_deref(), Some("Studio"));
        assert_eq!(addons_of(&dirs, &["9nkx70bbcdrn".into()]).len(), 1);
        assert!(addons_of(&dirs, &["9NXN8GF8N9ZQ".into()]).is_empty());
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
