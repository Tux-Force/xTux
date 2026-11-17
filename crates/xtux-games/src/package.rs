//! Finding a game's content id and its packages.

use xodus::XBOX_LIVE_PACKAGES_PC;
use xodus::api::displaycatalog::find_products_by_id;
use xodus::models::packagespc::{PackageDetails, PackageResponse};
use xodus::models::secrets::Token;
use xodus::tokens::TokenManager;

use crate::{Error, Result};

/// Result of looking up a store product.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentLookup {
    /// The product has a Windows desktop package with this content id.
    Found(String),
    /// The product is a bundle or edition; one of these product ids has to be looked up instead.
    Choose(Vec<String>),
}

/// Looks up the content id of a store product (for example `9NXN8GF8N9ZQ`).
pub async fn find_content_id(
    client: &reqwest::Client,
    product: String,
    market: Option<String>,
) -> Result<ContentLookup> {
    let displaycatalog = find_products_by_id(
        client,
        product,
        market.unwrap_or_else(|| "neutral".to_owned()),
        vec!["en".to_string(), "neutral".to_string()],
    )
    .await
    .map_err(|e| Error::msg(format!("catalog lookup failed: {e}")))?;

    let product_details = displaycatalog.product;

    let mut found_package = None;
    let mut subprods: Vec<String> = vec![];
    'o: for availability in &product_details.display_sku_availabilities {
        for package in &availability.sku.properties.packages {
            if package
                .platform_dependencies
                .iter()
                .any(|dep| dep.platform_name == "Windows.Desktop")
            {
                found_package = Some(package);
                break 'o;
            }
        }
        for availability in &availability.availabilities {
            if let Some(licensing_data) = &availability.licensing_data {
                for satisfies in &licensing_data.satisfying_entitlement_keys {
                    for entitlement_key in &satisfies.entitlement_keys {
                        let key: Vec<&str> = entitlement_key.split(':').collect();
                        if key.len() == 3 && key[0] == "big" {
                            subprods.push(key[1].to_string());
                        }
                    }
                }
            }
        }
    }
    subprods.sort();
    subprods.dedup();

    let Some(package) = found_package else {
        if !subprods.is_empty() {
            return Ok(ContentLookup::Choose(subprods));
        }
        return Err(Error::msg(
            "Windows.Desktop package not found, if you believe this is an error, please report it",
        ));
    };

    package
        .content_id
        .clone()
        .map(ContentLookup::Found)
        .ok_or_else(|| {
            Error::msg("ContentId not found, if you believe this is an error, please report it")
        })
}

/// The base package of a content id, as offered to the signed in user.
pub async fn get_packages(
    client: &reqwest::Client,
    tokens: &TokenManager,
    content_id: String,
) -> Result<PackageDetails> {
    let auth = update_authorization(client, tokens).await?;
    get_packages_authorized(client, &auth, content_id).await
}

/// One authorization can serve an entire update-check batch; never log this value.
pub async fn update_authorization(
    client: &reqwest::Client,
    tokens: &TokenManager,
) -> Result<String> {
    let (Ok(Token::Legacy(dev_token)), Ok(Token::Legacy(legacy))) =
        (tokens.get_device_sts_token(), tokens.get_user_sts_token())
    else {
        return Err(Error::SignedOut("no usable device or user token".into()));
    };

    let xsts_token =
        xodus::api::xbox::xsts_user_only(client, dev_token, legacy, "http://update.xboxlive.com")
            .await
            .map_err(|e| Error::msg(format!("could not get an update token: {e}")))?;

    Ok(xodus::api::xbox::get_xsts_auth_header(xsts_token))
}

pub async fn get_packages_authorized(
    client: &reqwest::Client,
    authorization: &str,
    content_id: String,
) -> Result<PackageDetails> {
    let response = client
        .get(format!(
            "{XBOX_LIVE_PACKAGES_PC}/GetBasePackage/{content_id}"
        ))
        .header("x-xbl-contract-version", "3")
        .header("Authorization", authorization)
        .send()
        .await?
        .error_for_status()?;
    let res: PackageResponse = response.json().await?;

    let PackageResponse::Found(package) = res else {
        return Err(Error::NotEntitled(
            "package was not found, is it owned by the user?".into(),
        ));
    };
    Ok(package)
}
