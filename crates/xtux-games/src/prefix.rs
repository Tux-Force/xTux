//! `xtux-cli prefix init`: prepares a Wine prefix for GDK games.
//!
//! - creates the prefix with `wineboot` when it does not exist yet,
//! - installs DXVK (`dxgi`, `d3d11`, `d3d10core`) and vkd3d-proton (`d3d12`, `d3d12core`), downloaded from their
//!   official GitHub releases or copied from a local Proton build, keeping Wine's own DLLs as a backup,
//! - adds the Microsoft TLS root certificates that Windows ships but Linux trust stores lack (some games verify their
//!   servers against them and report "antivirus interference" otherwise),
//! - sets how PlayStation controllers reach games (see [`apply_controller_mode`]),
//! - records what was done in `xtux-prefix.json`, which `run` reads to set the DLL overrides.

use std::path::{Path, PathBuf};

use serde_json::json;
use tokio::process::Command;

use crate::Progress;
use crate::settings::ControllerMode;

pub const STATE_FILE: &str = "xtux-prefix.json";

const DXVK_DLLS: &[&str] = &["dxgi.dll", "d3d11.dll", "d3d10core.dll"];
const VKD3D_DLLS: &[&str] = &["d3d12.dll", "d3d12core.dll"];

/// Root certificates Windows trusts that common Linux stores do not include.
const CERTIFICATES: &[(&str, &[u8])] = &[
    (
        "Microsoft TLS RSA Root G2",
        include_bytes!("../assets/certs/microsoft-tls-rsa-root-g2.crt"),
    ),
    (
        "Microsoft TLS ECC Root G2",
        include_bytes!("../assets/certs/microsoft-tls-ecc-root-g2.crt"),
    ),
];

/// PlayStation controllers that Wine opens through hidraw by default: DualShock 4 (both models), DualSense and
/// DualSense Edge.
const PLAYSTATION_CONTROLLERS: &[(u16, u16)] = &[
    (0x054c, 0x05c4),
    (0x054c, 0x09cc),
    (0x054c, 0x0ce6),
    (0x054c, 0x0df2),
];

pub struct InitOptions {
    pub wine: String,
    pub prefix: Option<String>,
    pub from_proton: Option<String>,
    pub skip_graphics: bool,
}

/// The prefix in use: `--prefix`, then `WINEPREFIX`, then `~/Games/xtux-prefix`.
pub fn resolve_prefix(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(PathBuf::from(p));
    }
    if let Some(p) = std::env::var_os("WINEPREFIX") {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Games/xtux-prefix"))
}

/// DLL overrides recorded by `prefix init`, if any.
pub fn recorded_overrides(prefix: &Path) -> Option<String> {
    let text = std::fs::read_to_string(prefix.join(STATE_FILE)).ok()?;
    let state: serde_json::Value = serde_json::from_str(&text).ok()?;
    state["dll_overrides"].as_str().map(str::to_string)
}

/// Creates or updates a prefix; returns the prefix path.
pub async fn init(
    client: &reqwest::Client,
    options: InitOptions,
    progress: &dyn Progress,
) -> crate::Result<PathBuf> {
    Ok(run_init(client, options, progress).await?)
}

