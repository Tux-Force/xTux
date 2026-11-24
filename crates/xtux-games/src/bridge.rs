//! Local bridge between the game's xgameruntime and this process.
//!
//! Some GDK calls need data only the Microsoft services can provide, with tokens that Xodus holds
//! (for example `XStoreQueryLicenseToken`). Instead of handing those tokens to the game, `run`
//! starts this bridge on the loopback interface. The game's xgameruntime finds it through
//! `XODUS_BRIDGE_PORT` and authenticates every request with `XODUS_BRIDGE_SECRET`, a random value
//! created for each run.
//!
//! The protocol is a minimal HTTP/1.1 subset with plain text bodies:
//!
//! `POST /store/license-token`
//!   body: line 1 parent product id, line 2 hex encoded custom developer string,
//!   following lines the product ids. Answers the license token.
//!
//! `POST /xbl/token`
//!   body: line 1 HTTP method, line 2 URL, line 3 `1` to force a refresh, line 4 the title id
//!   (decimal, 0 if unknown), line 5 the title's MSA app id (may be empty), line 6 the request
//!   body in base64 (may be empty).
//!   Answers two lines, the Authorization header value and the Signature header value (empty
//!   when the endpoint is not signed), or 204 when the URL does not need an Xbox Live token.
//!   With an MSA app id the token comes from a title-bound session (device, title and user
//!   token, like on Windows) and signed endpoints get a signature; without one it is a
//!   user-only token.
//!
//! `POST /packages`
//!   Answers the installed add-ons (DLC) of the game, see packages.rs for the format.
//!
//! `POST /packages/wait`
//!   body: the generation of the list the runtime knows. Answers the list once it changed, or the
//!   current one after a while (long poll); the runtime then asks again.
//!
//! `POST /store/package-license`
//!   body: the content id of an installed add-on of the game. Answers `1` when the user holds a
//!   license for it (asked once per run from the licensing service), 403 otherwise.
//!
//! `POST /store/products`
//!   body: line 1 `associated` (line 2: the Store ID whose add-ons are wanted) or `products` (the Store IDs on the
//!   following lines). Answers one product per line with the tab separated fields Store ID, kind, SKU, how many
//!   the user owns, whether it has a download, base price, price, currency, end of the offer, availability ID,
//!   in-app offer token, image, its width and height, title and description.
//!
//! `POST /store/consumable`
//!   body: the Store ID of a consumable of the game. Answers how many the user has.
//!
//! `POST /store/consume`
//!   body: line 1 Store ID, line 2 quantity, line 3 tracking id. Answers how many are left.
//!
//! `POST /store/user-key`
//!   body: line 1 `collections` or `purchase`, line 2 the service ticket, line 3 the publisher's user id. Answers
//!   the Store ID key.
//!
//! `POST /xbox/event`
//!   body: service config id, play session id, event name, dimensions and measurements (JSON), a line each.
//!   Answers 204 once the event waits for its upload, 400 for an invalid one.
//!
//! `POST /xbox/achievements`
//!   body: title id (decimal, 0 for the game itself). Answers the achievements as text for the
//!   player, the first line is the title of the window.
//!
//! `POST /xbox/profile`
//!   body: XUID of a player. Answers the profile as text, the first line is the gamertag.
//!
//! `POST /xbox/gamer-picture`
//!   body: line 1 XUID, line 2 the size in pixels (64, 208, 424 or 1080). Answers the PNG in base64.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use xodus::api::xbox::signing;
use xodus::api::xbox::title::{self, ResolvedEndpoint};
use xodus::auth::{TitleSession, XstsToken};
use xodus::models::secrets::Token;
use xodus::models::xbox::TitleMgtResponse;
use xodus::models::xgameruntime::xuser::UserIdentity;
use xodus::tokens::TokenManager;

use crate::cloudsave::Game;
use crate::events;
use crate::license::get_store_tokens;
use crate::packages;
use crate::profile;
use crate::store;

const MAX_REQUEST: usize = 1024 * 1024;
/// Requests handled at the same time; more wait to be accepted.
const MAX_CONNECTIONS: usize = 32;
/// XSTS tokens are valid for many hours; refresh well before that.
const XSTS_LIFETIME: Duration = Duration::from_secs(60 * 60);
const EVENT_PAUSE: Duration = Duration::from_secs(1);
const EVENT_MAX_PAUSE: Duration = Duration::from_secs(300);
/// How long a list of what the user owns is used.
const COLLECTION_AGE: Duration = Duration::from_secs(60);
/// A Store question; xgameruntime itself gives up after 30 seconds.
const STORE_TIMEOUT: Duration = Duration::from_secs(25);
/// A sign-in for the system UI; xgameruntime itself gives up after 30 seconds.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(25);

