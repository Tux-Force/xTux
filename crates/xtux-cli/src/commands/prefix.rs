//! `xtux-cli prefix init`: prepares a Wine prefix for GDK games.
//!
//! - creates the prefix with `wineboot` when it does not exist yet,
//! - installs DXVK (`dxgi`, `d3d11`, `d3d10core`) and vkd3d-proton (`d3d12`, `d3d12core`), downloaded from their
//!   official GitHub releases or copied from a local Proton build, keeping Wine's own DLLs as a backup,
//! - adds the Microsoft TLS root certificates that Windows ships but Linux trust stores lack (some games verify their
//!   servers against them and report "antivirus interference" otherwise),
//! - records what was done in `xtux-prefix.json`, which `run` reads to set the DLL overrides.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::json;
use tokio::process::Command;

pub const STATE_FILE: &str = "xtux-prefix.json";

const DXVK_DLLS: &[&str] = &["dxgi.dll", "d3d11.dll", "d3d10core.dll"];
const VKD3D_DLLS: &[&str] = &["d3d12.dll", "d3d12core.dll"];

/// Root certificates Windows trusts that common Linux stores do not include.
const CERTIFICATES: &[(&str, &[u8])] = &[
    (
        "Microsoft TLS RSA Root G2",
        include_bytes!("../../assets/certs/microsoft-tls-rsa-root-g2.crt"),
    ),
    (
        "Microsoft TLS ECC Root G2",
        include_bytes!("../../assets/certs/microsoft-tls-ecc-root-g2.crt"),
    ),
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

pub async fn init(client: &reqwest::Client, options: InitOptions) -> ExitCode {
    match run_init(client, options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("prefix init failed: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run_init(client: &reqwest::Client, options: InitOptions) -> Result<(), String> {
    let prefix = resolve_prefix(options.prefix.as_deref()).ok_or("cannot determine the prefix, pass --prefix")?;
    println!("Prefix: {}", prefix.display());

    if !prefix.join("drive_c").is_dir() {
        println!("Creating the prefix with wineboot ...");
        wine(&options.wine, &prefix, &["wineboot", "-u"]).await?;
        wait_wineserver(&options.wine, &prefix).await;
    }
    let system32 = prefix.join("drive_c/windows/system32");
    if !system32.is_dir() {
        return Err(format!("{} does not exist, is this a 64-bit prefix?", system32.display()));
    }

    let mut state = json!({ "certificates": [] });
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
                let (dxvk_version, dxvk) = fetch_release(client, "doitsujin/dxvk", ".tar.gz", work.path()).await?;
                let (vkd3d_version, vkd3d) =
                    fetch_release(client, "HansKristian-Work/vkd3d-proton", ".tar.zst", work.path()).await?;
                (find_dir(&dxvk, "x64")?, find_dir(&vkd3d, "x64")?, dxvk_version, vkd3d_version)
            }
        };

        let backup = prefix.join("xtux-builtin-backup");
        install_dlls(&dxvk_dir, &system32, &backup, DXVK_DLLS)?;
        install_dlls(&vkd3d_dir, &system32, &backup, VKD3D_DLLS)?;
        println!("Installed DXVK {dxvk_version} and vkd3d-proton {vkd3d_version}");
        overridden.extend(DXVK_DLLS.iter().chain(VKD3D_DLLS));
        state["dxvk"] = json!(dxvk_version);
        state["vkd3d_proton"] = json!(vkd3d_version);
    }

    import_certificates(&options.wine, &prefix).await?;
    state["certificates"] = json!(CERTIFICATES.iter().map(|(name, _)| *name).collect::<Vec<_>>());
    println!("Added {} root certificates", CERTIFICATES.len());

    if !overridden.is_empty() {
        let names: Vec<&str> = overridden.iter().map(|d| d.trim_end_matches(".dll")).collect();
        state["dll_overrides"] = json!(format!("{}=n,b", names.join(",")));
    }
    let text = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?;
    std::fs::write(prefix.join(STATE_FILE), text).map_err(|e| e.to_string())?;
    println!("Done. `xtux-cli run` uses this prefix setup when WINEPREFIX points here.");
    Ok(())
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
    let server = Path::new(wine).parent().map(|d| d.join("server/wineserver"));
    let server = match server {
        Some(s) if s.exists() => s,
        _ => PathBuf::from("wineserver"),
    };
    let _ = Command::new(server).arg("-w").env("WINEPREFIX", prefix).status().await;
}

/// Downloads the latest release asset of `repo` ending in `suffix` and unpacks it into `work`.
async fn fetch_release(
    client: &reqwest::Client,
    repo: &str,
    suffix: &str,
    work: &Path,
) -> Result<(String, PathBuf), String> {
    let release: serde_json::Value = client
        .get(format!("https://api.github.com/repos/{repo}/releases/latest"))
        .header("User-Agent", "xtux")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("{repo}: {e}"))?
        .json()
        .await
        .map_err(|e| format!("{repo}: {e}"))?;
    let version = release["tag_name"].as_str().unwrap_or("unknown").to_string();
    let asset = release["assets"]
        .as_array()
        .and_then(|assets| {
            assets.iter().find(|a| {
                a["name"].as_str().is_some_and(|n| n.ends_with(suffix) && !n.contains("native"))
            })
        })
        .ok_or_else(|| format!("{repo} {version}: no {suffix} asset"))?;
    let name = asset["name"].as_str().unwrap_or("release");
    let url = asset["browser_download_url"].as_str().ok_or("asset without download url")?;

    println!("Downloading {name} ...");
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
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
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
    Sha1::digest(data).iter().map(|b| format!("{b:02X}")).collect()
}

#[cfg(test)]
mod tests {
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
