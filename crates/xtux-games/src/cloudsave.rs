//! Cloud saves: keeps the game saves of xgameruntime (`XGameSave`) in step with the Xbox cloud, as Gaming Services
//! do on Windows, so a game continues where it was left on a Windows PC or an Xbox.
//!
//! # Local layout
//!
//! xgameruntime keeps the saves of one user and one service configuration (SCID) in
//! `<LocalAppData>/Packages/<package family name>/LocalState/GameSave/<xuid>/<scid>/`: one folder per container,
//! one file per blob. Container and blob names are stored percent encoded (see [`encode_name`]); a container's
//! display name is in its `.displayname` file. Files starting with `.` are not blobs.
//!
//! # Cloud
//!
//! The same data lives in the Xbox "connected storage" of `titlestorage.xboxlive.com`, reached with a title-bound
//! token of the game:
//!
//! - `GET  /connectedstorage/users/xuid(<xuid>)/scids/<scid>` lists the containers (`<name>,savedgame`, etag);
//! - `GET  …/savedgames/<container>` lists its atoms (blob name → atom id); `GET …/<atom>,binary` is the data;
//! - writes take a lock (`PUT …/lock`), upload each blob as a new atom (`POST …/atoms/<id>`, then the data to the
//!   returned Azure blob URI, then `POST …/atoms/<id>?commit=true`), replace the container (`PUT …/savedgames/…`)
//!   and release the lock (`DELETE …/lock`).
//!
//! # Syncing
//!
//! Before the game starts, [`Space::pull`] brings in containers that changed in the cloud; after it exits (once
//! nothing runs in the prefix any more), [`Space::push`] sends containers that changed locally. A state file per
//! save folder remembers, per container, the cloud etag and a hash of the local files at the last sync, so each
//! side can tell what changed since:
//!
//! - changed on one side: copied to the other;
//! - changed on both sides since the last sync: the newer one wins, the other one goes to the backups in
//!   `$XDG_STATE_HOME/xtux/save-backups/`;
//! - different on both sides without any earlier sync (saved here and elsewhere before syncing): left alone until
//!   the user picks a side ([`Saves::resolve`]), because "newer" says nothing about which one has the progress;
//! - deleted on one side: not deleted on the other. The service has been seen to list no containers for minutes
//!   while they still existed, and an empty or new save folder must never wipe the cloud; saves are only ever
//!   added or replaced, and whatever a sync replaces locally is backed up first.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use base64::Engine;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use xodus::auth::TitleSession;
use xodus::models::xgameruntime::xuser::UserIdentity;
use xodus::tokens::TokenManager;

const HOST: &str = "https://titlestorage.xboxlive.com/connectedstorage";
const RELYING_PARTY: &str = "http://xboxlive.com";
const DISPLAY_NAME_FILE: &str = ".displayname";
/// Backups kept per game; older ones are removed.
const MAX_BACKUPS: usize = 20;
/// Requests to the storage service; uploads of large saves take longer.
const TIMEOUT: Duration = Duration::from_secs(120);

pub type Result<T> = std::result::Result<T, String>;

/// What identifies a game's saves.
#[derive(Debug, Clone)]
pub struct Game {
    /// MSA app id from `MicrosoftGame.config`, for the title-bound sign-in.
    pub msa_app_id: String,
    pub title_id: u32,
    /// Package family name; xgameruntime keeps the saves under it, the service wants it as `x-xbl-pfn`.
    pub family: String,
    /// Display name, for messages.
    pub name: String,
}

impl Game {
    /// From the game's `MicrosoftGame.config`. Games without an MSA app id or title id cannot use the cloud.
    pub fn from_config(config: &str) -> Option<Game> {
        let msa_app_id = crate::library::element(config, "MSAAppId")?;
        let title_id = u32::from_str_radix(
            crate::library::element(config, "TitleId")?.trim_start_matches("0x"),
            16,
        )
        .ok()?;
        let family = crate::library::package_family(config)?;
        let name = crate::library::attribute(config, "ShellVisuals", "DefaultDisplayName")
            .filter(|n| !n.starts_with("ms-resource:"))
            .unwrap_or_else(|| family.clone());
        (!msa_app_id.is_empty() && title_id != 0).then_some(Game {
            msa_app_id,
            title_id,
            family,
            name,
        })
    }

    /// The SCID a game uses unless it names its own: the title id in the last 32 bits.
    pub fn default_scid(&self) -> String {
        format!("00000000-0000-0000-0000-0000{:08x}", self.title_id)
    }
}

/// `<prefix>/drive_c/users/<user>/AppData/Local`, as Wine sets `LOCALAPPDATA` for the prefix.
pub fn local_app_data(prefix: &Path) -> Option<PathBuf> {
    let users = prefix.join("drive_c/users");
    // Wine writes the folder of the prefix user into user.reg.
    if let Ok(reg) = std::fs::read_to_string(prefix.join("user.reg")) {
        for line in reg.lines() {
            let Some(value) = line.strip_prefix("\"Local AppData\"=\"") else {
                continue;
            };
            let value = value.trim_end_matches('"').replace("\\\\", "\\");
            if let Some(rest) = value
                .strip_prefix("C:\\users\\")
                .or_else(|| value.strip_prefix("C:\\Users\\"))
            {
                let parts: Vec<&str> = rest.split('\\').collect();
                if parts.iter().all(|p| plain_component(p)) {
                    return Some(users.join(parts.join("/")));
                }
            }
        }
    }
    let user = std::env::var("USER").ok().filter(|u| plain_component(u))?;
    Some(users.join(user).join("AppData/Local"))
}

fn plain_component(part: &str) -> bool {
    !part.is_empty() && part != "." && part != ".." && !part.contains(['/', '\0'])
}

/// The folder xgameruntime keeps the saves of `xuid` in for one SCID.
pub fn save_dir(prefix: &Path, game: &Game, xuid: &str, scid: &str) -> Option<PathBuf> {
    if !plain_component(&game.family) || !is_xuid(xuid) || !is_scid(scid) {
        return None;
    }
    Some(
        local_app_data(prefix)?
            .join("Packages")
            .join(&game.family)
            .join("LocalState/GameSave")
            .join(xuid)
            .join(scid.to_lowercase()),
    )
}

fn is_xuid(xuid: &str) -> bool {
    !xuid.is_empty() && xuid.len() <= 20 && xuid.bytes().all(|b| b.is_ascii_digit())
}

