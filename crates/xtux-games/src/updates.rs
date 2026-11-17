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
    if crate::layout::detect(path).is_some_and(|l| windows_update_unresolved(path, &l)) {
        return Err(Error::msg(WINDOWS_PENDING));
    }
    Ok(())
}

const WINDOWS_PENDING: &str = "The Windows Xbox app started an update of this game and did not finish it: files are still missing. Finish the update under Updates in xTux, or let the Xbox app finish it in Windows";

/// Notes that xTux finished an update of an Xbox app install, so a stale streaming state of the Xbox app does not
/// block it. Holds the package header and the Xbox app's state as they were then; once either changes the state
/// of the Xbox app counts again.
const WINDOWS_FINISHED: &str = ".xtux-windows-update-finished";

fn xvs_path(package: &Path) -> PathBuf {
    let mut name = package.as_os_str().to_owned();
    name.push(".xvs");
    PathBuf::from(name)
}

fn windows_stamp(package: &Path) -> Option<String> {
    use sha1::{Digest, Sha1};
    let mut header = [0u8; 4096];
    std::fs::File::open(package)
        .ok()?
        .read_exact(&mut header)
        .ok()?;
    let state = std::fs::read(xvs_path(package)).unwrap_or_default();
    Some(format!(
        "package:{:x}\nxvs:{:x}\n",
        Sha1::digest(header),
        Sha1::digest(state)
    ))
}

/// Records a finished update of an Xbox app install (see [`WINDOWS_FINISHED`]).
pub(crate) fn note_windows_update_finished(dir: &Path, package: &Path) -> Result<()> {
    let stamp = windows_stamp(package).ok_or("cannot read the package header")?;
    std::fs::write(dir.join(WINDOWS_FINISHED), stamp)?;
    Ok(())
}

/// Whether `dir` is an Xbox app install that the Xbox app left in the middle of an update which xTux did not
/// finish since. Its package header then does not describe the files on disk.
pub(crate) fn windows_update_unresolved(dir: &Path, layout: &crate::layout::GameLayout) -> bool {
    if !layout.windows_app || !windows_update_pending(&layout.package) {
        return false;
    }
    let finished = std::fs::read_to_string(dir.join(WINDOWS_FINISHED)).ok();
    finished.is_none() || finished != windows_stamp(&layout.package)
}

/// Whether the Windows Xbox app left this install in the middle of a download or an update. Its `.xvs` file
/// (UTF-16 JSON next to the package header) records the streaming state. While it is not completed, changed
/// files are empty placeholders and the package header itself may be only partly written.
pub(crate) fn windows_update_pending(package: &Path) -> bool {
    let Ok(raw) = std::fs::read(xvs_path(package)) else {
        return false;
    };
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let text = String::from_utf16_lossy(&units);
    let Some(end) = text.rfind('}') else {
        return false;
    };
    let Ok(state) =
        serde_json::from_str::<serde_json::Value>(text[..=end].trim_start_matches('\u{feff}'))
    else {
        return false;
    };
    let status = &state["Status"];
    let package = &status["Progress"]["Package"];
    match (
        status["Operation"].as_str(),
        package["StreamedBytes"].as_u64(),
        package["TotalBytes"].as_u64(),
    ) {
        (Some(operation), Some(streamed), Some(total)) => {
            operation != "Completed" && streamed < total
        }
        _ => false,
    }
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
            let windows_pending = crate::layout::detect(&result.path)
                .is_some_and(|l| windows_update_unresolved(&result.path, &l));
            let needs_update =
                latest > current || ((incomplete || windows_pending) && latest == current);
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
                && !remote.update_predownload
                && remote.content_id.eq_ignore_ascii_case(&local.content_id)
                && remote
                    .package_files
                    .iter()
                    .any(|p| p.file_name.ends_with(".msixvc") && p.delta_version_id.is_none());
            if incomplete && latest >= current {
                result.state = "available".into();
            }
            if windows_pending && latest >= current {
                // The header already carries the new version although its files are not there yet.
                result.state = "available".into();
            }
            result.message = if windows_pending && result.can_update {
                "The Windows Xbox app did not finish this update; xTux can finish it"
            } else if windows_pending {
                WINDOWS_PENDING
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
    fn unfinished_windows_updates_block_launches() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "FD1A3647-8CBF-439E-B7F9-B67AF942BA4A";
        std::fs::create_dir(tmp.path().join("Content")).unwrap();
        std::fs::write(tmp.path().join(id), b"").unwrap();
        std::fs::write(tmp.path().join(format!("{id}.xvi")), b"").unwrap();
        let xvs = |operation: &str, streamed: u64| {
            let json = format!(
                r#"{{"Status":{{"Operation":"{operation}","Progress":{{"Package":{{"TotalBytes":100,"StreamedBytes":{streamed}}}}}}}}}"#
            );
            let mut bytes: Vec<u8> = json.encode_utf16().flat_map(u16::to_le_bytes).collect();
            bytes.extend([0; 6]);
            std::fs::write(tmp.path().join(format!("{id}.xvs")), bytes).unwrap();
        };
        assert!(ensure_ready(tmp.path()).is_ok());
        xvs("Completed", 100);
        assert!(ensure_ready(tmp.path()).is_ok());
        xvs("Stopped", 0);
        let error = ensure_ready(tmp.path()).unwrap_err().to_string();
        assert!(error.contains("Xbox app"), "{error}");
        std::fs::write(tmp.path().join(format!("{id}.xvs")), b"not json").unwrap();
        assert!(ensure_ready(tmp.path()).is_ok());
    }
    #[test]
    fn windows_updates_finished_by_xtux_allow_launches_until_windows_changes_them() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "FD1A3647-8CBF-439E-B7F9-B67AF942BA4A";
        let package = tmp.path().join(id);
        std::fs::create_dir(tmp.path().join("Content")).unwrap();
        std::fs::write(&package, [1u8; 4096]).unwrap();
        std::fs::write(tmp.path().join(format!("{id}.xvi")), b"").unwrap();
        let xvs = |streamed: u64| {
            let json = format!(
                r#"{{"Status":{{"Operation":"Stopped","Progress":{{"Package":{{"TotalBytes":100,"StreamedBytes":{streamed}}}}}}}}}"#
            );
            let bytes: Vec<u8> = json.encode_utf16().flat_map(u16::to_le_bytes).collect();
            std::fs::write(tmp.path().join(format!("{id}.xvs")), bytes).unwrap();
        };
        xvs(0);
        let layout = crate::layout::detect(tmp.path()).unwrap();
        assert!(windows_update_unresolved(tmp.path(), &layout));
        assert!(ensure_ready(tmp.path()).is_err());

        note_windows_update_finished(tmp.path(), &package).unwrap();
        assert!(!windows_update_unresolved(tmp.path(), &layout));
        assert!(ensure_ready(tmp.path()).is_ok());

        // The Xbox app continued its update in Windows and stopped again.
        xvs(50);
        assert!(ensure_ready(tmp.path()).is_err());
        note_windows_update_finished(tmp.path(), &package).unwrap();
        assert!(ensure_ready(tmp.path()).is_ok());
        // A new package header (the next update) counts as unfinished again.
        std::fs::write(&package, [2u8; 4096]).unwrap();
        assert!(ensure_ready(tmp.path()).is_err());
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
