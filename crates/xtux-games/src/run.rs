//! Starting a game: license, decryption of the encrypted executables into memory files, the environment for
//! xgameruntime (identity, game bridge, DLL overrides) and the Wine process.

use std::collections::HashMap;
use std::future::Future;
use std::os::fd::{AsFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use msixvc::layout::PAGE_SIZE;
use msixvc::xvd::{SegmentFile, XvdFile};
#[cfg(target_os = "linux")]
use rustix::fs::{MemfdFlags, memfd_create};
use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
#[cfg(target_os = "macos")]
use tempfile::tempdir;
#[cfg(not(target_os = "linux"))]
use tempfile::{tempfile, tempfile_in};
use tokio::fs::{File, OpenOptions};
use tokio::process::{Child, Command};
use xodus::models::secrets::Token;
use xodus::tokens::TokenManager;

use crate::license::get_license;
use crate::{Error, Progress, Result};

/// Package header of the Xodus layout, see [`crate::layout`].
pub const PACKAGE_FILE: &str = crate::layout::XODUS_PACKAGE;

pub struct LaunchOptions {
    /// Game folder: a streamed or downloaded game, or a link folder of a Windows install.
    pub source: PathBuf,
    /// Wine binary to start the game with.
    pub wine: String,
    /// Executable to start (package relative), defaults to the first encrypted one.
    pub exe: Option<String>,
    /// Store market for the license request, `neutral` when not set.
    pub market: Option<String>,
    /// Environment for the game on top of this process' environment, for example the client's `WINEPREFIX`
    /// when the daemon starts the game. These values also count as "set by the user" for the defaults below.
    pub env: Vec<(String, String)>,
    /// Command line arguments for the game.
    pub args: Vec<String>,
    /// File for Wine's output (overwritten); `None` keeps this process' stdout and stderr.
    pub log: Option<PathBuf>,
}

type Cleanup = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A started game. Dropping it does not stop the game; call [`RunningGame::wait`].
pub struct RunningGame {
    pub pid: u32,
    /// When the game was started; logs older than this are from an earlier run.
    pub started: SystemTime,
    /// Set by the game bridge when a license token request failed while the game ran.
    pub license_failed: Arc<AtomicBool>,
    child: Child,
    cleanup: Cleanup,
}

impl RunningGame {
    /// Waits for the game to exit and releases what was set up for it.
    pub async fn wait(mut self) -> Result<ExitStatus> {
        let status = self.child.wait().await?;
        self.cleanup.await;
        Ok(status)
    }
}

#[cfg(target_os = "linux")]
fn make_temp_file(_folder: &str) -> std::io::Result<std::fs::File> {
    let fd = memfd_create("xodus", MemfdFlags::CLOEXEC).map_err(std::io::Error::from)?;
    Ok(std::fs::File::from(fd))
}

#[cfg(not(target_os = "linux"))]
fn make_temp_file(folder: &str) -> std::io::Result<std::fs::File> {
    if folder.is_empty() {
        tempfile()
    } else {
        tempfile_in(folder)
    }
}

#[cfg(target_os = "macos")]
async fn prepare(lfiles: &HashMap<String, SegmentFile>) -> Result<(Cleanup, String)> {
    async fn checked(cmd: &mut Command, what: &str) -> Result<std::process::Output> {
        let out = cmd.output().await?;
        if !out.status.success() {
            return Err(Error::msg(format!("{what} failed with {}", out.status)));
        }
        Ok(out)
    }

    let disk_size: u64 = lfiles
        .iter()
        .filter(|f| f.1.keep_encrypted)
        .map(|f| f.1.length + 4 * PAGE_SIZE as u64)
        .sum();

    let out = checked(
        Command::new("/usr/bin/hdiutil")
            .arg("attach")
            .arg("-nomount")
            .arg(format!("ram://{}", disk_size.div_ceil(256))),
        "hdiutil attach",
    )
    .await?;
    let device = String::from_utf8_lossy(&out.stdout).trim().to_string();

    let vol = uuid::Uuid::new_v4().to_string();
    checked(
        Command::new("/sbin/newfs_hfs")
            .arg("-v")
            .arg(vol)
            .arg(&device),
        "newfs_hfs",
    )
    .await?;

    let mount_dir_obj = tempdir()?;
    let mount_dir = mount_dir_obj.path().to_string_lossy().to_string();
    checked(
        Command::new("/sbin/mount")
            .args(["-t", "hfs", "-o", "nobrowse", "-v"])
            .arg(&device)
            .arg(&mount_dir),
        "mount",
    )
    .await?;

    let mount_dir_cl = mount_dir.clone();
    let cleanup: Cleanup = Box::pin(async move {
        let _keep = mount_dir_obj;
        let _ = Command::new("/sbin/umount")
            .arg("-f")
            .arg(&mount_dir_cl)
            .status()
            .await;
        let _ = Command::new("/usr/bin/hdiutil")
            .arg("detach")
            .arg("-force")
            .arg(&device)
            .status()
            .await;
    });
    Ok((cleanup, mount_dir))
}

#[cfg(not(target_os = "macos"))]
async fn prepare(_lfiles: &HashMap<String, SegmentFile>) -> Result<(Cleanup, String)> {
    Ok((Box::pin(async {}), String::new()))
}

/// Environment that steers Wine and its libraries for GDK games, unless the user set it.
fn compatibility_env(extra: &[(String, String)]) -> Vec<(&'static str, String)> {
    let is_set =
        |name: &str| extra.iter().any(|(k, _)| k == name) || std::env::var_os(name).is_some();
    let prefix = extra
        .iter()
        .find(|(k, _)| k == "WINEPREFIX")
        .map(|(_, v)| PathBuf::from(v))
        .or_else(|| crate::prefix::resolve_prefix(None));
    let mut env = Vec::new();

    // Use the DXVK/vkd3d-proton DLLs that `prefix init` installed.
    if !is_set("WINEDLLOVERRIDES") {
        if let Some(overrides) = prefix
            .as_deref()
            .and_then(crate::prefix::recorded_overrides)
        {
            env.push(("WINEDLLOVERRIDES", overrides));
        }
    }

    // Hardware video decoders hand out frames in GPU memory (CUDA, Vulkan), which Wine's media pipeline cannot
    // take; games then wait forever for their intro videos (MSFS 2024). Prefer the software decoders.
    if !is_set("GST_PLUGIN_FEATURE_RANK") {
        env.push((
            "GST_PLUGIN_FEATURE_RANK",
            [
                "nvh264dec",
                "nvh265dec",
                "nvav1dec",
                "nvvp8dec",
                "nvvp9dec",
                "nvjpegdec",
                "nvmpegvideodec",
                "nvmpeg2videodec",
                "nvmpeg4videodec",
                "vulkanh264dec",
                "vulkanh265dec",
                "vulkanav1dec",
            ]
            .map(|d| format!("{d}:NONE"))
            .join(","),
        ));
    }
    env
}

/// Identity of the signed in user, handed to the game's XUser implementation.
async fn identity_env(
    client: &reqwest::Client,
    tokens: &TokenManager,
) -> Vec<(&'static str, String)> {
    let mut env = Vec::new();
    let (Ok(Token::Legacy(dev_token)), Ok(Token::Legacy(legacy))) =
        (tokens.get_device_sts_token(), tokens.get_user_sts_token())
    else {
        tracing::warn!("no signed in user, the game runs without an Xbox Live identity");
        return env;
    };
    match xodus::api::xbox::xsts_user_only(client, dev_token, legacy, "http://xboxlive.com").await {
        Ok(xsts) => {
            if let Some(xuid) = xsts.xuid() {
                env.push(("XODUS_XUID", xuid.to_string()));
            }
            if let Some(gamertag) = xsts.gamertag() {
                env.push(("XODUS_GAMERTAG", gamertag.to_string()));
            }
            if let Some(age_group) = xsts.age_group() {
                env.push(("XODUS_AGE_GROUP", age_group.to_string()));
            }
        }
        Err(err) => tracing::warn!("could not get the Xbox Live identity: {err}"),
    }
    env
}

/// Reads the package, fetches the license, decrypts the executables and starts the game in Wine.
pub async fn launch(
    client: &reqwest::Client,
    tokens: &TokenManager,
    options: LaunchOptions,
    progress: &dyn Progress,
) -> Result<RunningGame> {
    // Both layouts are read in place (see layout.rs): game files under `root`, header in `package`.
    let layout = crate::layout::detect(&options.source).ok_or_else(|| {
        Error::msg(format!(
            "{} is not a game folder (no package header found)",
            options.source.display()
        ))
    })?;
    let out: &Path = &layout.root;
    let out_absolute = std::fs::canonicalize(out)
        .map_err(|e| Error::msg(format!("failed to resolve {}: {e}", out.display())))?;
    let final_path = layout.package.clone();

    progress.message("Reading the package");
    let mut file = OpenOptions::new()
        .read(true)
        .open(&final_path)
        .await
        .map_err(|e| Error::msg(format!("failed to open {}: {e}", final_path.display())))?;
    let xvd = XvdFile::parse(&mut file).await.map_err(|e| {
        Error::msg(format!(
            "failed to parse the XVD header of {}: {e}",
            final_path.display()
        ))
    })?;

    let files = xvd
        .parse_user_package_files(&mut file)
        .await
        .map_err(|e| Error::msg(format!("failed to parse the user package files: {e}")))?;
    let mut lfiles: HashMap<String, SegmentFile> = HashMap::new();
    for (k, v) in &files {
        if k == "SegmentMetadata.bin" {
            lfiles = xvd
                .parse_segment_metadata(&mut file, v)
                .await
                .map_err(|e| Error::msg(format!("failed to parse the segment metadata: {e}")))?;
        }
    }
    // Classic packages
    if lfiles.is_empty() {
        let sfiles = xvd
            .parse_ntfs_segment_metadata(&mut file, false)
            .await
            .map_err(|e| Error::msg(format!("failed to parse the NTFS segment metadata: {e}")))?;
        for (n, sfile) in &sfiles {
            if sfile.length.div_ceil(PAGE_SIZE as u64) as usize != sfile.data_hashs.len() {
                tracing::debug!("{n}: {} {}", sfile.offset, sfile.length);
            }
        }
        lfiles.extend(sfiles);
    }

    progress.message("Getting the license");
    let (key, game_splicense) = get_license(
        client,
        tokens,
        xvd.content_id().to_string(),
        options
            .market
            .clone()
            .unwrap_or_else(|| "neutral".to_string()),
    )
    .await?;
    if game_splicense.content_keys.len() != 1 {
        return Err(Error::msg(format!(
            "unexpected number of content keys: {}",
            game_splicense.content_keys.len()
        )));
    }
    let (_, content_key) = game_splicense
        .content_keys
        .into_iter()
        .next()
        .ok_or("the license has no content key")?;
    let full_key = content_key
        .unpack(&key)
        .map_err(|e| Error::msg(format!("failed to unpack the content key: {e}")))?;

    progress.message("Decrypting the game executables");
    let (cleanup, mount_dir) = prepare(&lfiles).await?;
    let mut fds = vec![];
    for file in &lfiles {
        if !file.1.keep_encrypted {
            continue;
        }
        let mut game_exe = File::from_std(make_temp_file(&mount_dir)?);
        let source_path = out.join(file.0.replace('\\', "/"));
        let mut raw = tokio::fs::read(&source_path)
            .await
            .map_err(|e| Error::msg(format!("failed to read {}: {e}", source_path.display())))?;
        // Installs made by the Windows Xbox app store the file at its logical size, while the decryption works
        // on whole pages. Only the last, partial block is lost this way.
        let padded = (file.1.length as usize).div_ceil(PAGE_SIZE) * PAGE_SIZE;
        if raw.len() < padded {
            raw.resize(padded, 0);
        }
        let mut cursor = std::io::Cursor::new(raw);
        xvd.mount_mem_fd(&mut cursor, &mut game_exe, file.1, *full_key, |_, _| {})
            .await
            .map_err(|e| Error::msg(format!("failed to decrypt {}: {e}", file.0)))?;

        let stdf = game_exe.into_std().await;
        let mut flags = fcntl_getfd(stdf.as_fd()).map_err(std::io::Error::from)?;
        flags.remove(FdFlags::CLOEXEC);
        fcntl_setfd(stdf.as_fd(), flags).map_err(std::io::Error::from)?;
        fds.push((file.0, stdf.into_raw_fd()));
    }

    // Tell Wine which memory file stands for which path of the game folder.
    let mut file_map = String::new();
    let nt_prefix = out_absolute.to_string_lossy().replace('/', "\\");
    let nt_prefix = nt_prefix.trim_end_matches('\\');
    let mut nt_entry = None;
    for (path, fd) in fds {
        if !file_map.is_empty() {
            file_map.push('|');
        }
        let nt_suffix = path.trim_start_matches('\\');
        let nt_path = format!("\\??\\Z:{nt_prefix}\\{nt_suffix}");
        match &options.exe {
            Some(exe) if exe == path => nt_entry = Some(nt_path),
            None if nt_entry.is_none() => nt_entry = Some(nt_path),
            _ => {}
        }
        file_map.push_str(&format!("{fd}:\\??\\Z:{nt_prefix}\\{nt_suffix}"));
    }
    let nt_entry = nt_entry.ok_or("could not find the game executable")?;

    let mut env = identity_env(client, tokens).await;
    env.extend(compatibility_env(&options.env));

    // Answers GDK calls that need the Microsoft services, see bridge.rs.
    let mut license_failed = Arc::new(AtomicBool::new(false));
    match crate::bridge::start(client.clone(), tokens.clone()).await {
        Ok(bridge) => {
            license_failed = bridge.license_failed;
            env.push(("XODUS_BRIDGE_PORT", bridge.port.to_string()));
            env.push(("XODUS_BRIDGE_SECRET", bridge.secret));
        }
        Err(err) => tracing::warn!("could not start the game bridge: {err}"),
    }

    progress.message("Starting the game");
    let started = SystemTime::now();
    let mut command = Command::new(&options.wine);
    if let Some(log) = &options.log {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        // Private: with WINEDEBUG channels, Wine's output can contain the bridge secret or tokens.
        if let Some(dir) = log.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(log)
            .map_err(|e| Error::msg(format!("could not create {}: {e}", log.display())))?;
        command.stdout(file.try_clone()?).stderr(file);
    }
    let child = command
        .arg(nt_entry)
        .args(&options.args)
        .envs(env)
        .envs(options.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .env("WINE_DLL_FILE_MAP", file_map)
        .env("XODUS_PACKAGE_ID", xvd.content_id().simple().to_string())
        // Only reached after the license was obtained from the Microsoft licensing service.
        .env("XODUS_LICENSE_VERIFIED", "1")
        .spawn()
        .map_err(|e| Error::msg(format!("could not start {}: {e}", options.wine)))?;
    let pid = child.id().ok_or("the game exited immediately")?;

    Ok(RunningGame {
        pid,
        started,
        license_failed,
        child,
        cleanup,
    })
}