struct State {
    client: reqwest::Client,
    /// Set when a license token request failed; read after the game exits (see `crash.rs`).
    license_failed: Arc<AtomicBool>,
    tokens: TokenManager,
    secret: String,
    /// Endpoint lists per title id (0 for the default list).
    endpoints: Mutex<HashMap<u32, TitleMgtResponse>>,
    /// User-only Authorization headers per relying party.
    xsts: Mutex<HashMap<String, (String, Instant)>>,
    /// Title-bound sessions per `client_id:title_id`.
    sessions: Mutex<HashMap<String, TitleSession>>,
    /// Title-bound XSTS tokens per `client_id:title_id:relying_party`.
    title_xsts: Mutex<HashMap<String, XstsToken>>,
    /// The add-ons of the game.
    packages: Arc<packages::Watcher>,
    /// Store market for licenses.
    market: String,
    /// Add-on license checks by content id.
    package_licenses: Mutex<HashMap<String, bool>>,
    /// The game, for what its system UI shows; `None` without an Xbox title id.
    game: Option<Game>,
    /// Signed in for the system UI: Authorization header, the user's XUID and since when.
    sign_in: Mutex<Option<(String, String, Instant)>>,
    /// Events the game wrote.
    events: events::Queue,
    /// The user's collection, how many of each product, and since when.
    collection: Mutex<Option<(HashMap<String, u64>, Instant)>>,
    /// Add-ons of products by Store ID.
    addons: Mutex<HashMap<String, Vec<String>>>,
}

pub struct Bridge {
    pub port: u16,
    pub secret: String,
    /// A license token request of the game failed.
    pub license_failed: Arc<AtomicBool>,
    pub events: PendingEvents,
}

/// The events of a game that still wait for their upload.
pub struct PendingEvents(Arc<State>);

impl PendingEvents {
    /// Uploads what the game wrote right before its end.
    pub async fn finish(self) {
        if tokio::time::timeout(Duration::from_secs(10), upload_events(&self.0))
            .await
            .is_err()
        {
            tracing::warn!("the last events of the game could not be uploaded in time");
        }
    }
}

/// Starts the bridge on 127.0.0.1 with a random port. It runs until the process exits. `scope` tells which add-ons
/// belong to the game, `market` is the Store market for their licenses, `game` is what the system UI is about.
pub async fn start(
    client: reqwest::Client,
    tokens: TokenManager,
    scope: packages::Scope,
    market: String,
    game: Option<Game>,
) -> std::io::Result<Bridge> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let secret = random_secret()?;

    let license_failed = Arc::new(AtomicBool::new(false));
    let state = Arc::new(State {
        client,
        license_failed: license_failed.clone(),
        tokens,
        secret: secret.clone(),
        endpoints: Mutex::new(HashMap::new()),
        xsts: Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
        title_xsts: Mutex::new(HashMap::new()),
        packages: packages::Watcher::new(scope),
        market,
        package_licenses: Mutex::new(HashMap::new()),
        game,
        sign_in: Mutex::new(None),
        events: events::Queue::default(),
        collection: Mutex::new(None),
        addons: Mutex::new(HashMap::new()),
    });
    let uploader = state.clone();
    let pending = PendingEvents(state.clone());
    tokio::spawn(async move {
        let mut pause = EVENT_PAUSE;
        loop {
            uploader.events.wait().await;
            // Events come in bursts, and a failed upload is tried again.
            loop {
                tokio::time::sleep(pause).await;
                if upload_events(&uploader).await {
                    pause = EVENT_PAUSE;
                    break;
                }
                pause = (pause * 2).min(EVENT_MAX_PAUSE);
            }
        }
    });
    // A game cannot tie up the process with countless open requests (long polls stay open for a while).
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    tokio::spawn(async move {
        loop {
            let Ok(permit) = slots.clone().acquire_owned().await else {
                break;
            };
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let state = state.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(err) = handle(stream, &state).await {
                    tracing::warn!("request failed: {err}");
                }
            });
        }
    });

    Ok(Bridge {
        port,
        secret,
        license_failed,
        events: pending,
    })
}

