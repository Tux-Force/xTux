use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use xodus::tokens::TokenManager;

mod activity;
mod connection;
mod simple_context;

const XML_MAGIC: u32 = 0x58445358;
const PROTO_MAGIC: u32 = 0x58445350;

#[tokio::main]
async fn main() {
    let filter = tracing_subscriber::EnvFilter::from_env("XODUS_LOG");
    let registry =
        tracing_subscriber::registry().with(tracing_subscriber::fmt::layer().with_filter(filter));

    #[cfg(feature = "tokio_console")]
    {
        use tracing::level_filters::LevelFilter;
        use tracing_subscriber::filter::Targets;

        let console_filter = Targets::new()
            .with_target("tokio", LevelFilter::TRACE)
            .with_target("runtime", LevelFilter::TRACE);
        let console_layer = console_subscriber::spawn().with_filter(console_filter);
        registry.with(console_layer).init();
    }
    #[cfg(not(feature = "tokio_console"))]
    {
        registry.init();
    }

    xodus::secrets::init_secrets().expect("Failed to init keychain");
    let tokens = Arc::new(TokenManager::with_keychain_and_memory());
    xodus::tokens::device::ensure_device_credentials(&reqwest::Client::new(), &tokens).await;
    // Without a device token the daemon still starts; requests then report the signed-out state.
    let device_token = match tokens.get_device_sts_token() {
        Ok(xodus::models::secrets::Token::Legacy(token)) => Some(token),
        Ok(_) => {
            tracing::warn!("the device token is not a legacy token");
            None
        }
        Err(err) => {
            tracing::warn!("no device token: {err}");
            None
        }
    };

    let shared = Arc::new(simple_context::SharedState::default());
    let cancellation = CancellationToken::new();
    let socket_path = xtux_protocol::socket_path();
    if socket_path.exists() {
        if tokio::net::UnixStream::connect(&socket_path).await.is_ok() {
            tracing::info!("xtux-service is already running");
            return;
        }
        // Left over from a daemon that did not exit cleanly.
        _ = tokio::fs::remove_file(&socket_path).await;
    }
    let activity = activity::Activity::new();
    let idle_timeout = std::env::var("XTUX_IDLE_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(300);
    if idle_timeout > 0 {
        let activity = activity.clone();
        let trigger = cancellation.clone();
        tokio::spawn(async move {
            let limit = std::time::Duration::from_secs(idle_timeout);
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if activity.idle_for().is_some_and(|idle| idle >= limit) {
                    tracing::info!("idle for {idle_timeout} s, exiting");
                    trigger.cancel();
                    break;
                }
            }
        });
    }
    let trigger = cancellation.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failure to handle ctrl_c");
        trigger.cancel();
    });
    {
        let listener = UnixListener::bind(&socket_path).expect("Unable to bind to socket");
        let mode = 0o600;
        let perms = Permissions::from_mode(mode);
        _ = tokio::fs::set_permissions(&socket_path, perms).await;
        loop {
            let accept = tokio::select! {
                r = listener.accept() => r,
                _ = cancellation.cancelled() => break,
            }
            .expect("Failed to accept");

            let token = cancellation.clone();
            let device_token = device_token.clone();
            let tokens = tokens.clone();
            let shared = shared.clone();
            let activity = activity.clone();
            tokio::spawn(async move {
                connection::router::route(accept.0, token, device_token, tokens, shared, activity).await
            });
        }
    }

    _ = tokio::fs::remove_file(socket_path).await;
}