fn is_scid(scid: &str) -> bool {
    scid.len() == 36
        && scid.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

/// Percent encoding of names on disk, the same as xgameruntime's: letters, digits, `-`, `_` and a `.` that is not
/// the first character stay, every other byte becomes `%XX`.
pub fn encode_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, b) in name.bytes().enumerate() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || (b == b'.' && i > 0) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn decode_name(name: &str) -> Option<String> {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            if let Ok(value) = u8::from_str_radix(hex, 16) {
                out.push(value);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

// ---------------------------------------------------------------------------------------------------------------
// Local containers
// ---------------------------------------------------------------------------------------------------------------

/// A container as stored locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalContainer {
    pub name: String,
    pub display_name: Option<String>,
    /// Blob name → data.
    pub blobs: BTreeMap<String, Vec<u8>>,
    pub modified: SystemTime,
}

impl LocalContainer {
    /// Hash of the names and contents of the blobs; what tells whether the container changed.
    pub fn hash(&self) -> String {
        let mut hash = Sha256::new();
        for (name, data) in &self.blobs {
            hash.update((name.len() as u64).to_le_bytes());
            hash.update(name.as_bytes());
            hash.update((data.len() as u64).to_le_bytes());
            hash.update(data);
        }
        hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Reads all containers of a save folder. Names that do not decode are left out.
pub fn read_local(dir: &Path) -> std::io::Result<BTreeMap<String, LocalContainer>> {
    let mut containers = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(containers),
        Err(err) => return Err(err),
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(encoded) = file_name.to_str() else {
            continue;
        };
        if encoded.starts_with('.') || !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = decode_name(encoded) else {
            continue;
        };
        containers.insert(name.clone(), read_container(&entry.path(), name)?);
    }
    Ok(containers)
}

fn read_container(dir: &Path, name: String) -> std::io::Result<LocalContainer> {
    let mut blobs = BTreeMap::new();
    let mut display_name = None;
    let mut modified = SystemTime::UNIX_EPOCH;
    for entry in std::fs::read_dir(dir)?.flatten() {
        let file_name = entry.file_name();
        let Some(encoded) = file_name.to_str() else {
            continue;
        };
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }
        if encoded == DISPLAY_NAME_FILE {
            display_name = std::fs::read_to_string(entry.path())
                .ok()
                .filter(|n| !n.is_empty());
            continue;
        }
        if encoded.starts_with('.') {
            continue;
        }
        let Some(blob) = decode_name(encoded) else {
            continue;
        };
        modified = modified.max(meta.modified()?);
        blobs.insert(blob, std::fs::read(entry.path())?);
    }
    Ok(LocalContainer {
        name,
        display_name,
        blobs,
        modified,
    })
}

/// Writes a container, replacing what was there: new files first, then a rename into place, so a failure leaves
/// either the old or the new container.
fn write_container(dir: &Path, container: &LocalContainer) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let target = dir.join(encode_name(&container.name));
    let temp = dir.join(format!(".sync-{}", encode_name(&container.name)));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir(&temp)?;
    for (name, data) in &container.blobs {
        let file = std::fs::File::create(temp.join(encode_name(name)))?;
        std::io::Write::write_all(&mut &file, data)?;
        // Keep the time the save was made, so the newer side can be told on a later conflict.
        if container.modified > SystemTime::UNIX_EPOCH {
            file.set_modified(container.modified)?;
        }
    }
    if let Some(display) = &container.display_name {
        std::fs::write(temp.join(DISPLAY_NAME_FILE), display)?;
    }
    let old = dir.join(format!(".old-{}", encode_name(&container.name)));
    let _ = std::fs::remove_dir_all(&old);
    if target.exists() {
        std::fs::rename(&target, &old)?;
    }
    std::fs::rename(&temp, &target)?;
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Backups
// ---------------------------------------------------------------------------------------------------------------

/// Where backups of one game go: `$XDG_STATE_HOME/xtux/save-backups/<family>/`.
fn backup_root(state: &Path, game: &Game) -> PathBuf {
    state.join("save-backups").join(&game.family)
}

/// Keeps a copy of a container before a sync replaces or deletes it.
fn backup(
    state: &Path,
    game: &Game,
    run: &str,
    side: &str,
    container: &LocalContainer,
) -> Result<()> {
    let dir = backup_root(state, game).join(run).join(side);
    write_container(&dir, container)
        .map_err(|e| format!("backup of {} failed: {e}", container.name))?;
    tracing::info!(
        "backed up {side} save {} to {}",
        container.name,
        dir.display()
    );
    Ok(())
}

/// Removes all but the newest [`MAX_BACKUPS`] backups of a game.
fn prune_backups(state: &Path, game: &Game) {
    let root = backup_root(state, game);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let mut runs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    runs.sort();
    while runs.len() > MAX_BACKUPS {
        let _ = std::fs::remove_dir_all(runs.remove(0));
    }
}

fn run_name() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ").to_string()
}