fn random_secret() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Compares in constant time, so the secret cannot be guessed byte by byte.
fn secret_matches(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

struct Request {
    method: String,
    path: String,
    secret: Option<String>,
    body: String,
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_REQUEST {
            return Ok(None);
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default().to_string();
    let path = request_line.next().unwrap_or_default().to_string();

    let mut length = 0usize;
    let mut secret = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = value.parse().unwrap_or(0);
        } else if name.eq_ignore_ascii_case("x-xodus-secret") {
            secret = Some(value.to_string());
        }
    }
    if length > MAX_REQUEST {
        return Ok(None);
    }

    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);

    Ok(Some(Request {
        method,
        path,
        secret,
        body: String::from_utf8_lossy(&body).to_string(),
    }))
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

async fn handle(mut stream: TcpStream, state: &Arc<State>) -> std::io::Result<()> {
    let (client, tokens, expected) = (&state.client, &state.tokens, state.secret.as_str());
    let Some(request) = read_request(&mut stream).await? else {
        return respond(&mut stream, "400 Bad Request", "malformed request").await;
    };
    if !request
        .secret
        .as_deref()
        .is_some_and(|s| secret_matches(s, expected))
    {
        return respond(&mut stream, "403 Forbidden", "invalid secret").await;
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/store/license-token") => {
            match license_token(client, tokens, &request.body).await {
                Ok(token) => respond(&mut stream, "200 OK", &token).await,
                Err(err) => {
                    tracing::warn!("license token request failed: {err}");
                    state.license_failed.store(true, Ordering::Relaxed);
                    respond(&mut stream, "502 Bad Gateway", &err).await
                }
            }
        }
        ("POST", "/xbl/token") => match xbl_token(state.clone(), &request.body).await {
            Ok(Some((header, signature))) => {
                respond(&mut stream, "200 OK", &format!("{header}\n{signature}")).await
            }
            Ok(None) => respond(&mut stream, "204 No Content", "").await,
            Err(err) => {
                tracing::warn!("Xbox Live token request failed: {err}");
                respond(&mut stream, "502 Bad Gateway", &err).await
            }
        },
        ("POST", "/packages") => {
            let scope = state.packages.scope().clone();
            let list =
                tokio::task::spawn_blocking(move || packages::encode(&packages::list(&scope)))
                    .await
                    .unwrap_or_default();
            respond(&mut stream, "200 OK", &list).await
        }
        ("POST", "/packages/wait") => {
            let list = state.packages.wait(request.body.trim()).await;
            respond(&mut stream, "200 OK", &list).await
        }
        ("POST", "/store/package-license") => {
            match package_license(state, request.body.trim()).await {
                Ok(true) => respond(&mut stream, "200 OK", "1").await,
                Ok(false) => respond(&mut stream, "403 Forbidden", "not entitled").await,
                Err(err) => {
                    tracing::warn!("add-on license check failed: {err}");
                    respond(&mut stream, "502 Bad Gateway", &err).await
                }
            }
        }
        ("POST", "/xbox/event") => match events::Event::parse(&request.body) {
            Ok(event) if state.game.is_some() => {
                state.events.push(event).await;
                respond(&mut stream, "204 No Content", "").await
            }
            Ok(_) => respond(&mut stream, "400 Bad Request", "not an Xbox title").await,
            Err(err) => {
                tracing::warn!("event of the game refused: {err}");
                respond(&mut stream, "400 Bad Request", &err).await
            }
        },
        (
            "POST",
            path @ ("/store/products" | "/store/consumable" | "/store/consume" | "/store/user-key"),
        ) => {
            let answer =
                tokio::time::timeout(STORE_TIMEOUT, store_request(state, path, &request.body))
                    .await
                    .unwrap_or_else(|_| Err("the Store did not answer in time".into()));
            match answer {
                Ok(text) => respond(&mut stream, "200 OK", &text).await,
                Err(err) => {
                    tracing::warn!("{path} failed: {err}");
                    respond(&mut stream, "502 Bad Gateway", &err).await
                }
            }
        }
        ("POST", path @ ("/xbox/achievements" | "/xbox/profile" | "/xbox/gamer-picture")) => {
            let answer = system_ui(state, path, &request.body).await;
            match answer {
                Ok(text) => respond(&mut stream, "200 OK", &text).await,
                Err(err) => {
                    tracing::warn!("{path} failed: {err}");
                    respond(&mut stream, "502 Bad Gateway", &err).await
                }
            }
        }
        _ => respond(&mut stream, "404 Not Found", "unknown endpoint").await,
    }
}

