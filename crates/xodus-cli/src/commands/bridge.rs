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
//!   (decimal, optional) for title specific endpoints.
//!   Answers the Authorization header value for the URL's relying party, or 204 when the URL
//!   does not need an Xbox Live token.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use xodus::models::secrets::Token;
use xodus::models::xbox::TitleMgtResponse;
use xodus::tokens::TokenManager;

use crate::license::get_store_tokens;

const MAX_REQUEST: usize = 64 * 1024;
/// XSTS tokens are valid for many hours; refresh well before that.
const XSTS_LIFETIME: Duration = Duration::from_secs(60 * 60);

struct State {
    client: reqwest::Client,
    tokens: TokenManager,
    secret: String,
    /// Endpoint lists per title id (0 for the default list).
    endpoints: Mutex<HashMap<u32, TitleMgtResponse>>,
    /// Authorization headers per relying party.
    xsts: Mutex<HashMap<String, (String, Instant)>>,
}

pub struct Bridge {
    pub port: u16,
    pub secret: String,
}

/// Starts the bridge on 127.0.0.1 with a random port. It runs until the process exits.
pub async fn start(client: reqwest::Client, tokens: TokenManager) -> std::io::Result<Bridge> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let secret = random_secret()?;

    let state = Arc::new(State {
        client,
        tokens,
        secret: secret.clone(),
        endpoints: Mutex::new(HashMap::new()),
        xsts: Mutex::new(HashMap::new()),
    });
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let state = state.clone();
            tokio::spawn(async move {
                if let Err(err) = handle(stream, &state).await {
                    eprintln!("xodus bridge: request failed: {err}");
                }
            });
        }
    });

    Ok(Bridge { port, secret })
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
        ("POST", "/store/license-token") => match license_token(client, tokens, &request.body).await
        {
            Ok(token) => respond(&mut stream, "200 OK", &token).await,
            Err(err) => {
                eprintln!("xodus bridge: license token request failed: {err}");
                respond(&mut stream, "502 Bad Gateway", &err).await
            }
        },
        ("POST", "/xbl/token") => match xbl_token(state.clone(), &request.body).await {
            Ok(Some(header)) => respond(&mut stream, "200 OK", &header).await,
            Ok(None) => respond(&mut stream, "204 No Content", "").await,
            Err(err) => {
                eprintln!("xodus bridge: Xbox Live token request failed: {err}");
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

    let store = get_store_tokens(client, tokens).await?;
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

async fn fetch_endpoints(client: &reqwest::Client, title_id: u32) -> Result<TitleMgtResponse, String> {
    if title_id == 0 {
        return xodus::api::xbox::title::get_title_management(client)
            .await
            .map_err(|err| err.to_string());
    }
    client
        .get(format!(
            "https://title.mgt.xboxlive.com/titles/{title_id}/endpoints?type=1"
        ))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|err| err.to_string())?
        .json()
        .await
        .map_err(|err| err.to_string())
}

/// Finds the relying party for a URL from the Xbox Live title endpoint list.
async fn relying_party(state: &State, url: &str, title_id: u32) -> Result<Option<String>, String> {
    // WebSocket URLs use the endpoints of the matching HTTP scheme.
    let url = if let Some(rest) = url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        url.to_string()
    };

    let mut endpoints = state.endpoints.lock().await;
    if !endpoints.contains_key(&title_id) {
        let response = match fetch_endpoints(&state.client, title_id).await {
            Ok(response) => response,
            // Fall back to the default list if the title has none.
            Err(err) if title_id != 0 => {
                eprintln!("xodus bridge: no endpoints for title {title_id}: {err}");
                fetch_endpoints(&state.client, 0).await?
            }
            Err(err) => return Err(err),
        };
        endpoints.insert(title_id, response);
    }
    let endpoints = &endpoints[&title_id];
    Ok(xodus::api::xbox::title::get_endpoint(&url, endpoints).and_then(|e| e.relying_party.clone()))
}

async fn xbl_token(state: Arc<State>, body: &str) -> Result<Option<String>, String> {
    let mut lines = body.lines();
    let _method = lines.next().unwrap_or_default();
    let url = lines.next().unwrap_or_default().trim().to_string();
    let force_refresh = lines.next().is_some_and(|l| l.trim() == "1");
    let title_id: u32 = lines.next().and_then(|l| l.trim().parse().ok()).unwrap_or(0);
    if url.is_empty() {
        return Err("missing url".to_string());
    }

    let rps = match relying_party(&state, &url, title_id).await? {
        Some(rp) => vec![rp],
        // Title specific services that are missing from the endpoint list: games name their own
        // XSTS relying party directly, e.g. https://xsts.<service>. Try the usual spellings.
        None => match reqwest::Url::parse(&url).ok().and_then(|u| u.host_str().map(str::to_string)) {
            Some(host) if host.starts_with("xsts.") => vec![
                format!("https://{host}"),
                format!("https://{host}/"),
                format!("http://{host}/"),
                format!("rp://{host}/"),
            ],
            _ => return Ok(None),
        },
    };

    let mut last_err = String::new();
    for rp in rps {
        match xsts_header(&state, rp.clone(), force_refresh).await {
            Ok(header) => {
                eprintln!("xodus bridge: token for {url} via relying party {rp}");
                return Ok(Some(header));
            }
            Err(err) => {
                eprintln!("xodus bridge: relying party {rp} failed: {err}");
                last_err = err;
            }
        }
    }
    Err(last_err)
}

async fn xsts_header(state: &Arc<State>, rp: String, force_refresh: bool) -> Result<String, String> {
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

    // The XSTS helper panics on errors; keep that inside its own task.
    let client = state.client.clone();
    let task_rp = rp.clone();
    let xsts = tokio::spawn(async move {
        xodus::api::xbox::run(&client, dev_token, user_token, &task_rp).await
    })
    .await
    .map_err(|_| format!("could not get an XSTS token for {rp}"))?;

    let header = xodus::api::xbox::get_xsts_auth_header(xsts);
    state
        .xsts
        .lock()
        .await
        .insert(rp, (header.clone(), Instant::now()));
    Ok(header)
}
