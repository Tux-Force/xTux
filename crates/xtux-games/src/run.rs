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
    /// Executable to start (package relative), defaults to the desktop manifest entry.
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
    /// Sends the saves to the Xbox cloud once the game exited.
    saves: Option<SaveSync>,
    unlocks: Option<UnlockWatch>,
    _access: crate::access::Guard,
    _addon_access: Vec<crate::access::Guard>,
}

impl RunningGame {
    /// Waits for the game to exit and releases what was set up for it.
    pub async fn wait(mut self) -> Result<ExitStatus> {
        let status = self.child.wait().await?;
        self.cleanup.await;
        match (self.saves.take(), &self.unlocks) {
            (Some(saves), _) => saves.push().await,
            (None, Some(unlocks)) if self.started.elapsed().is_ok_and(|e| e < DETACHED_START) => {
                unlocks.wait_for_game().await
            }
            (None, _) => {}
        }
        if let Some(unlocks) = self.unlocks.take() {
            unlocks.finish().await;
        }
        Ok(status)
    }
}

/// A game that goes on in the background lets go of the process xTux started soon.
const DETACHED_START: std::time::Duration = std::time::Duration::from_secs(120);

/// The process xTux started can exit while the game goes on in the background (Wine detaches it); the game is over
/// only once nothing runs in the prefix any more, which is also when its wineserver ends.
async fn wait_for_prefix(prefix: &Path) {
    while crate::prefix::server_running(prefix) {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Shows achievements unlocked while the game runs. Dropping it stops watching.
struct UnlockWatch {
    prefix: PathBuf,
    stop: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl UnlockWatch {
    fn start(
        client: &reqwest::Client,
        tokens: &TokenManager,
        source: &Path,
        prefix: &Path,
    ) -> Option<UnlockWatch> {
        let game = crate::cloudsave::Game::for_folder(source)?;
        let stop = tokio_util::sync::CancellationToken::new();
        let name = game.name.clone();
        let task = tokio::spawn(crate::achievements::watch(
            client.clone(),
            tokens.clone(),
            game,
            stop.clone(),
            move |achievement| {
                let name = name.clone();
                async move { crate::achievements::notify(&name, &achievement).await }
            },
        ));
        Some(UnlockWatch {
            prefix: prefix.to_owned(),
            stop,
            task,
        })
    }

    /// Waits for a game that goes on in the background, as long as there is something to watch.
    async fn wait_for_game(&self) {
        let ended = async {
            while !self.task.is_finished() {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        };
        tokio::select! {
            _ = wait_for_prefix(&self.prefix) => {}
            _ = ended => {}
        }
    }

    /// Looks once more for what was unlocked right before the end.
    async fn finish(mut self) {
        self.stop.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), &mut self.task).await;
    }
}

impl Drop for UnlockWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// What it takes to sync a game's saves with the Xbox cloud.
struct SaveSync {
    client: reqwest::Client,
    tokens: TokenManager,
    source: PathBuf,
    prefix: PathBuf,
}

/// How long a sync may hold up the start of a game or the end of a run.
const SAVE_SYNC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

impl SaveSync {
    /// Brings in the saves that changed in the cloud. Problems are reported; the game starts anyway.
    async fn pull(&self, progress: &dyn Progress) {
        progress.message("Syncing saves with the Xbox cloud");
        let result = tokio::time::timeout(SAVE_SYNC_TIMEOUT, async {
            match crate::cloudsave::Saves::open(
                &self.client,
                &self.tokens,
                &self.source,
                &self.prefix,
            )
            .await?
            {
                Some(saves) => saves.pull().await.map(Some),
                None => Ok(None),
            }
        })
        .await;
        match result {
            Ok(Ok(Some(report))) => {
                tracing::info!("cloud saves before the game: {report:?}");
                if !report.is_empty() {
                    progress.message(&format!("Cloud {}", report.summary()));
                }
            }
            Ok(Ok(None)) => tracing::info!("the game has no Xbox Live identity, no cloud saves"),
            Ok(Err(err)) => {
                tracing::warn!("cloud saves: {err}");
                progress.message(&format!("Cloud saves could not be synced ({err}); the game uses the saves on this computer"));
            }
            Err(_) => progress.message(
                "Cloud saves did not answer in time; the game uses the saves on this computer",
            ),
        }
    }

    /// Sends the saves that changed while playing.
    async fn push(&self) {
        // The saves are final only then.
        wait_for_prefix(&self.prefix).await;
        let result = tokio::time::timeout(SAVE_SYNC_TIMEOUT * 4, async {
            match crate::cloudsave::Saves::open(
                &self.client,
                &self.tokens,
                &self.source,
                &self.prefix,
            )
            .await?
            {
                Some(saves) => saves.push().await.map(Some),
                None => Ok(None),
            }
        })
        .await;
        match result {
            Ok(Ok(Some(report))) => {
                tracing::info!("cloud saves after the game: {}", report.summary())
            }
            Ok(Ok(None)) => {}
            Ok(Err(err)) => tracing::warn!(
                "cloud saves could not be sent, they stay on this computer until the next run: {err}"
            ),
            Err(_) => tracing::warn!(
                "cloud saves did not answer in time, they are sent after the next run"
            ),
        }
    }
}

/// Stops a launch's dedicated process group, including children that ignore Ctrl+C.
/// The caller must keep this future alive even if the initial launcher exits first.
pub async fn stop_process_group(pid: u32) {
    use rustix::process::{Pid, Signal, kill_process_group};
    let Some(group) = Pid::from_raw(pid as i32).filter(|p| p.as_raw_nonzero().get() > 1) else {
        return;
    };
    let _ = kill_process_group(group, Signal::INT);
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let _ = kill_process_group(group, Signal::KILL);
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

/// Folder of the Steam tool `name` (for example `Proton BattlEye Runtime`) in one of the Steam libraries, when it
/// has the subfolder `version`.
fn steam_tool(name: &str, version: &str) -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let roots = [
        home.join(".local/share/Steam"),
        home.join(".steam/steam"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
    ];
    let mut libraries: Vec<PathBuf> = roots.to_vec();
    for root in &roots {
        let Ok(text) = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) else {
            continue;
        };
        for line in text.lines() {
            let mut fields = line.split('"').filter(|f| !f.trim().is_empty());
            if fields.next() == Some("path")
                && let Some(path) = fields.next()
            {
                libraries.push(PathBuf::from(path.replace("\\\\", "\\")));
            }
        }
    }
    libraries
        .into_iter()
        .map(|library| library.join("steamapps/common").join(name))
        .find(|dir| dir.join(version).is_dir())
}

/// Folder of the Proton BattlEye Runtime: `PROTON_BATTLEYE_RUNTIME`, or the Steam tool of that name.
fn battleye_runtime() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("PROTON_BATTLEYE_RUNTIME") {
        return Some(PathBuf::from(dir));
    }
    steam_tool("Proton BattlEye Runtime", "v1")
}

/// Folder of the Proton EasyAntiCheat Runtime: `PROTON_EAC_RUNTIME`, or the Steam tool of that name (Steam app
/// 1826330). Wine takes its bridge from `v2/lib32` and `v2/lib64`.
fn eac_runtime() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("PROTON_EAC_RUNTIME") {
        return Some(PathBuf::from(dir));
    }
    steam_tool("Proton EasyAntiCheat Runtime", "v2")
}

