use xodus::api::live::{ExchangeTokenResponseOutcome, TraceTokenRefreshError};
use xodus::licensing::splicense::{DeviceKey, SPLicense};
use xodus::models::secrets::Token;
use xodus::models::soap;
use xodus::tokens::TokenManager;

use crate::{Error, Result};

/// Tokens for the Microsoft Store licensing service.
#[derive(Clone)]
pub struct StoreTokens {
    pub device: String,
    pub user: String,
    /// Ticket reference of the user (the PUID).
    pub ticket_reference: String,
}

pub async fn get_license(
    client: &reqwest::Client,
    tokens: &TokenManager,
    content_id: String,
    market: String,
) -> Result<(DeviceKey, SPLicense)> {
    let store = get_store_tokens(client, tokens).await?;

    let (_content, game_license) = xodus::licensing::content::get_license_content(
        client,
        store.device,
        store.user,
        store.ticket_reference,
        content_id,
        market,
    )
    .await
    .map_err(|err| match err {
        xodus::licensing::content::LicenseContentError::NotEntitled { description } => Error::NotEntitled(description),
        other => Error::msg(other),
    })?;

    let game_splicense = SPLicense::parse_base64(&game_license.splicense_block)
        .map_err(|e| Error::msg(format!("could not parse the game SPLicense: {e:?}")))?;

    let dev_license = tokens
        .get_device_license()
        .map_err(|e| Error::SignedOut(format!("no device license: {e}")))?;
    let device_license = SPLicense::parse_base64(&dev_license.splicense)
        .map_err(|e| Error::msg(format!("could not parse the device SPLicense: {e:?}")))?;
    let key = device_license
        .encrypted_device_key
        .ok_or("the device license has no device key")?
        .derive_device_key();
    Ok((key, game_splicense))
}

/// Exchanges the stored device and user tokens for tokens of the licensing service.
pub async fn get_store_tokens(
    client: &reqwest::Client,
    tokens: &TokenManager,
) -> Result<StoreTokens> {
    let dev_token = tokens
        .get_device_sts_token()
        .map_err(|e| Error::SignedOut(format!("no device token: {e}")))?;
    let Token::Legacy(dev_token) = dev_token else {
        return Err(Error::SignedOut("invalid device STS token".into()));
    };
    let user = tokens
        .get_user()
        .map_err(|e| Error::SignedOut(format!("no user: {e}")))?;
    let user_token = tokens
        .get_user_sts_token()
        .map_err(|e| Error::SignedOut(format!("no user token: {e}")))?;
    let Token::Legacy(legacy) = user_token else {
        return Err(Error::SignedOut("unsupported user token".into()));
    };

    let ms_device_token = xodus::api::live::exchange_device_token(
        client,
        dev_token.clone(),
        "{d6d5a677-0872-4ab0-9442-bb792fce85c5}".to_string(),
        "www.microsoft.com".to_owned(),
        Some(soap::PolicyReference::mbi_ssl()),
    )
    .await
    .map_err(|e| Error::msg(format!("device token exchange failed: {e}")))?;

    ms_device_token.refresh_device(tokens).trace();

    let user_token = xodus::api::live::exchange_user_token(
        client,
        legacy,
        user.username,
        dev_token,
        None,
        Some("Silent".to_string()),
        "{d6d5a677-0872-4ab0-9442-bb792fce85c5}".to_string(),
        &[(
            "www.microsoft.com".to_owned(),
            Some(soap::PolicyReference::mbi_ssl()),
        )],
    )
    .await
    .map_err(|e| Error::msg(format!("user token exchange failed: {e}")))?;

    let Some((ms_device_token, _)) = ms_device_token.get_rps() else {
        return Err(Error::msg("unsupported token"));
    };

    user_token.refresh_user(tokens).trace();

    let Some((user_token, _)) = user_token.get_rps() else {
        return Err(Error::msg("unsupported token"));
    };

    Ok(StoreTokens {
        device: ms_device_token,
        user: user_token,
        ticket_reference: user.puid,
    })
}