// ---------------------------------------------------------------------------------------------------------------
// Sync state
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct State {
    containers: BTreeMap<String, Synced>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Synced {
    /// Cloud etag after the last sync.
    etag: String,
    /// [`LocalContainer::hash`] after the last sync.
    hash: String,
}

fn state_file(state: &Path, game: &Game, xuid: &str, scid: &str, dir: &Path) -> PathBuf {
    // Per save folder: a new prefix has saves of its own, whatever an earlier one synced.
    let folder: String = Sha256::digest(dir.as_os_str().as_encoded_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    state
        .join("cloud-saves")
        .join(&game.family)
        .join(xuid)
        .join(format!("{}-{folder}.json", scid.to_lowercase()))
}

fn load_state(path: &Path) -> State {
    std::fs::read(path)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

fn save_state(path: &Path, state: &State) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let data = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, data).map_err(|e| e.to_string())?;
    std::fs::rename(&temp, path).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------------------------------
// Cloud
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudContainer {
    pub file_name: String,
    pub etag: String,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub client_file_time: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

impl CloudContainer {
    /// The container name, for entries that are containers (`<name>,savedgame`).
    pub fn name(&self) -> Option<&str> {
        self.file_name.strip_suffix(",savedgame")
    }

    /// When the container was last written by a client.
    pub fn modified(&self) -> Option<SystemTime> {
        let time = chrono::DateTime::parse_from_rfc3339(self.client_file_time.as_deref()?).ok()?;
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(time.timestamp().max(0) as u64))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContainerList {
    #[serde(default)]
    blobs: Vec<CloudContainer>,
    #[serde(default)]
    paging_info: Option<PagingInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PagingInfo {
    #[serde(default)]
    continuation_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Atom {
    atom: String,
    name: String,
    size: i64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Atoms {
    atoms: Vec<Atom>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AtomCreation {
    size: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AtomUpload {
    blob_uri: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AtomCommit {
    block_ids: Vec<String>,
    size: i64,
}

/// A signed in connection to one game's cloud saves.
pub struct Cloud {
    client: Client,
    authorization: String,
    pub xuid: String,
    pfn: String,
    /// The service, [`HOST`] except in tests.
    base: String,
}

impl Cloud {
    /// Signs in for the game (title-bound, like the game itself on Windows).
    pub async fn connect(client: &Client, tokens: &TokenManager, game: &Game) -> Result<Cloud> {
        let mut session =
            TitleSession::establish(client, tokens, &game.msa_app_id, i64::from(game.title_id))
                .await
                .map_err(|e| format!("Xbox Live sign-in for {} failed: {e}", game.name))?;
        // Signed in through the game (sisu with its MSA app id), but the storage service refuses tokens that carry
        // the title claim: device and user token only.
        let token = session
            .authenticator
            .get_xsts_token(
                Some(&session.device_token),
                None,
                Some(&session.user_token),
                RELYING_PARTY,
            )
            .await
            .map_err(|e| format!("Xbox Live token for {} failed: {e}", game.name))?;
        let identity = UserIdentity::from_xsts(&token).ok_or("Xbox Live token without a user")?;
        let uhs = identity.uhs.ok_or("Xbox Live token without a user hash")?;
        let xuid = identity
            .xuid
            .filter(|x| is_xuid(x))
            .ok_or("Xbox Live token without a XUID")?;
        Ok(Cloud {
            client: client.clone(),
            authorization: format!("XBL3.0 x={uhs};{}", token.token),
            xuid,
            pfn: game.family.clone(),
            base: HOST.to_string(),
        })
    }

    fn url(&self, scid: &str, rest: &str) -> String {
        format!(
            "{}/users/xuid({})/scids/{}{rest}",
            self.base,
            self.xuid,
            scid.to_lowercase()
        )
    }

    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .timeout(TIMEOUT)
            .header("x-xbl-contract-version", "2")
            .header("Authorization", &self.authorization)
            .header("x-xbl-pfn", &self.pfn)
    }

    async fn check(response: reqwest::Response, what: &str) -> Result<reqwest::Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().await.unwrap_or_default();
        Err(format!(
            "{what} failed: {status} {}",
            body.chars().take(300).collect::<String>()
        ))
    }

    /// All containers of a SCID; none when the game never saved to the cloud. `None` when the SCID has no cloud
    /// storage at all (the service does not know it for this title).
    pub async fn list(&self, scid: &str) -> Result<Option<Vec<CloudContainer>>> {
        match self.list_all(scid).await {
            Err(err) if err.contains("\"code\":2000") => {
                tracing::info!("no cloud storage for SCID {scid}");
                Ok(None)
            }
            other => other.map(Some),
        }
    }

    async fn list_all(&self, scid: &str) -> Result<Vec<CloudContainer>> {
        let mut all = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut url = reqwest::Url::parse(&self.url(scid, "")).map_err(|e| e.to_string())?;
            if let Some(token) = &token {
                url.query_pairs_mut()
                    .append_pair("continuationToken", token);
            }
            let response = self
                .request(reqwest::Method::GET, url.as_str())
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if response.status() == StatusCode::NOT_FOUND {
                return Ok(all);
            }
            let list: ContainerList = Self::check(response, "listing the cloud saves")
                .await?
                .json()
                .await
                .map_err(|e| e.to_string())?;
            all.extend(list.blobs);
            token = list
                .paging_info
                .and_then(|p| p.continuation_token)
                .filter(|t| !t.is_empty());
            if token.is_none() {
                return Ok(all);
            }
        }
    }

    /// Downloads a container with all its blobs.
    pub async fn download(&self, scid: &str, container: &CloudContainer) -> Result<LocalContainer> {
        let name = container.name().ok_or("not a container")?;
        let url = self.url(scid, &format!("/savedgames/{}", path_segment(name)));
        let atoms: Atoms = Self::check(
            self.request(reqwest::Method::GET, &url)
                .send()
                .await
                .map_err(|e| e.to_string())?,
            "reading a cloud save",
        )
        .await?
        .json()
        .await
        .map_err(|e| e.to_string())?;
        let mut blobs = BTreeMap::new();
        for atom in atoms.atoms {
            if !is_atom_id(&atom.atom) {
                return Err(format!("unexpected atom id {}", atom.atom));
            }
            let url = self.url(scid, &format!("/{},binary", atom.atom));
            let data = Self::check(
                self.request(reqwest::Method::GET, &url)
                    .send()
                    .await
                    .map_err(|e| e.to_string())?,
                "downloading a cloud save",
            )
            .await?
            .bytes()
            .await
            .map_err(|e| e.to_string())?;
            blobs.insert(atom.name, data.to_vec());
        }
        Ok(LocalContainer {
            name: name.to_string(),
            display_name: container.display_name.clone().filter(|n| !n.is_empty()),
            blobs,
            modified: container.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        })
    }

    async fn lock(&self, scid: &str) -> Result<()> {
        let url = format!("{}?friendlyName=xTux", self.url(scid, "/lock"));
        Self::check(
            self.request(reqwest::Method::PUT, &url)
                .header("x-xbl-lock-ver", "1")
                .header("x-xbl-lock-ext", "301")
                .header("content-length", "0")
                .send()
                .await
                .map_err(|e| e.to_string())?,
            "locking the cloud saves",
        )
        .await
        .map(drop)
    }

    async fn unlock(&self, scid: &str) -> Result<()> {
        Self::check(
            self.request(reqwest::Method::DELETE, &self.url(scid, "/lock"))
                .header("x-xbl-lock-ver", "1")
                .header("x-xbl-lock-ext", "300")
                .send()
                .await
                .map_err(|e| e.to_string())?,
            "unlocking the cloud saves",
        )
        .await
        .map(drop)
    }

    async fn upload_atom(&self, scid: &str, data: &[u8]) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string().to_uppercase();
        let url = self.url(scid, &format!("/atoms/{id}"));
        let upload: AtomUpload = Self::check(
            self.request(reqwest::Method::POST, &url)
                .header("x-xbl-lock-ext", "300")
                .json(&AtomCreation {
                    size: data.len() as i64,
                })
                .send()
                .await
                .map_err(|e| e.to_string())?,
            "creating a cloud save blob",
        )
        .await?
        .json()
        .await
        .map_err(|e| e.to_string())?;

        let block =
            base64::engine::general_purpose::STANDARD.encode(uuid::Uuid::new_v4().as_bytes());
        let mut blob_url = reqwest::Url::parse(&upload.blob_uri).map_err(|e| e.to_string())?;
        if blob_url.scheme() != "https" && self.base == HOST {
            return Err("the storage service answered an insecure upload address".into());
        }
        blob_url
            .query_pairs_mut()
            .append_pair("comp", "block")
            .append_pair("blockid", &block);
        // The Azure blob URI carries its own access signature; no Xbox Live token goes there.
        Self::check(
            self.client
                .put(blob_url)
                .timeout(TIMEOUT)
                .header("x-ms-blob-type", "BlockBlob")
                .body(data.to_vec())
                .send()
                .await
                .map_err(|e| e.to_string())?,
            "uploading a cloud save blob",
        )
        .await?;

        Self::check(
            self.request(reqwest::Method::POST, &format!("{url}?commit=true"))
                .header("x-xbl-lock-ext", "300")
                .json(&AtomCommit {
                    block_ids: vec![block],
                    size: data.len() as i64,
                })
                .send()
                .await
                .map_err(|e| e.to_string())?,
            "committing a cloud save blob",
        )
        .await?;
        Ok(id)
    }

    async fn upload_container(&self, scid: &str, container: &LocalContainer) -> Result<()> {
        let mut atoms = Vec::new();
        for (name, data) in &container.blobs {
            atoms.push(Atom {
                atom: self.upload_atom(scid, data).await?,
                name: name.clone(),
                size: data.len() as i64,
            });
        }
        let mut url = reqwest::Url::parse(&self.url(
            scid,
            &format!("/savedgames/{}", path_segment(&container.name)),
        ))
        .map_err(|e| e.to_string())?;
        {
            let mut query = url.query_pairs_mut();
            let time: chrono::DateTime<chrono::Utc> = container.modified.into();
            query.append_pair(
                "clientFileTime",
                &time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            );
            if let Some(display) = &container.display_name {
                query.append_pair("displayName", display);
            }
        }
        Self::check(
            self.request(reqwest::Method::PUT, url.as_str())
                .header("x-xbl-lock-ext", "300")
                .json(&Atoms { atoms })
                .send()
                .await
                .map_err(|e| e.to_string())?,
            "writing a cloud save",
        )
        .await
        .map(drop)
    }
}

/// A container name as one URL path segment.
fn path_segment(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn is_atom_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

// ---------------------------------------------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------------------------------------------

/// Which side wins for containers that were never synced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefer {
    Cloud,
    Local,
}

/// What a sync did, for messages.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub downloaded: Vec<String>,
    pub uploaded: Vec<String>,
    /// Synced containers the cloud no longer has: kept here, uploaded again by the next push.
    pub missing_in_cloud: Vec<String>,
    /// Containers changed on both sides; the newer one won, the other one is in the backups.
    pub conflicts: Vec<String>,
    /// Containers that differ on both sides without an earlier sync (saved here and in the cloud before xTux synced
    /// them): left alone on both sides until [`Saves::resolve`] picks one.
    pub unresolved: Vec<String>,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        *self == Report::default()
    }

    fn merge(&mut self, other: Report) {
        self.downloaded.extend(other.downloaded);
        self.uploaded.extend(other.uploaded);
        self.missing_in_cloud.extend(other.missing_in_cloud);
        self.conflicts.extend(other.conflicts);
        self.unresolved.extend(other.unresolved);
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (count, what) in [
            (self.downloaded.len(), "downloaded"),
            (self.uploaded.len(), "uploaded"),
            (
                self.missing_in_cloud.len(),
                "no longer in the cloud (kept here, sent again)",
            ),
            (
                self.conflicts.len(),
                "changed on both sides (newer kept, other one backed up)",
            ),
            (
                self.unresolved.len(),
                "differ here and in the cloud and were never synced: run `xtux-cli saves resolve <game> cloud|local`",
            ),
        ] {
            if count > 0 {
                parts.push(format!("{count} {what}"));
            }
        }
        if parts.is_empty() {
            "saves are up to date".into()
        } else {
            format!("saves: {}", parts.join(", "))
        }
    }
}

/// One user's saves of one SCID, locally and in the cloud.
pub struct Space<'a> {
    pub cloud: &'a Cloud,
    pub game: &'a Game,
    pub scid: String,
    pub dir: PathBuf,
    /// xTux's state folder: sync state and backups go there.
    pub state: PathBuf,
}

impl Space<'_> {
    fn state_path(&self) -> Result<PathBuf> {
        Ok(state_file(
            &self.state,
            self.game,
            &self.cloud.xuid,
            &self.scid,
            &self.dir,
        ))
    }

    /// Brings in what changed in the cloud since the last sync. Local changes stay for [`Space::push`].
    pub async fn pull(&self) -> Result<Report> {
        let state_path = self.state_path()?;
        let mut state = load_state(&state_path);
        let Some(cloud) = self.cloud.list(&self.scid).await? else {
            return Ok(Report::default());
        };
        // The service has been seen to list no containers for minutes while they were still there (and back
        // later). An empty list after a sync is taken as such a glitch: nothing changes this time.
        if cloud.is_empty() && !state.containers.is_empty() {
            return Err("the Xbox cloud listed no saves although some were synced before; trying again next time".into());
        }
        let local =
            read_local(&self.dir).map_err(|e| format!("reading {}: {e}", self.dir.display()))?;
        let run = run_name();
        let mut report = Report::default();

        let cloud_by_name: HashMap<&str, &CloudContainer> =
            cloud.iter().filter_map(|c| Some((c.name()?, c))).collect();

        for (name, remote) in &cloud_by_name {
            let synced = state.containers.get(*name);
            if synced.is_some_and(|s| s.etag == remote.etag) {
                continue; // The cloud did not change.
            }
            let mine = local.get(*name);
            let local_changed = match (mine, synced) {
                (None, _) => false,
                (Some(mine), Some(synced)) => mine.hash() != synced.hash,
                (Some(_), None) => true,
            };
            let theirs = self.cloud.download(&self.scid, remote).await?;
            if let Some(mine) = mine {
                if mine.hash() == theirs.hash() {
                    // Same content on both sides, e.g. the first sync after saving on both.
                    state.containers.insert(
                        name.to_string(),
                        Synced {
                            etag: remote.etag.clone(),
                            hash: mine.hash(),
                        },
                    );
                    continue;
                }
                if synced.is_none() {
                    report.unresolved.push(name.to_string());
                    continue;
                }
                if local_changed {
                    report.conflicts.push(name.to_string());
                    if mine.modified > theirs.modified {
                        // Ours is newer: keep it (push sends it), back up the cloud version.
                        backup(&self.state, self.game, &run, "cloud", &theirs)?;
                        continue;
                    }
                }
                backup(&self.state, self.game, &run, "local", mine)?;
            }
            write_container(&self.dir, &theirs)
                .map_err(|e| format!("writing the save {name}: {e}"))?;
            state.containers.insert(
                name.to_string(),
                Synced {
                    etag: remote.etag.clone(),
                    hash: theirs.hash(),
                },
            );
            report.downloaded.push(name.to_string());
        }

        // Missing in the cloud although it was synced: never delete it here. A container can vanish from the cloud
        // without anyone deleting it on purpose (seen with Planet Coaster 2), and deleting saves on that ground
        // would lose them. The local one stays and the next push uploads it again; deleting a save on another
        // device therefore does not delete it here.
        let gone: Vec<String> = state
            .containers
            .keys()
            .filter(|name| !cloud_by_name.contains_key(name.as_str()))
            .cloned()
            .collect();
        for name in gone {
            state.containers.remove(&name);
            if local.contains_key(&name) {
                tracing::warn!(
                    "save {name} is no longer in the cloud; it stays here and is uploaded again"
                );
                report.missing_in_cloud.push(name);
            }
        }

        save_state(&state_path, &state)?;
        prune_backups(&self.state, self.game);
        Ok(report)
    }

    /// Sends what changed locally since the last sync.
    pub async fn push(&self) -> Result<Report> {
        let state_path = self.state_path()?;
        let mut state = load_state(&state_path);
        let local =
            read_local(&self.dir).map_err(|e| format!("reading {}: {e}", self.dir.display()))?;

        let changed: Vec<&LocalContainer> = local
            .values()
            .filter(|c| {
                state
                    .containers
                    .get(&c.name)
                    .is_none_or(|s| s.hash != c.hash())
            })
            .collect();
        // Saves deleted here are not deleted in the cloud: an empty or new save folder must never wipe the cloud.
        // Their state stays, so pull does not bring them back while the cloud copy is unchanged.
        let mut report = Report::default();
        if changed.is_empty() {
            return Ok(report);
        }
        // Saves of a SCID without cloud storage stay local.
        if self.cloud.list(&self.scid).await?.is_none() {
            return Ok(report);
        }

        self.cloud.lock(&self.scid).await?;
        let result = self.push_locked(&mut state, &changed, &mut report).await;
        if let Err(err) = self.cloud.unlock(&self.scid).await {
            tracing::warn!("{err}");
        }
        save_state(&state_path, &state)?;
        prune_backups(&self.state, self.game);
        result.map(|()| report)
    }

    /// Settles the containers [`Space::pull`] left as unresolved: `prefer` says which side is kept. The other side
    /// is backed up; a kept local container is uploaded by the next [`Space::push`].
    pub async fn resolve(&self, prefer: Prefer) -> Result<Report> {
        let state_path = self.state_path()?;
        let mut state = load_state(&state_path);
        let Some(cloud) = self.cloud.list(&self.scid).await? else {
            return Ok(Report::default());
        };
        let local =
            read_local(&self.dir).map_err(|e| format!("reading {}: {e}", self.dir.display()))?;
        let run = run_name();
        let mut report = Report::default();
        for remote in &cloud {
            let Some(name) = remote.name() else { continue };
            let Some(mine) = local.get(name) else {
                continue;
            };
            if state.containers.contains_key(name) {
                continue;
            }
            let theirs = self.cloud.download(&self.scid, remote).await?;
            if theirs.hash() == mine.hash() {
                state.containers.insert(
                    name.to_string(),
                    Synced {
                        etag: remote.etag.clone(),
                        hash: mine.hash(),
                    },
                );
                continue;
            }
            match prefer {
                Prefer::Cloud => {
                    backup(&self.state, self.game, &run, "local", mine)?;
                    write_container(&self.dir, &theirs)
                        .map_err(|e| format!("writing the save {name}: {e}"))?;
                    state.containers.insert(
                        name.to_string(),
                        Synced {
                            etag: remote.etag.clone(),
                            hash: theirs.hash(),
                        },
                    );
                    report.downloaded.push(name.to_string());
                }
                Prefer::Local => {
                    backup(&self.state, self.game, &run, "cloud", &theirs)?;
                    // Synced with the cloud as it is now, but with a hash that differs: push uploads ours.
                    state.containers.insert(
                        name.to_string(),
                        Synced {
                            etag: remote.etag.clone(),
                            hash: String::new(),
                        },
                    );
                    report.uploaded.push(name.to_string());
                }
            }
        }
        save_state(&state_path, &state)?;
        prune_backups(&self.state, self.game);
        Ok(report)
    }

    async fn push_locked(
        &self,
        state: &mut State,
        changed: &[&LocalContainer],
        report: &mut Report,
    ) -> Result<()> {
        let cloud = self.cloud.list(&self.scid).await?.unwrap_or_default();
        let cloud_by_name: HashMap<&str, &CloudContainer> =
            cloud.iter().filter_map(|c| Some((c.name()?, c))).collect();
        let run = run_name();

        for mine in changed {
            let remote = cloud_by_name.get(mine.name.as_str());
            let synced = state.containers.get(&mine.name);
            // Changed in the cloud as well since the last sync (another device saved meanwhile).
            if let Some(remote) = remote.filter(|r| synced.is_none_or(|s| s.etag != r.etag)) {
                let theirs = self.cloud.download(&self.scid, remote).await?;
                if theirs.hash() == mine.hash() {
                    state.containers.insert(
                        mine.name.clone(),
                        Synced {
                            etag: remote.etag.clone(),
                            hash: mine.hash(),
                        },
                    );
                    continue;
                }
                if synced.is_none() {
                    report.unresolved.push(mine.name.clone());
                    continue;
                }
                report.conflicts.push(mine.name.clone());
                if theirs.modified > mine.modified {
                    // The cloud one is newer: keep it, bring it here; ours goes to the backups.
                    backup(&self.state, self.game, &run, "local", mine)?;
                    write_container(&self.dir, &theirs)
                        .map_err(|e| format!("writing the save {}: {e}", mine.name))?;
                    state.containers.insert(
                        mine.name.clone(),
                        Synced {
                            etag: remote.etag.clone(),
                            hash: theirs.hash(),
                        },
                    );
                    report.downloaded.push(mine.name.clone());
                    continue;
                }
                backup(&self.state, self.game, &run, "cloud", &theirs)?;
            }
            self.cloud.upload_container(&self.scid, mine).await?;
            report.uploaded.push(mine.name.clone());
            // The etag comes from the listing below.
            state.containers.insert(
                mine.name.clone(),
                Synced {
                    etag: String::new(),
                    hash: mine.hash(),
                },
            );
        }

        // Record the etags the service gave the uploaded containers.
        if !report.uploaded.is_empty() {
            for remote in self.cloud.list(&self.scid).await?.unwrap_or_default() {
                let Some(name) = remote.name() else { continue };
                if let Some(synced) = state.containers.get_mut(name).filter(|s| s.etag.is_empty()) {
                    synced.etag = remote.etag.clone();
                }
            }
        }
        Ok(())
    }
}

/// The SCIDs of a game's saves: those xgameruntime has folders for, plus the default one.
pub fn local_scids(prefix: &Path, game: &Game, xuid: &str) -> Vec<String> {
    let mut scids = vec![game.default_scid()];
    let user_dir = save_dir(prefix, game, xuid, &game.default_scid())
        .and_then(|d| d.parent().map(Path::to_path_buf));
    let entries = user_dir.and_then(|d| std::fs::read_dir(d).ok());
    for entry in entries.into_iter().flatten().flatten() {
        if let Some(name) = entry.file_name().to_str().map(str::to_lowercase)
            && is_scid(&name)
            && !scids.contains(&name)
            && entry.path().is_dir()
        {
            scids.push(name);
        }
    }
    scids
}

/// A game's saves in one Wine prefix, signed in to the cloud.
pub struct Saves {
    pub game: Game,
    pub cloud: Cloud,
    prefix: PathBuf,
}

impl Saves {
    /// Signs in for the game in `game_dir`. `None` when the game has no cloud identity (no MSA app id or title id
    /// in its `MicrosoftGame.config`).
    pub async fn open(
        client: &Client,
        tokens: &TokenManager,
        game_dir: &Path,
        prefix: &Path,
    ) -> Result<Option<Saves>> {
        // Saves belong to an existing prefix; never create one by writing into it.
        if !prefix.join("drive_c/users").is_dir() {
            return Err(format!("{} is not a Wine prefix", prefix.display()));
        }
        let layout = crate::layout::detect(game_dir).ok_or("not a game folder")?;
        let Some(game) =
            crate::library::read_config(&layout.root).and_then(|c| Game::from_config(&c))
        else {
            return Ok(None);
        };
        let cloud = Cloud::connect(client, tokens, &game).await?;
        Ok(Some(Saves {
            game,
            cloud,
            prefix: prefix.to_path_buf(),
        }))
    }

    fn spaces(&self) -> Vec<Space<'_>> {
        let Some(state) = crate::logs::state_dir() else {
            tracing::warn!("no state folder (HOME not set), cloud saves are not synced");
            return Vec::new();
        };
        local_scids(&self.prefix, &self.game, &self.cloud.xuid)
            .into_iter()
            .filter_map(|scid| {
                let dir = save_dir(&self.prefix, &self.game, &self.cloud.xuid, &scid)?;
                Some(Space {
                    cloud: &self.cloud,
                    game: &self.game,
                    scid,
                    dir,
                    state: state.clone(),
                })
            })
            .collect()
    }

    /// The local save folder per SCID.
    pub fn save_dirs(&self) -> Vec<(String, PathBuf)> {
        self.spaces().into_iter().map(|s| (s.scid, s.dir)).collect()
    }

    /// The cloud containers per SCID that has cloud storage.
    pub async fn list(&self) -> Result<Vec<(String, Vec<CloudContainer>)>> {
        let mut all = Vec::new();
        for space in self.spaces() {
            if let Some(containers) = self.cloud.list(&space.scid).await? {
                all.push((space.scid.clone(), containers));
            }
        }
        Ok(all)
    }

    pub async fn pull(&self) -> Result<Report> {
        let mut report = Report::default();
        for space in self.spaces() {
            report.merge(space.pull().await?);
        }
        Ok(report)
    }

    pub async fn push(&self) -> Result<Report> {
        let mut report = Report::default();
        for space in self.spaces() {
            report.merge(space.push().await?);
        }
        Ok(report)
    }

    /// Settles never-synced containers that differ, then pushes a kept local side.
    pub async fn resolve(&self, prefer: Prefer) -> Result<Report> {
        let mut report = Report::default();
        for space in self.spaces() {
            report.merge(space.resolve(prefer).await?);
        }
        if prefer == Prefer::Local {
            let pushed = self.push().await?;
            report.conflicts.extend(pushed.conflicts);
            report.unresolved.extend(pushed.unresolved);
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_encode_like_xgameruntime() {
        assert_eq!(encode_name("park2-9A2F14A6"), "park2-9A2F14A6");
        assert_eq!(encode_name(".hidden/a b"), "%2Ehidden%2Fa%20b");
        assert_eq!(encode_name("a.b"), "a.b");
        for name in ["park2-9A2F14A6", ".hidden/a b", "ä.ö", "100%"] {
            assert_eq!(decode_name(&encode_name(name)).as_deref(), Some(name));
        }
    }

    #[test]
    fn scids_and_xuids_are_checked() {
        assert!(is_scid("00000000-0000-0000-0000-0000682b6cf1"));
        assert!(!is_scid("../../../../etc/passwd-0000-0000-0000"));
        assert!(is_xuid("2535430556564898"));
        assert!(!is_xuid("25354/0"));
    }

    #[test]
    fn default_scid_from_title_id() {
        let game = Game {
            msa_app_id: "x".into(),
            title_id: 0x682b6cf1,
            family: "F_x".into(),
            name: "G".into(),
        };
        assert_eq!(game.default_scid(), "00000000-0000-0000-0000-0000682b6cf1");
    }

    #[test]
    fn local_containers_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let container = LocalContainer {
            name: "slot 1".into(),
            display_name: Some("Park".into()),
            blobs: BTreeMap::from([
                ("data".to_string(), b"abc".to_vec()),
                ("meta/x".to_string(), vec![]),
            ]),
            modified: SystemTime::UNIX_EPOCH,
        };
        write_container(dir.path(), &container).unwrap();
        // Rewriting replaces the blobs.
        let mut second = container.clone();
        second.blobs.remove("meta/x");
        write_container(dir.path(), &second).unwrap();
        let read = read_local(dir.path()).unwrap();
        assert_eq!(read.len(), 1);
        let back = &read["slot 1"];
        assert_eq!(back.display_name.as_deref(), Some("Park"));
        assert_eq!(back.hash(), second.hash());
        assert_ne!(back.hash(), container.hash());
        // Temporary folders of an interrupted write are not containers.
        std::fs::create_dir(dir.path().join(".sync-x")).unwrap();
        assert_eq!(read_local(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn local_app_data_from_the_registry() {
        let prefix = tempfile::tempdir().unwrap();
        std::fs::write(
            prefix.path().join("user.reg"),
            "[Software\\\\Microsoft]\n\"Local AppData\"=\"C:\\\\users\\\\steamuser\\\\AppData\\\\Local\"\n",
        )
        .unwrap();
        assert_eq!(
            local_app_data(prefix.path()).unwrap(),
            prefix.path().join("drive_c/users/steamuser/AppData/Local")
        );
    }
}

/// The sync logic against an in-memory stand-in of the connected storage service.
#[cfg(test)]
mod sync_tests {
    use super::*;
    use axum::body::Bytes;
    use axum::http::{Method, StatusCode as Status, Uri};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Stored {
        etag: String,
        time: String,
        display: Option<String>,
        atoms: Vec<Atom>,
    }

    #[derive(Default)]
    struct Service {
        containers: BTreeMap<String, Stored>,
        atoms: HashMap<String, Vec<u8>>,
        blobs: HashMap<String, Vec<u8>>,
        next_etag: u32,
        /// Answer an empty list, like the glitch seen with the real service.
        empty_list: bool,
        locked: bool,
    }

    type Shared = Arc<Mutex<Service>>;

    async fn handle(
        state: axum::extract::State<(Shared, String)>,
        method: Method,
        uri: Uri,
        body: Bytes,
    ) -> (Status, String) {
        let (service, base) = state.0;
        let mut service = service.lock().unwrap();
        let path = uri.path().to_string();
        let query = uri.query().unwrap_or("");
        if let Some(id) = path.strip_prefix("/blob/") {
            service.blobs.insert(id.to_string(), body.to_vec());
            return (Status::CREATED, String::new());
        }
        let rest = path.split_once("/scids/").map(|(_, r)| r).unwrap_or("");
        let rest = rest.split_once('/').map(|(_, r)| r).unwrap_or("");
        match (method, rest) {
            (Method::GET, "") => {
                let blobs: Vec<serde_json::Value> = if service.empty_list {
                    vec![]
                } else {
                    service
                        .containers
                        .iter()
                        .map(|(name, c)| {
                            serde_json::json!({"fileName": format!("{name},savedgame"), "etag": c.etag,
                                "size": 1, "clientFileTime": c.time, "displayName": c.display})
                        })
                        .collect()
                };
                (
                    Status::OK,
                    serde_json::json!({"blobs": blobs, "pagingInfo": {"totalItems": blobs.len()}})
                        .to_string(),
                )
            }
            (Method::PUT, "lock") => {
                service.locked = true;
                (Status::OK, String::new())
            }
            (Method::DELETE, "lock") => {
                service.locked = false;
                (Status::OK, String::new())
            }
            (Method::POST, r) if r.starts_with("atoms/") => {
                let id = r.trim_start_matches("atoms/").to_string();
                if query.contains("commit=true") {
                    let data = service.blobs.remove(&id).unwrap_or_default();
                    service.atoms.insert(id, data);
                    (Status::OK, String::new())
                } else {
                    assert!(service.locked, "writes need the lock");
                    (
                        Status::OK,
                        serde_json::json!({"blobUri": format!("{base}/blob/{id}")}).to_string(),
                    )
                }
            }
            (Method::PUT, r) if r.starts_with("savedgames/") => {
                assert!(service.locked, "writes need the lock");
                let name = decode_url(r.trim_start_matches("savedgames/"));
                let atoms: Atoms = serde_json::from_slice(&body).unwrap();
                let params: HashMap<String, String> =
                    reqwest::Url::parse(&format!("http://x/?{query}"))
                        .unwrap()
                        .query_pairs()
                        .map(|(k, v)| (k.into_owned(), v.into_owned()))
                        .collect();
                service.next_etag += 1;
                let etag = format!("\"e{}\"", service.next_etag);
                service.containers.insert(
                    name,
                    Stored {
                        etag,
                        time: params.get("clientFileTime").cloned().unwrap_or_default(),
                        display: params.get("displayName").cloned(),
                        atoms: atoms.atoms,
                    },
                );
                (Status::OK, String::new())
            }
            (Method::GET, r) if r.starts_with("savedgames/") => {
                let name = decode_url(r.trim_start_matches("savedgames/"));
                match service.containers.get(&name) {
                    Some(c) => (
                        Status::OK,
                        serde_json::to_string(&Atoms {
                            atoms: c.atoms.clone(),
                        })
                        .unwrap(),
                    ),
                    None => (Status::NOT_FOUND, String::new()),
                }
            }
            (Method::GET, r) if r.ends_with(",binary") => {
                let id = r.trim_end_matches(",binary");
                match service.atoms.get(id) {
                    // Atoms are bytes; the tests only store UTF-8.
                    Some(data) => (Status::OK, String::from_utf8(data.clone()).unwrap()),
                    None => (Status::NOT_FOUND, String::new()),
                }
            }
            other => panic!("unexpected request {other:?}"),
        }
    }

    fn decode_url(text: &str) -> String {
        reqwest::Url::parse(&format!("http://x/{text}"))
            .unwrap()
            .path_segments()
            .unwrap()
            .next()
            .map(|s| decode_name(s).unwrap())
            .unwrap()
    }

    async fn start() -> (Shared, String) {
        let service: Shared = Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = axum::Router::new()
            .fallback(handle)
            .with_state((service.clone(), base.clone()));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (service, base)
    }

    /// Puts a container straight into the stand-in, as another device would.
    fn store_remote(service: &Shared, name: &str, data: &str, time: &str) {
        let mut service = service.lock().unwrap();
        let id = uuid::Uuid::new_v4().to_string().to_uppercase();
        service.atoms.insert(id.clone(), data.as_bytes().to_vec());
        service.next_etag += 1;
        let etag = format!("\"e{}\"", service.next_etag);
        service.containers.insert(
            name.into(),
            Stored {
                etag,
                time: time.into(),
                display: None,
                atoms: vec![Atom {
                    atom: id,
                    name: "data".into(),
                    size: data.len() as i64,
                }],
            },
        );
    }

    fn remote_data(service: &Shared, name: &str) -> Option<String> {
        let service = service.lock().unwrap();
        let container = service.containers.get(name)?;
        let atom = container.atoms.iter().find(|a| a.name == "data")?;
        Some(String::from_utf8(service.atoms[&atom.atom].clone()).unwrap())
    }

    fn write_local(dir: &Path, name: &str, data: &str, secs: u64) {
        let container = LocalContainer {
            name: name.into(),
            display_name: None,
            blobs: BTreeMap::from([("data".to_string(), data.as_bytes().to_vec())]),
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
        };
        write_container(dir, &container).unwrap();
    }

    fn local_data(dir: &Path, name: &str) -> Option<String> {
        let containers = read_local(dir).unwrap();
        Some(String::from_utf8(containers.get(name)?.blobs["data"].clone()).unwrap())
    }

    #[tokio::test]
    async fn syncs_both_ways_and_never_loses_a_save() {
        let state = tempfile::tempdir().unwrap();
        let saves = tempfile::tempdir().unwrap();
        let dir = saves.path().to_path_buf();
        let (service, base) = start().await;
        let game = Game {
            msa_app_id: "x".into(),
            title_id: 0x1234,
            family: "Test.Game_abc".into(),
            name: "Test".into(),
        };
        let cloud = Cloud {
            client: Client::new(),
            authorization: "XBL3.0 x=1;t".into(),
            xuid: "1".into(),
            pfn: game.family.clone(),
            base,
        };
        let space = Space {
            cloud: &cloud,
            game: &game,
            scid: game.default_scid(),
            dir: dir.clone(),
            state: state.path().to_path_buf(),
        };

        // A save made on another device comes down; a second pull has nothing to do.
        store_remote(
            &service,
            "slot1",
            "from xbox",
            "2026-01-01T10:00:00.0000000Z",
        );
        let report = space.pull().await.unwrap();
        assert_eq!(report.downloaded, vec!["slot1"]);
        assert_eq!(local_data(&dir, "slot1").as_deref(), Some("from xbox"));
        assert!(space.pull().await.unwrap().is_empty());

        // A save made here goes up, and comes back the same.
        write_local(&dir, "slot2", "made on linux", 1_800_000_000);
        let report = space.push().await.unwrap();
        assert_eq!(report.uploaded, vec!["slot2"]);
        assert_eq!(
            remote_data(&service, "slot2").as_deref(),
            Some("made on linux")
        );
        assert!(space.pull().await.unwrap().is_empty());
        assert!(space.push().await.unwrap().is_empty());

        // Changed on both sides after a sync: the newer one wins, the other one is backed up.
        write_local(&dir, "slot1", "played here", 1_900_000_000);
        store_remote(
            &service,
            "slot1",
            "played on xbox",
            "2026-01-02T10:00:00.0000000Z",
        );
        let report = space.pull().await.unwrap();
        assert_eq!(report.conflicts, vec!["slot1"]);
        assert_eq!(
            local_data(&dir, "slot1").as_deref(),
            Some("played here"),
            "ours is newer (2030 > 2026)"
        );
        let report = space.push().await.unwrap();
        assert_eq!(report.uploaded, vec!["slot1"]);
        assert_eq!(
            remote_data(&service, "slot1").as_deref(),
            Some("played here")
        );
        let backups = state.path().join("save-backups/Test.Game_abc");
        let backed_up: Vec<_> = std::fs::read_dir(&backups).unwrap().flatten().collect();
        assert!(backed_up.iter().any(
            |run| local_data(&run.path().join("cloud"), "slot1").as_deref()
                == Some("played on xbox")
        ));

        // The cloud listing nothing for a while must not touch the saves here.
        service.lock().unwrap().empty_list = true;
        assert!(space.pull().await.is_err());
        service.lock().unwrap().empty_list = false;
        assert_eq!(local_data(&dir, "slot1").as_deref(), Some("played here"));
        assert_eq!(local_data(&dir, "slot2").as_deref(), Some("made on linux"));

        // Deleting a save here neither deletes it in the cloud nor brings it back.
        std::fs::remove_dir_all(dir.join("slot2")).unwrap();
        assert!(space.push().await.unwrap().is_empty());
        assert!(space.pull().await.unwrap().is_empty());
        assert!(remote_data(&service, "slot2").is_some());
        assert!(local_data(&dir, "slot2").is_none());

        // A fresh save folder (another prefix) gets the cloud saves, and an empty one never wipes the cloud.
        let other = tempfile::tempdir().unwrap();
        let fresh = Space {
            cloud: &cloud,
            game: &game,
            scid: game.default_scid(),
            dir: other.path().to_path_buf(),
            state: state.path().to_path_buf(),
        };
        assert!(fresh.push().await.unwrap().is_empty());
        let report = fresh.pull().await.unwrap();
        assert_eq!(report.downloaded.len(), 2);

        // Differing on both sides without an earlier sync: left alone until the user picks a side.
        let third = tempfile::tempdir().unwrap();
        write_local(third.path(), "slot1", "old linux save", 1_950_000_000);
        let unsynced = Space {
            cloud: &cloud,
            game: &game,
            scid: game.default_scid(),
            dir: third.path().to_path_buf(),
            state: state.path().to_path_buf(),
        };
        let report = unsynced.pull().await.unwrap();
        assert_eq!(report.unresolved, vec!["slot1"]);
        assert_eq!(
            local_data(third.path(), "slot1").as_deref(),
            Some("old linux save")
        );
        let report = unsynced.push().await.unwrap();
        assert_eq!(report.unresolved, vec!["slot1"]);
        assert_eq!(
            remote_data(&service, "slot1").as_deref(),
            Some("played here")
        );
        let report = unsynced.resolve(Prefer::Cloud).await.unwrap();
        assert_eq!(report.downloaded, vec!["slot1"]);
        assert_eq!(
            local_data(third.path(), "slot1").as_deref(),
            Some("played here")
        );
        assert!(unsynced.pull().await.unwrap().is_empty());
    }
}