/// Easy Anti-Cheat in a game folder.
#[derive(Debug, PartialEq, Eq)]
struct EasyAntiCheat {
    /// The game ships EAC's Linux client (`easyanticheat_x64.so` next to `easyanticheat_x64.dll`). Without it Wine
    /// loads the Windows client, which needs a kernel driver: the developer has not enabled EAC for Linux in this
    /// version of the game.
    linux_client: bool,
}

/// Looks for EAC's client DLL in the game folder and up to three folders deep.
fn easyanticheat(content: &Path) -> Option<EasyAntiCheat> {
    const DLLS: &[&str] = &[
        "easyanticheat_x64.dll",
        "easyanticheat_x86.dll",
        "easyanticheat.dll",
    ];
    let mut found = None;
    let mut folders = vec![(content.to_path_buf(), 0)];
    while let Some((folder, depth)) = folders.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        let names: Vec<(String, std::fs::FileType, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                Some((
                    e.file_name().to_string_lossy().to_lowercase(),
                    e.file_type().ok()?,
                    e.path(),
                ))
            })
            .collect();
        for (name, kind, path) in &names {
            if kind.is_dir() && depth < 3 {
                folders.push((path.clone(), depth + 1));
            } else if DLLS.contains(&name.as_str()) {
                let so = name.replace(".dll", ".so");
                let linux_client = names.iter().any(|(other, _, _)| *other == so);
                // One folder with the Linux client is enough.
                if linux_client || found.is_none() {
                    found = Some(EasyAntiCheat { linux_client });
                }
            }
        }
    }
    found
}

