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

use std::io::Read;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use xodus::tokens::TokenManager;

use crate::license::get_store_tokens;

const MAX_REQUEST: usize = 64 * 1024;

pub struct Bridge {
    pub port: u16,
    pub secret: String,
}

/// Starts the bridge on 127.0.0.1 with a random port. It runs until the process exits.
pub async fn start(client: reqwest::Client, tokens: TokenManager) -> std::io::Result<Bridge> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let secret = random_secret()?;

    let expected = secret.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let client = client.clone();
            let tokens = tokens.clone();
            let expected = expected.clone();
            tokio::spawn(async move {
                if let Err(err) = handle(stream, &client, &tokens, &expected).await {
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

async fn handle(
    mut stream: TcpStream,
    client: &reqwest::Client,
    tokens: &TokenManager,
    expected: &str,
) -> std::io::Result<()> {
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
