//! Known anti-cheat systems, found by their files in a game folder.
//!
//! Kernel anti-cheats load a Windows driver, which Wine cannot do: those games only run on Windows. Others work on
//! Linux only when the game's developer allows it, so a game may or may not start.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AntiCheat {
    /// Name of the anti-cheat, for example `Ricochet (Activision)`.
    pub name: String,
    /// A kernel driver Wine cannot load: the game runs on Windows only.
    pub blocks: bool,
}

/// File or folder names (lower case) in the game's content folder, with the anti-cheat they belong to.
const KNOWN: &[(&str, &str, bool)] = &[
    ("randgrid.sys", "Ricochet (Activision)", true),
    ("vgk.sys", "Vanguard (Riot)", true),
    ("eaanticheat.gameservice.exe", "EA AntiCheat", true),
    ("eaanticheat.installer.exe", "EA AntiCheat", true),
    ("denuvo-anti-cheat.sys", "Denuvo Anti-Cheat", true),
    ("gameguard", "nProtect GameGuard", true),
    ("xigncode", "XIGNCODE3", true),
    ("easyanticheat", "Easy Anti-Cheat", false),
    ("easyanticheat_eos_setup.exe", "Easy Anti-Cheat", false),
    ("battleye", "BattlEye", false),
    ("beservice.exe", "BattlEye", false),
];

/// The anti-cheat of the game whose content folder is `content`, the blocking one first.
pub fn detect(content: &Path) -> Option<AntiCheat> {
    let names: Vec<String> = std::fs::read_dir(content)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_lowercase())
        .collect();
    let mut found: Vec<&(&str, &str, bool)> = KNOWN
        .iter()
        .filter(|(file, _, _)| names.iter().any(|n| n == file))
        .collect();
    found.sort_by_key(|(_, _, blocks)| !blocks);
    found.first().map(|(_, name, blocks)| AntiCheat {
        name: name.to_string(),
        blocks: *blocks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_kernel_anti_cheat_first() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect(dir.path()), None);
        std::fs::create_dir(dir.path().join("EasyAntiCheat")).unwrap();
        assert_eq!(detect(dir.path()).unwrap().name, "Easy Anti-Cheat");
        assert!(!detect(dir.path()).unwrap().blocks);
        std::fs::write(dir.path().join("Randgrid.sys"), b"").unwrap();
        let found = detect(dir.path()).unwrap();
        assert_eq!(found.name, "Ricochet (Activision)");
        assert!(found.blocks);
    }
}
