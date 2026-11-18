//! The GameInput redistributable for games that ship `GameInputRedist.msi` (Minecraft for Windows).
//!
//! Such games load `GameInputRedist.dll` from the `RedistDir` the installer records, and show "Your game is missing
//! a required component" without it. The MSI cannot be installed with `msiexec` under a 64-bit-only Wine (its WiX
//! custom actions are 32-bit), so xTux installs what it contains itself:
//!
//! - `GameInputRedist.dll` and `GameInputRedistService.exe` into `C:\Program Files\Microsoft GameInput\x64`,
//! - `RedistDir` under `HKLM\SOFTWARE\Microsoft\GameInput`, in both registry views (games read the 32-bit one),
//! - the COM proxy of the DLL (`regsvr32`) and the `GameInputRedistService` service, started automatically.
//!
//! The service needs the WNF and stubless proxy exports of the Tux-Force Wine build.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use serde_json::json;
use sha1::{Digest, Sha1};
use tokio::process::Command;

use crate::prefix::STATE_FILE;
use crate::{Error, Result};

const INSTALL_DIR: &str = r"C:\Program Files\Microsoft GameInput\x64\";
const SERVICE: &str = "GameInputRedistService";
const DLL: &str = "GameInputRedist.dll";
const SERVICE_EXE: &str = "GameInputRedistService.exe";

/// The redistributable a game ships: `GameInputRedist.msi` in its root or its `Installers` folder.
pub fn shipped_msi(game_root: &Path) -> Option<PathBuf> {
    [game_root.join("Installers"), game_root.to_path_buf()]
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flat_map(|entries| entries.flatten())
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case("GameInputRedist.msi")
        })
        .map(|entry| entry.path())
}

/// Installs the redistributable `game_root` ships into `prefix`, unless this one is installed already.
/// Returns whether anything was installed.
pub async fn ensure(wine: &str, prefix: &Path, game_root: &Path) -> Result<bool> {
    let Some(msi) = shipped_msi(game_root) else {
        return Ok(false);
    };
    let data = tokio::fs::read(&msi).await?;
    let digest: String = Sha1::digest(&data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let state_file = prefix.join(STATE_FILE);
    let mut state = std::fs::read_to_string(&state_file)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .filter(|state| state.is_object())
        .unwrap_or_else(|| json!({}));
    let dir = prefix.join("drive_c/Program Files/Microsoft GameInput/x64");
    if state["gameinput"].as_str() == Some(digest.as_str()) && dir.join(DLL).is_file() {
        return Ok(false);
    }

    let files = tokio::task::spawn_blocking(move || extract(&data))
        .await
        .map_err(|e| Error::msg(e.to_string()))??;
    std::fs::create_dir_all(&dir)?;
    for (name, bytes) in [(DLL, &files.dll), (SERVICE_EXE, &files.service)] {
        // Renamed into place: a running service keeps its mapping of the old file.
        let partial = dir.join(format!("{name}.partial"));
        std::fs::write(&partial, bytes)?;
        std::fs::rename(&partial, dir.join(name))?;
    }

    let reg = registry_text();
    let file = tempfile::Builder::new().suffix(".reg").tempfile()?;
    std::fs::write(file.path(), reg)?;
    let reg_path = file.path().to_string_lossy().to_string();
    wine_cmd(wine, prefix, &["reg", "import", &reg_path]).await?;
    let dll = format!("{INSTALL_DIR}{DLL}");
    wine_cmd(wine, prefix, &["regsvr32", "/s", &dll]).await?;
    if wine_cmd(wine, prefix, &["sc", "query", SERVICE])
        .await
        .is_err()
    {
        let bin_path = format!("\"{INSTALL_DIR}{SERVICE_EXE}\"");
        wine_cmd(
            wine,
            prefix,
            &[
                "sc", "create", SERVICE, "binPath=", &bin_path, "start=", "auto",
            ],
        )
        .await?;
    }
    // Already running after an update or a previous start: not an error.
    let _ = wine_cmd(wine, prefix, &["sc", "start", SERVICE]).await;

    state["gameinput"] = json!(digest);
    let text = serde_json::to_string_pretty(&state).map_err(|e| Error::msg(e.to_string()))?;
    std::fs::write(&state_file, text)?;
    Ok(true)
}

fn registry_text() -> String {
    let dir = INSTALL_DIR.replace('\\', "\\\\");
    let mut reg = String::from("Windows Registry Editor Version 5.00\r\n\r\n");
    for key in ["SOFTWARE", r"SOFTWARE\Wow6432Node"] {
        reg.push_str(&format!(
            "[HKEY_LOCAL_MACHINE\\{key}\\Microsoft\\GameInput]\r\n\"RedistDir\"=\"{dir}\"\r\n\r\n"
        ));
    }
    reg
}

async fn wine_cmd(wine: &str, prefix: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new(wine)
        .args(args)
        .env("WINEPREFIX", prefix)
        .env("WINEDEBUG", "-all")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| Error::msg(format!("could not start {wine}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::msg(format!(
            "`wine {}` failed with {status}",
            args.join(" ")
        )))
    }
}