async fn run_init(
    client: &reqwest::Client,
    options: InitOptions,
    progress: &dyn Progress,
) -> Result<PathBuf, String> {
    let prefix = resolve_prefix(options.prefix.as_deref())
        .ok_or("cannot determine the prefix, pass --prefix")?;
    progress.message(&format!("Prefix: {}", prefix.display()));

    if !prefix.join("drive_c").is_dir() {
        progress.message("Creating the prefix with wineboot");
        wine(&options.wine, &prefix, &["wineboot", "-u"]).await?;
        wait_wineserver(&options.wine, &prefix).await;
    }
    let system32 = prefix.join("drive_c/windows/system32");
    if !system32.is_dir() {
        return Err(format!(
            "{} does not exist, is this a 64-bit prefix?",
            system32.display()
        ));
    }

    // A certificate-only update must retain the installed graphics configuration.
    let mut state = std::fs::read_to_string(prefix.join(STATE_FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .filter(|state| state.is_object())
        .unwrap_or_else(|| json!({}));
    let mut overridden: Vec<&str> = Vec::new();

    if !options.skip_graphics {
        let work = tempfile::tempdir().map_err(|e| e.to_string())?;
        let (dxvk_dir, vkd3d_dir, dxvk_version, vkd3d_version) = match &options.from_proton {
            Some(proton) => {
                let lib = Path::new(proton).join("files/lib/wine");
                (
                    lib.join("dxvk/x86_64-windows"),
                    lib.join("vkd3d-proton/x86_64-windows"),
                    format!("from {proton}"),
                    format!("from {proton}"),
                )
            }
            None => {
                let (dxvk_version, dxvk) =
                    fetch_release(client, "doitsujin/dxvk", ".tar.gz", work.path(), progress)
                        .await?;
                let (vkd3d_version, vkd3d) = fetch_release(
                    client,
                    "HansKristian-Work/vkd3d-proton",
                    ".tar.zst",
                    work.path(),
                    progress,
                )
                .await?;
                (
                    find_dir(&dxvk, "x64")?,
                    find_dir(&vkd3d, "x64")?,
                    dxvk_version,
                    vkd3d_version,
                )
            }
        };

        let backup = prefix.join("xtux-builtin-backup");
        install_dlls(&dxvk_dir, &system32, &backup, DXVK_DLLS)?;
        install_dlls(&vkd3d_dir, &system32, &backup, VKD3D_DLLS)?;
        progress.message(&format!(
            "Installed DXVK {dxvk_version} and vkd3d-proton {vkd3d_version}"
        ));
        overridden.extend(DXVK_DLLS.iter().chain(VKD3D_DLLS));
        state["dxvk"] = json!(dxvk_version);
        state["vkd3d_proton"] = json!(vkd3d_version);
    }

    import_certificates(&options.wine, &prefix).await?;
    state["certificates"] = json!(
        CERTIFICATES
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
    );
    progress.message(&format!("Added {} root certificates", CERTIFICATES.len()));

    let mode = crate::settings::Settings::load()
        .unwrap_or_default()
        .controller;
    write_controller_mode(&options.wine, &prefix, mode).await?;
    wait_wineserver(&options.wine, &prefix).await;
    state["controller"] = json!(mode.as_str());
    progress.message(&format!("PlayStation controllers: {}", mode.as_str()));

    if !overridden.is_empty() {
        let names: Vec<&str> = overridden
            .iter()
            .map(|d| d.trim_end_matches(".dll"))
            .collect();
        state["dll_overrides"] = json!(format!("{}=n,b", names.join(",")));
    }
    let text = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?;
    std::fs::write(prefix.join(STATE_FILE), text).map_err(|e| e.to_string())?;
    Ok(prefix)
}

/// Whether applying a controller mode needs a restart of the prefix.
#[derive(Debug, PartialEq, Eq)]
pub enum ControllerChange {
    /// Already set, or set while the prefix was not running.
    Applied,
    /// Written, but Wine reads it when the prefix starts: games already running keep the old mode.
    AfterRestart,
}

/// Makes PlayStation controllers show up as Xbox gamepads (`xbox`: SDL maps them, Wine's `Hidraw` device option
/// off) or as themselves (`native`: Wine's default, hidraw). Does nothing when the prefix already has `mode`.
pub async fn apply_controller_mode(
    wine_path: &str,
    prefix: &Path,
    mode: ControllerMode,
) -> crate::Result<ControllerChange> {
    let state_file = prefix.join(STATE_FILE);
    let mut state = std::fs::read_to_string(&state_file)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .filter(|state| state.is_object())
        .unwrap_or_else(|| json!({}));
    if state["controller"].as_str() == Some(mode.as_str()) {
        return Ok(ControllerChange::Applied);
    }

    let running = server_running(prefix);
    write_controller_mode(wine_path, prefix, mode).await?;
    if !running {
        // The game then starts a fresh wineserver, whose winebus reads the new setting.
        wait_wineserver(wine_path, prefix).await;
    }
    state["controller"] = json!(mode.as_str());
    let text =
        serde_json::to_string_pretty(&state).map_err(|e| crate::Error::msg(e.to_string()))?;
    std::fs::write(&state_file, text)?;
    Ok(if running {
        ControllerChange::AfterRestart
    } else {
        ControllerChange::Applied
    })
}

/// The `.reg` text for `mode`: `HKLM\System\CurrentControlSet\Services\WineBus\Devices\<vid>/<pid>`, `Hidraw`.
fn controller_reg(mode: ControllerMode) -> String {
    let mut reg = String::from("Windows Registry Editor Version 5.00\r\n\r\n");
    for (vid, pid) in PLAYSTATION_CONTROLLERS {
        reg.push_str(&format!(
            "[HKEY_LOCAL_MACHINE\\System\\CurrentControlSet\\Services\\WineBus\\Devices\\{vid:04x}/{pid:04x}]\r\n{}\r\n\r\n",
            match mode {
                ControllerMode::Xbox => "\"Hidraw\"=dword:00000000",
                ControllerMode::Native => "\"Hidraw\"=-",
            }
        ));
    }
    reg
}

async fn write_controller_mode(
    wine_path: &str,
    prefix: &Path,
    mode: ControllerMode,
) -> Result<(), String> {
    let file = tempfile::Builder::new()
        .suffix(".reg")
        .tempfile()
        .map_err(|e| e.to_string())?;
    std::fs::write(file.path(), controller_reg(mode)).map_err(|e| e.to_string())?;
    let path = file.path().to_string_lossy().to_string();
    wine(wine_path, prefix, &["reg", "import", &path]).await
}

/// Whether a wineserver serves `prefix`: it listens on `/tmp/.wine-<uid>/server-<dev>-<inode>/socket`.
pub fn server_running(prefix: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(prefix) else {
        return false;
    };
    let uid = rustix::process::getuid().as_raw();
    let socket = PathBuf::from(format!(
        "/tmp/.wine-{uid}/server-{:x}-{:x}/socket",
        meta.dev(),
        meta.ino()
    ));
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

async fn wine(wine: &str, prefix: &Path, args: &[&str]) -> Result<(), String> {
    let status = Command::new(wine)
        .args(args)
        .env("WINEPREFIX", prefix)
        .env("WINEDEBUG", "-all")
        .status()
        .await
        .map_err(|e| format!("could not start {wine}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`wine {}` failed with {status}", args.join(" ")))
    }
}

/// Waits until the prefix's wineserver has written the registry to disk.
async fn wait_wineserver(wine: &str, prefix: &Path) {
    let server = Path::new(wine)
        .parent()
        .map(|d| d.join("server/wineserver"));
    let server = match server {
        Some(s) if s.exists() => s,
        _ => PathBuf::from("wineserver"),
    };
    let _ = Command::new(server)
        .arg("-w")
        .env("WINEPREFIX", prefix)
        .status()
        .await;
}

/// Downloads the latest release asset of `repo` ending in `suffix` and unpacks it into `work`.
async fn fetch_release(
    client: &reqwest::Client,
    repo: &str,
    suffix: &str,
    work: &Path,
    progress: &dyn Progress,
) -> Result<(String, PathBuf), String> {
    let release: serde_json::Value = client
        .get(format!(
            "https://api.github.com/repos/{repo}/releases/latest"
        ))
        .header("User-Agent", "xtux")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("{repo}: {e}"))?
        .json()
        .await
        .map_err(|e| format!("{repo}: {e}"))?;
    let version = release["tag_name"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    let asset = release["assets"]
        .as_array()
        .and_then(|assets| {
            assets.iter().find(|a| {
                a["name"]
                    .as_str()
                    .is_some_and(|n| n.ends_with(suffix) && !n.contains("native"))
            })
        })
        .ok_or_else(|| format!("{repo} {version}: no {suffix} asset"))?;
    let name = asset["name"].as_str().unwrap_or("release");
    let url = asset["browser_download_url"]
        .as_str()
        .ok_or("asset without download url")?;

    progress.message(&format!("Downloading {name}"));
    let bytes = client
        .get(url)
        .header("User-Agent", "xtux")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("{name}: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("{name}: {e}"))?;
    let archive = work.join(name);
    std::fs::write(&archive, &bytes).map_err(|e| e.to_string())?;

    let target = work.join(repo.replace('/', "_"));
    std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
    // tar picks gzip or zstd by itself; zstd archives need the zstd tool installed.
    let status = Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&target)
        .status()
        .await
        .map_err(|e| format!("could not run tar: {e}"))?;
    if !status.success() {
        return Err(format!("unpacking {name} failed (is zstd installed?)"));
    }
    Ok((version, target))
}

/// The first directory named `name` below `root`.
fn find_dir(root: &Path, name: &str) -> Result<PathBuf, String> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .map_err(|e| e.to_string())?
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == name) {
                    return Ok(path);
                }
                stack.push(path);
            }
        }
    }
    Err(format!("no {name} directory in {}", root.display()))
}