fn decode_hex(value: &str) -> Option<String> {
    if value.len() % 2 != 0 {
        return None;
    }
    let bytes = (0..value.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&value[i..i + 2], 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    String::from_utf8(bytes).ok()
}

async fn license_token(
    client: &reqwest::Client,
    tokens: &TokenManager,
    body: &str,
) -> Result<String, String> {
    let mut lines = body.lines();
    let parent = lines.next().unwrap_or_default().trim().to_string();
    let custom = decode_hex(lines.next().unwrap_or_default().trim())
        .ok_or("invalid custom developer string")?;
    let products: Vec<String> = lines
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if parent.is_empty() {
        return Err("missing parent product id".to_string());
    }

    let store = get_store_tokens(client, tokens)
        .await
        .map_err(|e| e.to_string())?;
    xodus::licensing::content::get_license_token(
        client,
        store.device,
        store.user,
        store.ticket_reference,
        parent,
        products,
        custom,
    )
    .await
    .map_err(|err| err.to_string())
}

/// Whether the user holds a license for the installed add-on `content_id` of the game. Only content ids of add-on
/// folders of the game are asked for; that keeps lookups to installed content, it does not stop a game that writes
/// such a folder itself (it can ask for license tokens of any product through `/store/license-token` anyway).
async fn package_license(state: &State, content_id: &str) -> Result<bool, String> {
    let content_id = content_id.to_uppercase();
    if !crate::layout::is_content_id(&content_id) {
        return Err("not a content id".to_string());
    }
    // Not held across the checks below: one slow license request does not hold up the others.
    if let Some(valid) = state.package_licenses.lock().await.get(&content_id) {
        return Ok(*valid);
    }
    let scope = state.packages.scope().clone();
    let installed = tokio::task::spawn_blocking(move || packages::list(&scope))
        .await
        .map_err(|e| e.to_string())?;
    if !installed.iter().any(|p| p.content_id == content_id) {
        return Ok(false);
    }
    let valid = match crate::license::get_license(
        &state.client,
        &state.tokens,
        content_id.clone(),
        state.market.clone(),
    )
    .await
    {
        Ok(_) => true,
        Err(crate::Error::NotEntitled(description)) => {
            tracing::info!("no license for add-on {content_id}: {description}");
            false
        }
        Err(err) => return Err(err.to_string()),
    };
    state
        .package_licenses
        .lock()
        .await
        .insert(content_id, valid);
    Ok(valid)
}

/// Uploads the waiting events; false when some are left for another try.
async fn upload_events(state: &State) -> bool {
    let send = |list: Vec<events::Event>| async move {
        let tickets = event_tickets(state, false)
            .await
            .map_err(events::UploadError::Retry)?;
        match events::upload(&state.client, events::collector(), &tickets, &list).await {
            Err(events::UploadError::SignIn(_)) => {
                let tickets = event_tickets(state, true)
                    .await
                    .map_err(events::UploadError::Retry)?;
                events::upload(&state.client, events::collector(), &tickets, &list).await
            }
            result => result,
        }
    };
    match state.events.flush(send).await {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!("events of the game not uploaded: {err:?}");
            false
        }
    }
}

async fn event_tickets(state: &State, force_refresh: bool) -> Result<events::Tickets, String> {
    let game = state.game.as_ref().ok_or("not an Xbox title")?;
    let key = ensure_session(state, &game.msa_app_id, game.title_id).await?;
    let (device, _) = title_token(state, &key, events::DEVICE_RELYING_PARTY, force_refresh).await?;
    let (user, _) = title_token(state, &key, events::USER_RELYING_PARTY, force_refresh).await?;
    let xuid = match UserIdentity::from_xsts(&user).and_then(|i| i.xuid) {
        Some(xuid) => xuid,
        // Not every relying party gets the XUID.
        None => sign_in(state).await?.1,
    };
    Ok(events::Tickets {
        title_id: game.title_id,
        xuid,
        device: authorization(&device)?,
        user: authorization(&user)?,
    })
}

