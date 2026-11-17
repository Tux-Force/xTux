//! Version checks and update targets for installed games and add-ons.
use std::io::Read;
use std::path::{Path, PathBuf};

use msixvc::models::xvd::XvdHeader;
use msixvc_common::parse::BinaryTryParse;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

pub(crate) const INCOMPLETE: &str = ".xtux-update-incomplete";

pub(crate) fn ensure_ready(path: &Path) -> Result<()> {
    if path.join(INCOMPLETE).exists() {
        return Err(Error::msg(
            "An update is incomplete; finish or retry it before starting this game",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Update {
    pub path: PathBuf,
    pub name: String,
    pub installed_version: String,
    pub latest_version: Option<String>,
    /// available | current | newer | failed
    pub state: String,
    pub can_update: bool,
    pub message: String,
}

pub struct Installed {
    pub path: PathBuf,
    pub name: String,
    pub product_id: String,
    pub content_id: String,
    pub version: String,
    pub windows_app: bool,
}

/// Validate the actual library folder and read the installed package's own version.
pub fn installed(path: &Path, libraries: &[PathBuf]) -> Result<Installed> {
    let path = crate::uninstall::check(path, libraries)?;
    let layout = crate::layout::detect(&path).ok_or("not an installed package")?;
    let config = crate::library::read_config(&layout.root).ok_or("game configuration missing")?;
    let product_id = crate::library::element(&config, "StoreId")
        .filter(|id| crate::art::is_store_id(id))
        .ok_or("installed package has no valid Store ID")?;
    let mut bytes = XvdHeader::buffer();
    std::fs::File::open(&layout.package)?.read_exact(&mut bytes)?;
    let header = XvdHeader::try_from_array(&bytes).map_err(Error::msg)?;
    Ok(Installed {
        name: crate::library::attribute(&config, "ShellVisuals", "DefaultDisplayName")
            .filter(|n| !n.starts_with("ms-resource:"))
            .unwrap_or_else(|| {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }),
        path,
        product_id,
        content_id: header.vduid.to_string(),
        version: header.package_version.to_string(),
        windows_app: layout.windows_app,
    })
}

/// Both dotted GDK versions and the packed four-component hexadecimal API representation.
fn version(text: &str) -> Option<[u16; 4]> {
    // GetBasePackage appends the build GUID, e.g. 1.9.24.0.<guid>.
    if let Some((number, build)) = text.rsplit_once('.') {
        if crate::layout::is_content_id(build) {
            return version(number);
        }
    }
    if !text.contains('.') {
        let packed = if let Some(hex) = text.strip_prefix("0x") {
            u64::from_str_radix(hex, 16).ok()?
        } else if text.len() == 16
            && (text.starts_with('0') || text.bytes().any(|c| c.is_ascii_alphabetic()))
        {
            u64::from_str_radix(text, 16).ok()?
        } else {
            text.parse::<u64>().ok()?
        };
        return Some([
            (packed >> 48) as u16,
            (packed >> 32) as u16,
            (packed >> 16) as u16,
            packed as u16,
        ]);
    }
    let parts: Vec<u16> = text
        .split('.')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    parts.try_into().ok()
}

pub async fn check(
    client: &reqwest::Client,
    authorization: &str,
    path: &Path,
    libraries: &[PathBuf],
) -> Result<Update> {
    let local = installed(path, libraries)?;
    let mut result = Update {
        path: local.path,
        name: local.name,
        installed_version: local.version.clone(),
        latest_version: None,
        state: "failed".into(),
        can_update: false,
        message: String::new(),
    };
    match tokio::time::timeout(
        std::time::Duration::from_secs(60),
        crate::package::get_packages_authorized(client, authorization, local.content_id.clone()),
    )
    .await
    .map_err(|_| Error::msg("Update check timed out"))?
    {
        Ok(remote) => {
            let Some(latest) = version(&remote.version) else {
                result.message = format!(
                    "Unrecognized server package version: {}",
                    remote.version.chars().take(40).collect::<String>()
                );
                return Ok(result);
            };
            let current =
                version(&local.version).ok_or("unrecognized installed package version")?;
            let incomplete = result.path.join(INCOMPLETE).exists();
            let needs_update = latest > current || (incomplete && latest == current);
            result.latest_version = Some(
                latest
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join("."),
            );
            result.state = match latest.cmp(&current) {
                std::cmp::Ordering::Greater => "available",
                std::cmp::Ordering::Equal => "current",
                std::cmp::Ordering::Less => "newer",
            }
            .into();
            result.can_update = needs_update
                && !local.windows_app
                && !remote.update_predownload
                && remote.content_id.eq_ignore_ascii_case(&local.content_id)
                && remote
                    .package_files
                    .iter()
                    .any(|p| p.file_name.ends_with(".msixvc") && p.delta_version_id.is_none());
            if incomplete && latest >= current {
                result.state = "available".into();
            }
            result.message = if needs_update && local.windows_app {
                "Update this Xbox app installation in Windows"
            } else if needs_update && remote.update_predownload {
                "Update is offered for predownload; not yet ready to install"
            } else if needs_update && !result.can_update {
                "No supported full package available"
            } else if incomplete && result.can_update {
                "Finish the incomplete update before playing"
            } else if latest > current {
                "Update available"
            } else if latest == current {
                "Up to date"
            } else {
                "Installed version is newer than the offered package"
            }
            .into();
        }
        Err(err) => result.message = err.to_string(),
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interrupted_updates_block_launches_until_completed() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(ensure_ready(tmp.path()).is_ok());
        std::fs::write(tmp.path().join(INCOMPLETE), "package:first").unwrap();
        assert!(ensure_ready(tmp.path()).is_err());
        std::fs::remove_file(tmp.path().join(INCOMPLETE)).unwrap();
        assert!(ensure_ready(tmp.path()).is_ok());
    }
    #[test]
    fn compares_packed_and_dotted_versions_numerically() {
        assert_eq!(version("0001000900180000"), version("1.9.24.0"));
        assert_eq!(
            version("1.9.24.0.11111111-2222-3333-4444-555555555555"),
            version("1.9.24.0")
        );
        assert_eq!(
            version(&0x0001000900180000u64.to_string()),
            version("1.9.24.0")
        );
        assert!(version("1.10.0.0") > version("1.9.24.0"));
        for invalid in ["", "1.2.3", "1.2.3.65536", "abcdefghijklmnop"] {
            assert_eq!(version(invalid), None);
        }
    }
}
