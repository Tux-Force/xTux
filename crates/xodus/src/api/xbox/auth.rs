use crate::models::xbox::{
    UserAuthProperties, UserAuthRequest, XstsPropertyBag, XstsRequest, XstsResponse,
};

async fn post_user_authenticate(
    client: &reqwest::Client,
    rps_ticket: String,
) -> reqwest::Result<reqwest::Response> {
    let body = UserAuthRequest {
        relying_party: "http://auth.xboxlive.com".to_string(),
        token_type: "JWT".to_string(),
        properties: UserAuthProperties {
            auth_method: "RPS".to_string(),
            site_name: "user.auth.xboxlive.com".to_string(),
            rps_ticket,
        },
    };

    client
        .post("https://user.auth.xboxlive.com/user/authenticate")
        .header("Content-Type", "application/json")
        .header("x-xbl-contract-version", "1")
        .json(&body)
        .send()
        .await
}

pub async fn authenticate_xbox_user(
    client: &reqwest::Client,
    rps_ticket: String,
) -> reqwest::Result<XstsResponse> {
    let mut resp = post_user_authenticate(client, rps_ticket.clone()).await?;

    // Local workaround for xodus-gaming/xodus#238: some accounts receive
    // delegation-compact tickets in five-part JWE form that Xbox rejects with
    // HTTP 400 under the `d=` prefix. Retry once with the `t=` prefix.
    if resp.status() == reqwest::StatusCode::BAD_REQUEST {
        if let Some(rest) = rps_ticket.strip_prefix("d=") {
            resp = post_user_authenticate(client, format!("t={rest}")).await?;
        }
    }

    resp.error_for_status()?.json().await
}

pub async fn request_xsts_token(
    client: &reqwest::Client,
    token: String,
    relying_party: &str,
) -> reqwest::Result<XstsResponse> {
    let body = XstsRequest {
        relying_party: Some(relying_party.to_string()),
        token_type: Some("JWT".to_string()),
        properties: XstsPropertyBag {
            user_tokens: Some(vec![token]),
            sandbox_id: Some("RETAIL".to_string()),
            delegation_token: None,
            service_token: None,
        },
    };

    let resp = client
        .post("https://xsts.auth.xboxlive.com/xsts/authorize")
        .header("Content-Type", "application/json")
        .header("x-xbl-contract-version", "1")
        .json(&body)
        .send()
        .await?
        .error_for_status()?;

    resp.json().await
}

pub fn get_xsts_auth_header(xsts: XstsResponse) -> String {
    let uhs = xsts.user_hash().expect("XSTS response missing xui claim");
    format!("XBL3.0 x={uhs};{}", xsts.token)
}