async fn store_request(state: &State, path: &str, body: &str) -> Result<String, String> {
    let lines: Vec<&str> = body.lines().map(str::trim).collect();
    let line = |i: usize| lines.get(i).copied().unwrap_or_default();
    match path {
        "/store/products" => {
            let products = match line(0) {
                "associated" => associated_products(state, line(1)).await?,
                "products" => {
                    let ids: Vec<String> = lines[1..]
                        .iter()
                        .filter(|id| crate::art::is_store_id(id))
                        .map(|id| id.to_uppercase())
                        .take(500)
                        .collect();
                    store_products(state, &ids).await?
                }
                _ => return Err("unknown kind of list".into()),
            };
            Ok(store::encode(&products))
        }
        "/store/consumable" => {
            let product = own_consumable(state, line(0)).await?;
            let owned = collection(state, true).await?;
            Ok(owned.get(&product).copied().unwrap_or(0).to_string())
        }
        "/store/consume" => {
            let product = own_consumable(state, line(0)).await?;
            let quantity: u32 = line(1).parse().map_err(|_| "invalid quantity")?;
            let tracking = uuid::Uuid::parse_str(line(2)).map_err(|_| "invalid tracking id")?;
            if quantity == 0 {
                return Err("invalid quantity".into());
            }
            let left =
                store::consume(&state.client, &state.tokens, &product, quantity, tracking).await;
            *state.collection.lock().await = None;
            Ok(left.map_err(|e| e.to_string())?.to_string())
        }
        _ => {
            let key = match line(0) {
                "collections" => store::Key::Collections,
                "purchase" => store::Key::Purchase,
                _ => return Err("unknown kind of key".into()),
            };
            store::user_key(&state.client, &state.tokens, key, line(1), line(2))
                .await
                .map_err(|e| e.to_string())
        }
    }
}

fn store_market(state: &State) -> String {
    crate::catalog::valid_market(Some(&state.market))
}

/// Whether `product` is the game or belongs to it.
fn is_own_product(state: &State, product: &store::Product) -> bool {
    let own = &state.packages.scope().store_ids;
    own.contains(&product.store_id) || product.parents.iter().any(|p| own.contains(p))
}

/// How many of each product the user has.
async fn collection(state: &State, fresh: bool) -> Result<HashMap<String, u64>, String> {
    let mut cached = state.collection.lock().await;
    if let Some((owned, since)) = &*cached
        && !fresh
        && since.elapsed() < COLLECTION_AGE
    {
        return Ok(owned.clone());
    }
    let mut owned = HashMap::new();
    for item in store::collection(&state.client, &state.tokens, &store_market(state))
        .await
        .map_err(|e| e.to_string())?
    {
        *owned.entry(item.product_id).or_insert(0) += item.quantity;
    }
    *cached = Some((owned.clone(), Instant::now()));
    Ok(owned)
}

/// Products with what the user owns of them; of other games' products that stays unknown.
async fn store_products(state: &State, ids: &[String]) -> Result<Vec<store::Product>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let market = store_market(state);
    let (products, owned) = tokio::join!(
        store::products(&state.client, ids, &market),
        collection(state, false)
    );
    let mut products = products.map_err(|e| e.to_string())?;
    // Without an answer about ownership a game would offer to buy what the user has.
    let owned = owned?;
    for product in &mut products {
        if is_own_product(state, product) {
            product.quantity = owned.get(&product.store_id).copied().unwrap_or(0);
        }
    }
    Ok(products)
}

async fn associated_products(state: &State, store_id: &str) -> Result<Vec<store::Product>, String> {
    let store_id = store_id.to_uppercase();
    let known = state.addons.lock().await.get(&store_id).cloned();
    let ids = match known {
        Some(ids) => ids,
        None => {
            let ids = store::addon_ids(&state.client, &store_id, &store_market(state))
                .await
                .map_err(|e| e.to_string())?;
            state.addons.lock().await.insert(store_id, ids.clone());
            ids
        }
    };
    store_products(state, &ids).await
}

/// The Store ID of a consumable of the game.
async fn own_consumable(state: &State, store_id: &str) -> Result<String, String> {
    if !crate::art::is_store_id(store_id) {
        return Err("not a Store ID".into());
    }
    let ids = [store_id.to_uppercase()];
    let product = store::products(&state.client, &ids, &store_market(state))
        .await
        .map_err(|e| e.to_string())?
        .pop()
        .ok_or("the Store does not know this product")?;
    if !is_own_product(state, &product) {
        return Err("not a product of this game".into());
    }
    if !matches!(product.kind.as_str(), "Consumable" | "UnmanagedConsumable") {
        return Err("not a consumable".into());
    }
    Ok(product.store_id)
}