fn install_dlls(from: &Path, system32: &Path, backup: &Path, dlls: &[&str]) -> Result<(), String> {
    std::fs::create_dir_all(backup).map_err(|e| e.to_string())?;
    for dll in dlls {
        let source = from.join(dll);
        if !source.is_file() {
            return Err(format!("{} is missing", source.display()));
        }
        let target = system32.join(dll);
        let saved = backup.join(dll);
        // Keep Wine's own DLL from the first run only, so repeated runs do not overwrite it.
        if target.exists() && !saved.exists() {
            std::fs::copy(&target, &saved).map_err(|e| e.to_string())?;
        }
        std::fs::copy(&source, &target).map_err(|e| format!("{}: {e}", target.display()))?;
    }
    Ok(())
}

/// Adds the certificates to the machine's Root store (Wine reads `Blob` values of serialized certificates).
async fn import_certificates(wine_path: &str, prefix: &Path) -> Result<(), String> {
    let mut reg = String::from("Windows Registry Editor Version 5.00\r\n\r\n");
    for (_, der) in CERTIFICATES {
        // Serialized element: property id 32 (CERT_CERT_PROP_ID), reserved 1, length, DER data.
        let mut blob = Vec::with_capacity(der.len() + 12);
        blob.extend_from_slice(&32u32.to_le_bytes());
        blob.extend_from_slice(&1u32.to_le_bytes());
        blob.extend_from_slice(&(der.len() as u32).to_le_bytes());
        blob.extend_from_slice(der);
        let hex: Vec<String> = blob.iter().map(|b| format!("{b:02x}")).collect();
        reg.push_str(&format!(
            "[HKEY_LOCAL_MACHINE\\Software\\Microsoft\\SystemCertificates\\Root\\Certificates\\{}]\r\n\"Blob\"=hex:{}\r\n\r\n",
            sha1_hex(der),
            hex.join(",")
        ));
    }
    let file = tempfile::Builder::new()
        .suffix(".reg")
        .tempfile()
        .map_err(|e| e.to_string())?;
    std::fs::write(file.path(), reg).map_err(|e| e.to_string())?;
    let path = file.path().to_string_lossy().to_string();
    wine(wine_path, prefix, &["reg", "import", &path]).await?;
    wait_wineserver(wine_path, prefix).await;
    Ok(())
}