#[derive(Debug)]
struct RedistFiles {
    dll: Vec<u8>,
    service: Vec<u8>,
}

/// The x64 runtime DLL and service from the MSI's embedded cabinets. The cabinet names files by MSI keys, so they
/// are told apart by their version resource; of the DLLs named `GameInputRedist.dll`, the runtime is the one
/// exporting `GameInputInitialize` (the others are inbox `GameInput.dll` replacements).
fn extract(msi: &[u8]) -> Result<RedistFiles> {
    let mut package = cfb::CompoundFile::open(Cursor::new(msi))
        .map_err(|e| Error::msg(format!("not an MSI: {e}")))?;
    let streams: Vec<PathBuf> = package
        .walk()
        .filter(|entry| entry.is_stream())
        .map(|entry| entry.path().to_path_buf())
        .collect();
    let mut dll: Option<Vec<u8>> = None;
    let mut service = None;
    for path in streams {
        let mut data = Vec::new();
        package.open_stream(&path)?.read_to_end(&mut data)?;
        if !data.starts_with(b"MSCF") {
            continue;
        }
        let mut cabinet = cab::Cabinet::new(Cursor::new(data))?;
        let names: Vec<String> = cabinet
            .folder_entries()
            .flat_map(|folder| folder.file_entries())
            .map(|file| file.name().to_string())
            .collect();
        for name in names {
            let mut bytes = Vec::new();
            cabinet.read_file(&name)?.read_to_end(&mut bytes)?;
            if pe_machine(&bytes) != Some(0x8664) {
                continue;
            }
            match original_filename(&bytes).as_deref() {
                Some(n) if n.eq_ignore_ascii_case(SERVICE_EXE) => service = Some(bytes),
                Some(n)
                    if n.eq_ignore_ascii_case(DLL)
                        && contains(&bytes, b"GameInputInitialize\0")
                        && dll.as_ref().is_none_or(|d| d.len() < bytes.len()) =>
                {
                    dll = Some(bytes)
                }
                _ => {}
            }
        }
    }
    Ok(RedistFiles {
        dll: dll.ok_or("the MSI has no x64 GameInputRedist.dll")?,
        service: service.ok_or("the MSI has no x64 GameInputRedistService.exe")?,
    })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// `IMAGE_FILE_HEADER.Machine` of a PE file.
fn pe_machine(pe: &[u8]) -> Option<u16> {
    if !pe.starts_with(b"MZ") {
        return None;
    }
    let offset = u32::from_le_bytes(pe.get(0x3c..0x40)?.try_into().ok()?) as usize;
    if pe.get(offset..offset + 4)? != b"PE\0\0" {
        return None;
    }
    Some(u16::from_le_bytes(
        pe.get(offset + 4..offset + 6)?.try_into().ok()?,
    ))
}

/// The `OriginalFilename` of a version resource: the UTF-16 key, its terminator and padding, then the value.
fn original_filename(pe: &[u8]) -> Option<String> {
    let key: Vec<u8> = "OriginalFilename\0"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let start = pe.windows(key.len()).position(|w| w == key)? + key.len();
    let units: Vec<u16> = pe[start..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .skip_while(|&u| u == 0)
        .take_while(|&u| u != 0)
        .take(260)
        .collect();
    String::from_utf16(&units).ok().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_version_resource_fields() {
        let mut pe = b"MZ".to_vec();
        pe.resize(0x40, 0);
        pe[0x3c] = 0x40;
        pe.extend_from_slice(b"PE\0\0");
        pe.extend_from_slice(&0x8664u16.to_le_bytes());
        for unit in "OriginalFilename\0\0GameInputRedist.dll\0".encode_utf16() {
            pe.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(pe_machine(&pe), Some(0x8664));
        assert_eq!(
            original_filename(&pe).as_deref(),
            Some("GameInputRedist.dll")
        );
        assert_eq!(pe_machine(b"not a pe"), None);
    }

    #[test]
    fn registry_sets_both_views() {
        let reg = registry_text();
        assert!(reg.contains(
            "[HKEY_LOCAL_MACHINE\\SOFTWARE\\Wow6432Node\\Microsoft\\GameInput]\r\n\"RedistDir\"=\"C:\\\\Program Files\\\\Microsoft GameInput\\\\x64\\\\\""
        ));
        assert_eq!(reg.matches("RedistDir").count(), 2);
    }

    /// Runs against Minecraft's MSI when `XTUX_TEST_GAMEINPUT_MSI` names it.
    #[test]
    fn extracts_a_real_redistributable() {
        let Some(path) = std::env::var_os("XTUX_TEST_GAMEINPUT_MSI") else {
            return;
        };
        let files = extract(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(original_filename(&files.dll).as_deref(), Some(DLL));
        assert!(contains(&files.dll, b"GameInputInitialize\0"));
        assert_eq!(
            original_filename(&files.service).as_deref(),
            Some(SERVICE_EXE)
        );
    }
}