/// What the game's system UI shows.
async fn system_ui(state: &State, path: &str, body: &str) -> Result<String, String> {
    let ask = || async {
        match path {
            "/xbox/achievements" => show_achievements(state, body.trim()).await,
            "/xbox/profile" => show_profile(state, body.trim()).await,
            _ => gamer_picture(state, body).await,
        }
    };
    match ask().await {
        // The sign-in is no longer accepted: once more with a new one.
        Err(err) if err.contains(&reqwest::StatusCode::UNAUTHORIZED.to_string()) => {
            *state.sign_in.lock().await = None;
            ask().await
        }
        answer => answer,
    }
}

/// Authorization header and XUID for the system UI.
async fn sign_in(state: &State) -> Result<(String, String), String> {
    let game = state.game.as_ref().ok_or("not an Xbox title")?;
    let mut sign_in = state.sign_in.lock().await;
    if let Some((authorization, xuid, since)) = &*sign_in
        && since.elapsed() < XSTS_LIFETIME
    {
        return Ok((authorization.clone(), xuid.clone()));
    }
    // Others wait for this one.
    let (authorization, xuid) = tokio::time::timeout(
        SIGN_IN_TIMEOUT,
        crate::cloudsave::xbox_authorization(&state.client, &state.tokens, game),
    )
    .await
    .map_err(|_| "the sign-in did not answer in time")??;
    *sign_in = Some((authorization.clone(), xuid.clone(), Instant::now()));
    Ok((authorization, xuid))
}

async fn show_achievements(state: &State, title_id: &str) -> Result<String, String> {
    let title_id: u32 = title_id.parse().map_err(|_| "invalid title id")?;
    let (authorization, xuid) = sign_in(state).await?;
    let game = state.game.as_ref().ok_or("not an Xbox title")?;
    let (title_id, name) = if title_id == 0 || title_id == game.title_id {
        (game.title_id, game.name.as_str())
    } else {
        (title_id, "")
    };
    let list = crate::achievements::Achievements::new(&state.client, authorization, xuid, title_id)
        .list()
        .await?;
    Ok(crate::achievements::to_text(name, &list))
}

async fn profile(state: &State, xuid: &str) -> Result<profile::Profile, String> {
    let (authorization, _) = sign_in(state).await?;
    profile::fetch(&state.client, &authorization, xuid).await
}

async fn show_profile(state: &State, xuid: &str) -> Result<String, String> {
    Ok(profile(state, xuid).await?.to_text())
}

async fn gamer_picture(state: &State, body: &str) -> Result<String, String> {
    let mut lines = body.lines();
    let xuid = lines.next().unwrap_or_default().trim();
    let pixels = lines
        .next()
        .and_then(|l| l.trim().parse().ok())
        .filter(|p| matches!(p, 64 | 208 | 424 | 1080))
        .ok_or("invalid picture size")?;
    let profile = profile(state, xuid).await?;
    let picture = profile::picture(&profile, pixels).await?;
    Ok(base64::engine::general_purpose::STANDARD.encode(picture))
}

async fn fetch_endpoints(
    client: &reqwest::Client,
    title_id: u32,
    authorization: Option<&str>,
) -> Result<TitleMgtResponse, String> {
    if title_id == 0 {
        return title::get_title_management(client)
            .await
            .map_err(|err| err.to_string());
    }
    // Title tables only list the title's own services; `?type=1` is rejected for them (414).
    let mut request = client
        .get(format!(
            "https://title.mgt.xboxlive.com/titles/{title_id}/endpoints"
        ))
        .header("x-xbl-contract-version", "1");
    if let Some(authorization) = authorization {
        request = request.header("Authorization", authorization);
    }
    request
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|err| err.to_string())?
        .json()
        .await
        .map_err(|err| err.to_string())
}

