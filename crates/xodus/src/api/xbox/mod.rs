use crate::models::soap;
use crate::models::xbox::XstsResponse;
use crate::{api::live::ExchangeTokenResponseOutcome, models::secrets::LegacyToken};

pub mod auth;
pub mod signing;
pub mod title;
pub use auth::{authenticate_xbox_user, get_xsts_auth_header, request_xsts_token};

pub async fn run(
    client: &reqwest::Client,
    dev_token: LegacyToken,
    legacy: LegacyToken,
    relying_party: &str,
) -> XstsResponse {
    xsts_user_only(client, dev_token, legacy, relying_party)
        .await
        .expect("Failed to authenticate Xbox user")
}

/// XSTS token for `relying_party` from the stored MSA user token only (no device/title
/// identity, so requests made with it cannot be signed). Non-panicking variant of [`run`].
pub async fn xsts_user_only(
    client: &reqwest::Client,
    dev_token: LegacyToken,
    legacy: LegacyToken,
    relying_party: &str,
) -> Result<XstsResponse, Box<dyn std::error::Error + Send + Sync>> {
    let user_token = crate::api::live::exchange_user_token(
        client,
        legacy,
        "USERNAME".to_string(),
        dev_token,
        None,
        Some("Silent".to_string()),
        "000000004424da1f".to_string(),
        &[(
            "scope=service::user.auth.xboxlive.com::MBI_SSL&api-version=2.0&clientid=000000004424da1f".to_owned(),
            Some(soap::PolicyReference::token_broker()),
        )],
    )
    .await?;

    let Some((user_token, _)) = user_token.get_rps() else {
        return Err("unsupported MSA user token type".into());
    };
    let resp = authenticate_xbox_user(client, user_token).await?;

    Ok(request_xsts_token(client, resp.token, relying_party).await?)
}
