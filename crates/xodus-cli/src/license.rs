use xodus::licensing::splicense::{DeviceKey, SPLicense};
use xodus::models::live::ExchangeUserTokenOutcome;
use xodus::models::secrets::Token;
use xodus::models::soap;
use xodus::tokens::TokenManager;

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
) -> std::result::Result<(DeviceKey, SPLicense), String> {
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
    .map_err(|err| err.to_string())?;

    let game_splicense = SPLicense::parse_base64(&game_license.splicense_block)
        .expect("could not parse base64 game SPLicense");

    let dev_license = tokens.get_device_license().unwrap();
    let device_license = SPLicense::parse_base64(&dev_license.splicense)
        .expect("could not parse base64 device SPLicense");
    let key = device_license
        .encrypted_device_key
        .unwrap()
        .derive_device_key();
    Ok((key, game_splicense))
}

/// Exchanges the stored device and user tokens for tokens of the licensing service.
pub async fn get_store_tokens(
    client: &reqwest::Client,
    tokens: &TokenManager,
) -> std::result::Result<StoreTokens, String> {
    let dev_token = tokens.get_device_sts_token().unwrap();
    let Token::Legacy(dev_token) = dev_token else {
        return Err("Invalid STS token".to_string());
    };
    let user = tokens.get_user().unwrap();
    let user_token = tokens.get_user_sts_token().unwrap();
    let Token::Legacy(legacy) = user_token else {
        return Err("Unspported user token".to_string());
    };

    let ms_device_token = xodus::api::live::exchange_device_token(
        client,
        dev_token.clone(),
        "{d6d5a677-0872-4ab0-9442-bb792fce85c5}".to_string(),
        "www.microsoft.com".to_owned(),
        Some(soap::PolicyReference::mbi_ssl()),
    )
    .await
    .unwrap();

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
    .expect("Failed to get ms user token");

    let ms_device_token: Token = ms_device_token.into();
    let Token::Compact(ms_device_token) = ms_device_token else {
        return Err("Unsupported token".to_string());
    };

    let user_token: Token = match user_token {
        ExchangeUserTokenOutcome::Fault(_) => {
            return Err("Failed to get exchange MS token".to_string());
        }
        ExchangeUserTokenOutcome::Issued(
            soap::BodyContent::RequestSecurityTokenResponseCollection(mut collection),
        ) => {
            let token = collection.security_tokens.remove(0);
            token.into()
        }
        ExchangeUserTokenOutcome::Issued(soap::BodyContent::RequestSecurityTokenResponse(
            token,
        )) => (*token).into(),
        _ => unreachable!("Only responses are handled"),
    };
    let Token::Compact(user_token) = user_token else {
        return Err("Unsupported token".to_string());
    };

    Ok(StoreTokens {
        device: ms_device_token,
        user: user_token,
        ticket_reference: user.puid,
    })
}