/// The endpoint table of a title, or the default one (title id 0). Title tables are only
/// served to title-bound tokens, so they need `authorization`; failures fall back to the
/// default table and are not retried.
async fn endpoint_table(
    state: &State,
    title_id: u32,
    authorization: Option<&str>,
) -> Result<TitleMgtResponse, String> {
    let mut endpoints = state.endpoints.lock().await;
    if !endpoints.contains_key(&title_id) {
        let table = match fetch_endpoints(&state.client, title_id, authorization).await {
            Ok(table) => table,
            Err(err) if title_id != 0 => {
                tracing::warn!("no endpoints for title {title_id}: {err}");
                match endpoints.get(&0) {
                    Some(default) => default.clone(),
                    None => fetch_endpoints(&state.client, 0, None).await?,
                }
            }
            Err(err) => return Err(err),
        };
        endpoints.insert(title_id, table);
    }
    Ok(endpoints[&title_id].clone())
}

/// Relying parties to try for a URL: the endpoint table entry, or for unknown `xsts.<service>`
/// hosts (games name their own relying party that way) the usual spellings of it.
fn candidate_endpoints(url: &str, table: &TitleMgtResponse) -> Vec<ResolvedEndpoint> {
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    // resolve_relying_party falls back to the default relying party when nothing matches;
    // for xsts.<service> hosts the service's own relying party is wanted instead.
    let listed = title::get_endpoint(url, table).is_some();
    if !listed && host.starts_with("xsts.") {
        return [
            format!("https://{host}"),
            format!("https://{host}/"),
            format!("http://{host}/"),
            format!("rp://{host}/"),
        ]
        .into_iter()
        .map(|relying_party| ResolvedEndpoint {
            relying_party,
            signature_policy: None,
        })
        .collect();
    }
    title::resolve_relying_party(url, table)
        .into_iter()
        .collect()
}

/// A title table entry for the URL that names a relying party.
fn title_endpoint(url: &str, table: &TitleMgtResponse) -> Option<ResolvedEndpoint> {
    let mut filtered = table.clone();
    filtered.end_points.retain(|e| e.relying_party.is_some());
    title::get_endpoint(url, &filtered)?;
    title::resolve_relying_party(url, &filtered)
}

struct XblRequest {
    method: String,
    url: String,
    force_refresh: bool,
    title_id: u32,
    client_id: String,
    body: Vec<u8>,
}

fn parse_xbl_request(body: &str) -> Result<XblRequest, String> {
    let mut lines = body.split('\n');
    let mut next = || lines.next().unwrap_or_default().trim().to_string();
    let method = next().to_ascii_uppercase();
    let url = next();
    let force_refresh = next() == "1";
    let title_id = next().parse().unwrap_or(0);
    let client_id = next();
    let encoded = next();
    if url.is_empty() {
        return Err("missing url".to_string());
    }
    let body = if encoded.is_empty() {
        Vec::new()
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| "request body is not valid base64".to_string())?
    };
    Ok(XblRequest {
        method: if method.is_empty() {
            "GET".to_string()
        } else {
            method
        },
        url,
        force_refresh,
        title_id,
        client_id,
        body,
    })
}

/// Answers `(authorization, signature)` for a request, `None` when it needs no token.
async fn xbl_token(state: Arc<State>, body: &str) -> Result<Option<(String, String)>, String> {
    let request = parse_xbl_request(body)?;
    if !request.client_id.is_empty() && request.title_id != 0 {
        return title_bound_token(&state, &request).await;
    }

    let table = endpoint_table(&state, 0, None).await?;
    let mut last_err = None;
    for endpoint in candidate_endpoints(&request.url, &table) {
        let rp = endpoint.relying_party;
        match user_only_header(&state, rp.clone(), request.force_refresh).await {
            Ok(header) => {
                tracing::info!("user token for {} via {rp}", request.url);
                return Ok(Some((header, String::new())));
            }
            Err(err) => {
                tracing::warn!("relying party {rp} failed: {err}");
                last_err = Some(err);
            }
        }
    }
    last_err.map_or(Ok(None), Err)
}

/// Title-bound session for a game, established through the sisu flow when needed.
async fn ensure_session(state: &State, client_id: &str, title_id: u32) -> Result<String, String> {
    let key = format!("{client_id}:{title_id}");
    let mut sessions = state.sessions.lock().await;
    if !sessions.get(&key).is_some_and(TitleSession::is_valid) {
        tracing::info!("establishing Xbox Live session for title {title_id}");
        let session =
            TitleSession::establish(&state.client, &state.tokens, client_id, i64::from(title_id))
                .await
                .map_err(|err| {
                    crate::cloudsave::sign_in_error(&format!("title {title_id:08X}"), &*err)
                })?;
        sessions.insert(key.clone(), session);
    }
    Ok(key)
}