/// Certificate thumbprint as Windows names its registry key.
fn sha1_hex(data: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    Sha1::digest(data)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect()
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn certificate_update_preserves_graphics_settings() {
        let prefix = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(prefix.path().join("drive_c/windows/system32")).unwrap();
        let previous = serde_json::json!({
            "dxvk": "existing-dxvk", "vkd3d_proton": "existing-vkd3d",
            "dll_overrides": "dxgi,d3d11,d3d12,d3d12core=n,b"
        });
        std::fs::write(prefix.path().join(super::STATE_FILE), previous.to_string()).unwrap();
        super::run_init(
            &reqwest::Client::new(),
            super::InitOptions {
                // From PATH: /bin/true does not exist on macOS.
                wine: "true".into(),
                prefix: Some(prefix.path().to_str().unwrap().into()),
                from_proton: None,
                skip_graphics: true,
            },
            &crate::Silent,
        )
        .await
        .unwrap();
        let after: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(prefix.path().join(super::STATE_FILE)).unwrap(),
        )
        .unwrap();
        for key in ["dxvk", "vkd3d_proton", "dll_overrides"] {
            assert_eq!(after[key], previous[key]);
        }
        assert_eq!(after["certificates"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn controller_reg_covers_playstation_controllers() {
        use super::{ControllerMode, PLAYSTATION_CONTROLLERS, controller_reg};
        let xbox = controller_reg(ControllerMode::Xbox);
        assert!(xbox.contains(
            "[HKEY_LOCAL_MACHINE\\System\\CurrentControlSet\\Services\\WineBus\\Devices\\054c/0ce6]\r\n\"Hidraw\"=dword:00000000"
        ));
        assert_eq!(
            xbox.matches("dword:00000000").count(),
            PLAYSTATION_CONTROLLERS.len()
        );
        let native = controller_reg(ControllerMode::Native);
        assert_eq!(
            native.matches("\"Hidraw\"=-").count(),
            PLAYSTATION_CONTROLLERS.len()
        );
    }

    #[test]
    fn thumbprints_match_windows() {
        assert_eq!(
            super::sha1_hex(super::CERTIFICATES[0].1),
            "21734D95A2473BE25CBFD12A84C6FBC5BC8E2414"
        );
        assert_eq!(
            super::sha1_hex(super::CERTIFICATES[1].1),
            "F82BB951BA6B8A85ADFA7515028560D1250E7237"
        );
    }
}
