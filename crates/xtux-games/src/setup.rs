//! What a new user still needs before the first game starts, shared by the setup of the CLI and the launcher.
//!
//! Signing in is checked by the clients (the account lives in xtux-service); this covers the local parts:
//! a Wine to start games with, a prepared prefix and the library folders.

use std::path::{Path, PathBuf};

use crate::settings::Settings;

/// The local state the setup looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupState {
    /// The Wine games start with, when it exists: from the settings, else the one xTux installed.
    pub wine: Option<String>,
    /// The Tux-Force Wine xTux manages, if installed.
    pub managed_wine: Option<crate::wine::InstalledWine>,
    /// Set in the settings but not found.
    pub missing_wine: Option<String>,
    pub prefix: PathBuf,
    /// `prefix init` ran for it (DXVK, vkd3d-proton, certificates, controller mode).
    pub prefix_ready: bool,
    pub libraries: Vec<PathBuf>,
}

impl SetupState {
    /// Everything local is in place for the first start.
    pub fn complete(&self) -> bool {
        self.wine.is_some() && self.prefix_ready
    }
}

pub fn state(settings: &Settings) -> SetupState {
    let managed_wine = crate::wine::installed();
    let (wine, missing_wine) = match &settings.wine {
        Some(wine) if wine_exists(wine) => (Some(wine.clone()), None),
        Some(wine) => (None, Some(wine.clone())),
        None => (
            managed_wine
                .as_ref()
                .map(|w| w.wine.to_string_lossy().into_owned()),
            None,
        ),
    };
    let prefix = settings
        .prefix
        .clone()
        .or_else(|| crate::prefix::resolve_prefix(None))
        .unwrap_or_else(|| PathBuf::from("xtux-prefix"));
    SetupState {
        wine,
        managed_wine,
        missing_wine,
        prefix_ready: prefix_ready(&prefix),
        prefix,
        libraries: settings.library_dirs.clone(),
    }
}

/// A path to a file, or a command found in `PATH`.
pub fn wine_exists(wine: &str) -> bool {
    if wine.contains('/') {
        return Path::new(wine).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(wine).is_file()))
}

fn prefix_ready(prefix: &Path) -> bool {
    prefix.join("drive_c").is_dir() && prefix.join(crate::prefix::STATE_FILE).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_wine_and_a_prepared_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let wine = dir.path().join("wine");
        std::fs::write(&wine, "").unwrap();
        let prefix = dir.path().join("prefix");
        let settings = Settings {
            wine: Some(wine.to_string_lossy().into_owned()),
            prefix: Some(prefix.clone()),
            ..Settings::default()
        };
        let state = state(&settings);
        assert_eq!(state.wine.as_deref(), wine.to_str());
        assert!(!state.prefix_ready);
        assert!(!state.complete());

        std::fs::create_dir_all(prefix.join("drive_c")).unwrap();
        std::fs::write(prefix.join(crate::prefix::STATE_FILE), "{}").unwrap();
        assert!(super::state(&settings).complete());

        let missing = Settings {
            wine: Some("/nonexistent/wine".into()),
            prefix: Some(prefix),
            ..Settings::default()
        };
        let state = super::state(&missing);
        assert_eq!(state.wine, None);
        assert_eq!(state.missing_wine.as_deref(), Some("/nonexistent/wine"));
        assert!(!state.complete());
    }

    #[test]
    fn finds_commands_in_path() {
        assert!(wine_exists("sh"));
        assert!(!wine_exists("certainly-not-a-command-xtux"));
    }
}
