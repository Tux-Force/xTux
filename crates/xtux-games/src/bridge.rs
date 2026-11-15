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

use crate::license::get_store_tokens;

const MAX_REQUEST: usize = 1024 * 1024;
/// XSTS tokens are valid for many hours; refresh well before that.
const XSTS_LIFETIME: Duration = Duration::from_secs(60 * 60);

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
}

pub struct Bridge {
    pub port: u16,
    pub secret: String,
    /// A license token request of the game failed.
    pub license_failed: Arc<AtomicBool>,
}

/// Starts the bridge on 127.0.0.1 with a random port. It runs until the process exits.
pub async fn start(client: reqwest::Client, tokens: TokenManager) -> std::io::Result<Bridge> {
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
    });
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let state = state.clone();
            tokio::spawn(async move {
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
                .map_err(|err| format!("Xbox Live sign-in for title {title_id} failed: {err}"))?;
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