/// Title-bound XSTS token for one relying party, cached until it expires.
async fn title_token(
    state: &State,
    session_key: &str,
    relying_party: &str,
    force_refresh: bool,
) -> Result<(XstsToken, xodus::auth::ProofKey), String> {
    let cache_key = format!("{session_key}:{relying_party}");
    let mut sessions = state.sessions.lock().await;
    let session = sessions
        .get_mut(session_key)
        .ok_or("title session vanished")?;
    let proof_key = session.proof_key();

    let mut cache = state.title_xsts.lock().await;
    if !force_refresh {
        if let Some(token) = cache.get(&cache_key).filter(|t| t.check_validity().is_ok()) {
            return Ok((token.clone(), proof_key));
        }
    }
    let token = session
        .get_xsts_token(relying_party)
        .await
        .map_err(|err| format!("XSTS request for {relying_party} failed: {err}"))?;
    cache.insert(cache_key, token.clone());
    Ok((token, proof_key))
}

async fn title_bound_token(
    state: &State,
    request: &XblRequest,
) -> Result<Option<(String, String)>, String> {
    let url = reqwest::Url::parse(&request.url).map_err(|_| "invalid url".to_string())?;
    let session_key = ensure_session(state, &request.client_id, request.title_id).await?;

    // The title's own endpoint table needs a title-bound token for the default relying party.
    let (default_token, _) =
        title_token(state, &session_key, title::DEFAULT_RELYING_PARTY, false).await?;
    let default_auth = authorization(&default_token)?;
    let title_table = endpoint_table(state, request.title_id, Some(&default_auth)).await?;
    let default_table = endpoint_table(state, 0, None).await?;

    // The title's own services win; its catch-all entries carry no relying party.
    let candidates = match title_endpoint(&request.url, &title_table) {
        Some(endpoint) => vec![endpoint],
        None => candidate_endpoints(&request.url, &default_table),
    };

    let mut last_err = None;
    for endpoint in candidates {
        let rp = endpoint.relying_party.clone();
        let (token, proof_key) =
            match title_token(state, &session_key, &rp, request.force_refresh).await {
                Ok(result) => result,
                Err(err) => {
                    tracing::warn!("{err}");
                    last_err = Some(err);
                    continue;
                }
            };
        let header = authorization(&token)?;
        let signature = match &endpoint.signature_policy {
            Some(policy) => signing::sign_request(
                &proof_key,
                i32::from(policy.version),
                policy.max_body_bytes as usize,
                chrono::Utc::now(),
                &request.method,
                &signing::path_and_query(&url),
                &header,
                &request.body,
            )
            .map_err(|err| format!("request signing failed: {err}"))?,
            None => String::new(),
        };
        tracing::info!(
            "title token for {} via {rp}{}",
            request.url,
            if signature.is_empty() { "" } else { ", signed" }
        );
        return Ok(Some((header, signature)));
    }
    last_err.map_or(Ok(None), Err)
}

/// `XBL3.0 x=<uhs>;<token>` for an XSTS token.
fn authorization(token: &XstsToken) -> Result<String, String> {
    let uhs = UserIdentity::from_xsts(token)
        .and_then(|identity| identity.uhs)
        .ok_or("XSTS token without user hash")?;
    Ok(format!("XBL3.0 x={uhs};{}", token.token))
}

async fn user_only_header(
    state: &Arc<State>,
    rp: String,
    force_refresh: bool,
) -> Result<String, String> {
    if !force_refresh {
        if let Some((header, at)) = state.xsts.lock().await.get(&rp) {
            if at.elapsed() < XSTS_LIFETIME {
                return Ok(header.clone());
            }
        }
    }

    let (Ok(Token::Legacy(dev_token)), Ok(Token::Legacy(user_token))) = (
        state.tokens.get_device_sts_token(),
        state.tokens.get_user_sts_token(),
    ) else {
        return Err("no device or user token, log in first".to_string());
    };

    let xsts = xodus::api::xbox::xsts_user_only(&state.client, dev_token, user_token, &rp)
        .await
        .map_err(|err| format!("could not get an XSTS token for {rp}: {err}"))?;

    let header = xodus::api::xbox::get_xsts_auth_header(xsts);
    state
        .xsts
        .lock()
        .await
        .insert(rp, (header.clone(), Instant::now()));
    Ok(header)
}
