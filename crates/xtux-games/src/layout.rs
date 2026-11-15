//! Game folder layouts.
//!
//! xTux installs games like the Windows Xbox app does, so one folder works for both:
//!
//! ```text
//! <game>/Content/...                      game files
//! <game>/<CONTENT-ID>                     package header (license, keys, file list), named by the content id
//! <game>/<CONTENT-ID>.xvi/.xvs/.xct/...   bookkeeping of the Windows Xbox app (only in its installs)
//! ```
//!
//! The older Xodus layout keeps the game files directly in the folder with the package header in
//! `.xodus-streaming.msixvc`; it is still read.

use std::path::{Path, PathBuf};

/// Package header file of the Xodus layout.
pub const XODUS_PACKAGE: &str = ".xodus-streaming.msixvc";
/// Folder with the game files in the Windows layout.
pub const CONTENT_DIR: &str = "Content";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameLayout {
    /// Folder the package's file paths are relative to.
    pub root: PathBuf,
    /// Package header file.
    pub package: PathBuf,
    /// Installed by the Windows Xbox app (its bookkeeping files are present).
    pub windows_app: bool,
}

/// Whether `name` looks like a content id (`8-4-4-4-12` hex digits).
pub fn is_content_id(name: &str) -> bool {
    let parts: Vec<&str> = name.split('-').collect();
    parts.len() == 5
        && parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12])
        && parts
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The package header of a Windows layout folder: the content id named file.
pub fn find_package(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
        let path = entry.path();
        let name = path.file_name()?.to_str()?;
        (is_content_id(name) && path.is_file()).then_some(path)
    })
}

/// The layout of a game folder, `None` when it is not a game folder.
pub fn detect(dir: &Path) -> Option<GameLayout> {
    let xodus = dir.join(XODUS_PACKAGE);
    if xodus.exists() {
        return Some(GameLayout {
            root: dir.to_path_buf(),
            package: xodus,
            windows_app: false,
        });
    }
    let content = dir.join(CONTENT_DIR);
    if !content.is_dir() {
        return None;
    }
    let package = find_package(dir)?;
    let windows_app = package.with_extension("xvi").exists()
        || dir
            .join(format!("{}.xvi", package.file_name()?.to_string_lossy()))
            .exists();
    Some(GameLayout {
        root: content,
        package,
        windows_app,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_ids() {
        assert!(is_content_id("FD1A3647-8CBF-439E-B7F9-B67AF942BA4A"));
        assert!(is_content_id("fd1a3647-8cbf-439e-b7f9-b67af942ba4a"));
        assert!(!is_content_id("FD1A3647-8CBF-439E-B7F9-B67AF942BA4A.xvi"));
        assert!(!is_content_id("Content"));
    }

    #[test]
    fn detects_both_layouts() {
        let root = tempfile::tempdir().unwrap();
        let windows = root.path().join("win");
        std::fs::create_dir_all(windows.join("Content")).unwrap();
        std::fs::write(windows.join("FD1A3647-8CBF-439E-B7F9-B67AF942BA4A"), b"").unwrap();
        let layout = detect(&windows).unwrap();
        assert_eq!(layout.root, windows.join("Content"));
        assert!(!layout.windows_app);
        std::fs::write(
            windows.join("FD1A3647-8CBF-439E-B7F9-B67AF942BA4A.xvi"),
            b"",
        )
        .unwrap();
        assert!(detect(&windows).unwrap().windows_app);

        let xodus = root.path().join("xodus");
        std::fs::create_dir_all(&xodus).unwrap();
        std::fs::write(xodus.join(XODUS_PACKAGE), b"").unwrap();
        assert_eq!(detect(&xodus).unwrap().root, xodus);

        assert!(detect(root.path()).is_none());
    }
}

/// Moves an install from the Xodus layout into the Windows layout, in place: game files into `Content/`, the
/// package header to its content id. Returns the new header path. Only renames, so it stays on the same drive.
pub async fn migrate_xodus(dir: &Path) -> crate::Result<PathBuf> {
    use crate::Error;

    let layout =
        detect(dir).ok_or_else(|| Error::msg(format!("{} is not a game folder", dir.display())))?;
    if layout.root != dir || layout.package != dir.join(XODUS_PACKAGE) {
        return Err(Error::msg(format!(
            "{} already uses the Windows layout",
            dir.display()
        )));
    }
    let mut file = tokio::fs::File::open(&layout.package).await?;
    let xvd = msixvc::xvd::XvdFile::parse(&mut file)
        .await
        .map_err(|e| Error::msg(format!("cannot read the package header: {e}")))?;
    let id = xvd.content_id().hyphenated().to_string().to_uppercase();

    let content = dir.join(CONTENT_DIR);
    if content.exists() {
        return Err(Error::msg(format!("{} exists already", content.display())));
    }
    std::fs::create_dir(&content)?;
    for entry in std::fs::read_dir(dir)?.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // The header, the streaming cache and the new folder stay where they are.
        if name_str == XODUS_PACKAGE
            || name_str.starts_with(".xodus-streaming")
            || name_str == CONTENT_DIR
        {
            continue;
        }
        std::fs::rename(entry.path(), content.join(&name))?;
    }
    let package = dir.join(&id);
    std::fs::rename(&layout.package, &package)?;
    Ok(package)
}