/// The `Battleye/BELauncher.ini` of a game folder, whatever the case of its folder name.
fn battleye_launcher_config(content: &Path) -> Option<PathBuf> {
    std::fs::read_dir(content)
        .ok()?
        .flatten()
        .find(|entry| entry.file_name().eq_ignore_ascii_case("battleye"))
        .map(|entry| entry.path().join("BELauncher.ini"))
        .filter(|config| config.is_file())
}

/// Adds `value` to the DLL overrides xTux passes to Wine, unless the user chose their own.
fn add_dll_override(
    env: &mut Vec<(&'static str, String)>,
    user_env: &[(String, String)],
    value: &str,
) {
    if user_env.iter().any(|(k, _)| k == "WINEDLLOVERRIDES")
        || std::env::var_os("WINEDLLOVERRIDES").is_some()
    {
        return;
    }
    match env.iter_mut().find(|(k, _)| *k == "WINEDLLOVERRIDES") {
        Some((_, overrides)) if !overrides.is_empty() => {
            overrides.push(';');
            overrides.push_str(value);
        }
        Some((_, overrides)) => *overrides = value.to_string(),
        None => env.push(("WINEDLLOVERRIDES", value.to_string())),
    }
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

/// Which add-ons belong to the game in `source`: those naming its Store ID, or a product it belongs to (from the
/// Store catalog; skipped when the catalog cannot be reached quickly).
async fn addon_scope(client: &reqwest::Client, source: &Path) -> crate::packages::Scope {
    let settings = crate::settings::Settings::load().unwrap_or_default();
    let Some(store_id) = crate::library::inspect(source)
        .and_then(|g| g.store_id)
        .filter(|s| crate::art::is_store_id(s))
    else {
        return crate::packages::Scope::default();
    };
    let market = settings
        .market
        .filter(|m| m.len() == 2 && m.chars().all(|c| c.is_ascii_alphabetic()))
        .unwrap_or_else(|| "US".to_string());
    let mut store_ids = vec![store_id.to_uppercase()];
    match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        crate::catalog::details(client, &store_ids, &market),
    )
    .await
    {
        Ok(Ok(titles)) => store_ids.extend(titles.into_iter().flat_map(|t| t.parents)),
        Ok(Err(err)) => tracing::warn!("no catalog entry for {store_id}: {err}"),
        Err(_) => tracing::warn!("the catalog did not answer for {store_id}"),
    }
    store_ids.sort();
    store_ids.dedup();
    crate::packages::Scope {
        library_dirs: settings.library_dirs,
        store_ids,
    }
}

/// Reads the package, fetches the license, decrypts the executables and starts the game in Wine.
pub async fn launch(
    client: &reqwest::Client,
    tokens: &TokenManager,
    options: LaunchOptions,
    progress: &dyn Progress,
) -> Result<RunningGame> {
    let access = crate::access::acquire(&options.source, false)?;
    crate::updates::ensure_ready(&options.source)?;
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
    let entry = options.exe.clone().or_else(|| {
        crate::library::read_config(out)
            .and_then(|config| crate::library::desktop_executable(&config))
    });
    for (path, fd) in fds {
        if !file_map.is_empty() {
            file_map.push('|');
        }
        let nt_suffix = path.trim_start_matches('\\');
        let nt_path = format!("\\??\\Z:{nt_prefix}\\{nt_suffix}");
        match &entry {
            Some(exe)
                if exe
                    .replace('/', "\\")
                    .eq_ignore_ascii_case(path.trim_start_matches('\\')) =>
            {
                nt_entry = Some(nt_path)
            }
            None if nt_entry.is_none() => nt_entry = Some(nt_path),
            _ => {}
        }
        file_map.push_str(&format!("{fd}:\\??\\Z:{nt_prefix}\\{nt_suffix}"));
    }
    // Some packages (e.g. Minecraft for Windows) ship their entry executable unencrypted; Wine then reads it
    // straight from the game folder instead of from a decrypted memory file.
    let nt_entry = nt_entry
        .or_else(|| {
            let exe = entry.as_ref()?;
            out.join(exe.replace('\\', "/")).is_file().then(|| {
                let nt_suffix = exe.replace('/', "\\");
                format!(
                    "\\??\\Z:{nt_prefix}\\{}",
                    nt_suffix.trim_start_matches('\\')
                )
            })
        })
        .ok_or("could not find the game executable")?;

    let prefix = options
        .env
        .iter()
        .find(|(k, _)| k == "WINEPREFIX")
        .map(|(_, v)| PathBuf::from(v))
        .or_else(|| crate::prefix::resolve_prefix(None))
        .filter(|p| p.join("drive_c").is_dir());
    let saves = prefix
        .as_ref()
        .filter(|_| {
            crate::settings::Settings::load()
                .unwrap_or_default()
                .cloud_saves
                != Some(false)
        })
        .map(|prefix| SaveSync {
            client: client.clone(),
            tokens: tokens.clone(),
            source: options.source.clone(),
            prefix: prefix.clone(),
        });
    if let Some(saves) = &saves {
        saves.pull(progress).await;
    }
    if let Some(prefix) = &prefix {
        let mode = crate::settings::Settings::load()
            .unwrap_or_default()
            .effective(&options.source)
            .controller
            .unwrap_or_default();
        match crate::prefix::apply_controller_mode(&options.wine, prefix, mode).await {
            Ok(crate::prefix::ControllerChange::Applied) => {}
            Ok(crate::prefix::ControllerChange::AfterRestart) => progress.message(&format!(
                "PlayStation controllers switch to {} once all games of this prefix are closed",
                mode.as_str()
            )),
            // Not worth failing the start for.
            Err(err) => tracing::warn!("could not set the controller mode: {err}"),
        }
        if crate::gameinput::shipped_msi(out).is_some() {
            match crate::gameinput::ensure(&options.wine, prefix, out).await {
                Ok(true) => progress.message("Installed the GameInput redistributable of the game"),
                Ok(false) => {}
                // The game reports the missing component itself.
                Err(err) => tracing::warn!("could not install GameInput: {err}"),
            }
        }
    }

    let mut env = identity_env(client, tokens).await;
    env.extend(compatibility_env(&options.env));
    // Without AMD's own amd_ags_x64.dll in the game folder, a game only probes for AGS. Wine's builtin then
    // reports a GPU whose driver extensions it cannot provide, and DayZ re-initialises it until its stack overflows.
    if !out.join("amd_ags_x64.dll").is_file() {
        add_dll_override(&mut env, &options.env, "amd_ags_x64=d");
    }
    // The BattlEye launcher installs a Windows service with a kernel driver, which Wine cannot run, and the game
    // then waits for it forever. Like Proton, start the game through Wine's belauncher.exe with the Proton BattlEye
    // Runtime instead; its beclient DLLs replace the ones in the game folder.
    let nt_entry = match (
        options
            .exe
            .is_none()
            .then(|| battleye_launcher_config(out))
            .flatten(),
        battleye_runtime(),
    ) {
        (Some(_), Some(runtime)) => {
            env.push((
                "PROTON_BATTLEYE_RUNTIME",
                runtime.to_string_lossy().into_owned(),
            ));
            add_dll_override(&mut env, &options.env, "beclient,beclient_x64=b,n");
            progress.message("Starting the game through BattlEye");
            "C:\\windows\\system32\\belauncher.exe".to_string()
        }
        (Some(_), None) => {
            progress.message(
                "BattlEye needs the Proton BattlEye Runtime from Steam (Library, Tools); online play will not work",
            );
            nt_entry
        }
        _ => nt_entry,
    };
    // Easy Anti-Cheat runs on Linux through the Proton EasyAntiCheat Runtime: Wine loads its bridge instead of the
    // game's easyanticheat DLL when the game ships EAC's Linux client next to it.
    if let Some(eac) = easyanticheat(out) {
        match (eac_runtime(), eac.linux_client) {
            (Some(runtime), linux_client) => {
                env.push((
                    "PROTON_EAC_RUNTIME",
                    runtime.to_string_lossy().into_owned(),
                ));
                progress.message(if linux_client {
                    "Easy Anti-Cheat runs through the Proton EasyAntiCheat Runtime"
                } else {
                    "Easy Anti-Cheat: this version of the game has no Linux client of EAC (the developer has not \
                     enabled it), so protected modes and online play will likely not work"
                });
            }
            (None, true) => progress.message(
                "Easy Anti-Cheat needs the Proton EasyAntiCheat Runtime from Steam (Library, Tools, or \
                 steam://install/1826330); online play will not work",
            ),
            (None, false) => progress.message(
                "Easy Anti-Cheat: this version of the game has no Linux client of EAC, so protected modes and \
                 online play will likely not work",
            ),
        }
    }
    // Read by GetCurrentPackageFamilyName/FullName of the Tux-Force Wine build.
    if let Some((family, full)) =
        crate::library::read_config(out).and_then(|c| crate::library::package_identity(&c))
    {
        env.push(("XODUS_PACKAGE_FAMILY_NAME", family));
        env.push(("XODUS_PACKAGE_FULL_NAME", full));
    }

    // Answers GDK calls that need the Microsoft services, see bridge.rs.
    let scope = addon_scope(client, &options.source).await;
    let mut addon_access = Vec::new();
    for addon in crate::library::addons_of(&scope.library_dirs, &scope.store_ids) {
        let guard = crate::access::acquire(&addon.path, false)?;
        crate::updates::ensure_ready(&addon.path)?;
        addon_access.push(guard);
    }
    let market = options
        .market
        .clone()
        .unwrap_or_else(|| "neutral".to_string());
    let mut license_failed = Arc::new(AtomicBool::new(false));
    match crate::bridge::start(
        client.clone(),
        tokens.clone(),
        scope,
        market,
        crate::cloudsave::Game::for_folder(&options.source),
    )
    .await
    {
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
    // Keep game children separate from the daemon and other running games.
    command.process_group(0);
    let child = command
        // Relative game assets must resolve inside the package, independent of the daemon cwd.
        .current_dir(&out_absolute)
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

    let unlocks = prefix
        .as_ref()
        .filter(|_| {
            crate::settings::Settings::load()
                .unwrap_or_default()
                .achievement_notifications
                != Some(false)
        })
        .and_then(|prefix| UnlockWatch::start(client, tokens, &options.source, prefix));

    Ok(RunningGame {
        pid,
        started,
        license_failed,
        child,
        cleanup,
        saves,
        unlocks,
        _access: access,
        _addon_access: addon_access,
    })
}

#[cfg(test)]
mod process_tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[tokio::test]
    async fn stop_forces_an_unresponsive_game_to_exit() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut child = Command::new("/bin/sh")
            .args(["-c", "trap '' INT; echo ready; while :; do sleep 1; done"])
            .process_group(0)
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .await
            .unwrap();
        assert_eq!(ready.trim(), "ready");
        stop_process_group(child.id().unwrap()).await;
        let status = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.signal(), Some(9));
    }
}

#[cfg(test)]
mod anticheat_tests {
    use super::*;

    #[test]
    fn finds_easyanticheat_and_its_linux_client() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(easyanticheat(dir.path()), None);
        let bin = dir.path().join("Game/Binaries/Win64");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("EasyAntiCheat_x64.dll"), b"").unwrap();
        assert_eq!(
            easyanticheat(dir.path()),
            Some(EasyAntiCheat {
                linux_client: false
            })
        );
        std::fs::write(bin.join("easyanticheat_x64.so"), b"").unwrap();
        assert_eq!(
            easyanticheat(dir.path()),
            Some(EasyAntiCheat { linux_client: true })
        );
    }
}
