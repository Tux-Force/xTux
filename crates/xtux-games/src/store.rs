//! The Store for a running game: products with prices, what the user owns, consumables and Store ID keys.
//!
//! Products and prices come from the public Store catalog, what the user owns from the collections service. A game
//! only learns and changes the ownership of its own products.

use std::collections::{HashMap, HashSet};

use futures_util::{FutureExt, StreamExt};
use serde_json::{Value, json};
use xodus::tokens::TokenManager;

use crate::catalog::body;
use crate::{Error, Result};

/// Add-ons per page of the list of a game; the service does not take much more.
const ADDON_PAGE: usize = 20;
const MAX_ADDONS: usize = 1000;
const BATCH: usize = 20;
/// Requests at the same time.
const PARALLEL: usize = 4;

/// An item of the user's collection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Item {
    pub product_id: String,
    pub kind: String,
    pub quantity: u64,
}

/// A product as the game sees it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Product {
    pub store_id: String,
    /// `Game`, `Durable`, `Consumable`, `UnmanagedConsumable` or `Pass`.
    pub kind: String,
    pub sku_id: String,
    pub title: String,
    pub description: String,
    pub offer_token: String,
    pub has_download: bool,
    pub image: String,
    pub image_width: u64,
    pub image_height: u64,
    /// The offer the user can buy; empty when there is none.
    pub availability_id: String,
    pub currency: String,
    pub base_price: f64,
    pub price: f64,
    /// End of the offer (seconds since 1970), 0 without one.
    pub offer_end: i64,
    /// Products this one belongs to or is sold by.
    pub parents: Vec<String>,
    /// How many the user owns, 0 for none.
    pub quantity: u64,
}

fn string(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

fn kind(name: &str) -> Option<&'static str> {
    [
        "Game",
        "Durable",
        "Consumable",
        "UnmanagedConsumable",
        "Pass",
    ]
    .into_iter()
    .find(|k| k.eq_ignore_ascii_case(name))
}

fn has_action(availability: &Value, action: &str) -> bool {
    availability["Actions"]
        .as_array()
        .is_some_and(|a| a.iter().any(|a| a == action))
}

fn end_of(availability: &Value) -> i64 {
    availability["Conditions"]["EndDate"]
        .as_str()
        .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
        // The Store writes "never" as a date in 9998.
        .map(|d| d.timestamp())
        .filter(|t| *t < 250_000_000_000)
        .unwrap_or(0)
}

fn on_desktop(availability: &Value) -> bool {
    match availability["Conditions"]["ClientConditions"]["AllowedPlatforms"].as_array() {
        Some(platforms) => platforms
            .iter()
            .any(|p| p["PlatformName"] == "Windows.Desktop"),
        None => true,
    }
}

/// A product of the display catalog.
fn product(value: &Value) -> Option<Product> {
    let store_id = string(&value["ProductId"]);
    if !crate::art::is_store_id(&store_id) {
        return None;
    }
    let kind = kind(value["ProductKind"].as_str()?)?;
    let localized = &value["LocalizedProperties"][0];
    let empty = Vec::new();
    let skus = value["DisplaySkuAvailabilities"]
        .as_array()
        .unwrap_or(&empty);
    let full: Vec<&Value> = skus
        .iter()
        .filter(|s| s["Sku"]["Properties"]["IsTrial"] != true)
        .collect();
    // The SKU that is for sale, otherwise the standard one.
    let offer = full.iter().find_map(|sku| {
        let offer = sku["Availabilities"]
            .as_array()?
            .iter()
            .find(|a| has_action(a, "Purchase") && on_desktop(a))?;
        Some((*sku, offer))
    });
    let sku = offer.map(|(sku, _)| sku).or_else(|| {
        full.iter()
            .find(|s| s["Sku"]["SkuId"] == "0010")
            .or(full.first())
            .copied()
    });
    let price = |name: &str| {
        offer
            .and_then(|(_, o)| o["OrderManagementData"]["Price"][name].as_f64())
            .filter(|p| p.is_finite() && *p >= 0.0)
            .unwrap_or(0.0)
    };
    let image = localized["Images"].as_array().and_then(|images| {
        ["Poster", "BoxArt", "Logo", "Tile"]
            .iter()
            .find_map(|p| images.iter().find(|i| i["ImagePurpose"] == *p))
            .or(images.first())
    });
    let uri = image.map(|i| string(&i["Uri"])).unwrap_or_default();
    let parents: HashSet<String> = value["MarketProperties"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["RelatedProducts"].as_array().into_iter().flatten())
        .filter(|r| {
            r["RelationshipType"].as_str().is_some_and(|t| {
                t.eq_ignore_ascii_case("addOnParent") || t.eq_ignore_ascii_case("SellableBy")
            })
        })
        .map(|r| string(&r["RelatedProductId"]))
        .collect();
    let (list, msrp) = (price("ListPrice"), price("MSRP"));
    Some(Product {
        kind: kind.to_string(),
        sku_id: sku
            .map(|s| string(&s["Sku"]["SkuId"]))
            .filter(|s| s.len() == 4 && s.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or_else(|| "0010".into()),
        title: string(&localized["ProductTitle"]),
        description: match string(&localized["ShortDescription"]) {
            short if short.is_empty() => string(&localized["ProductDescription"]),
            short => short,
        }
        .chars()
        .take(1200)
        .collect(),
        offer_token: match sku.map(|s| string(&s["Sku"]["Properties"]["InAppOfferToken"])) {
            Some(token) if !token.is_empty() => token,
            _ => string(&value["Properties"]["InAppOfferToken"]),
        },
        has_download: sku.is_some_and(|s| {
            s["Sku"]["Properties"]["Packages"]
                .as_array()
                .is_some_and(|p| !p.is_empty())
        }),
        image: match uri.strip_prefix("//") {
            Some(rest) => format!("https://{rest}"),
            None => uri,
        },
        image_width: image.and_then(|i| i["Width"].as_u64()).unwrap_or(0),
        image_height: image.and_then(|i| i["Height"].as_u64()).unwrap_or(0),
        availability_id: offer
            .map(|(_, o)| string(&o["AvailabilityId"]))
            .unwrap_or_default(),
        currency: offer
            .map(|(_, o)| string(&o["OrderManagementData"]["Price"]["CurrencyCode"]))
            .unwrap_or_default(),
        base_price: msrp.max(list),
        price: list,
        offer_end: offer.map(|(_, o)| end_of(o)).unwrap_or(0),
        parents: parents.into_iter().collect(),
        quantity: 0,
        store_id,
    })
}

/// Products of the display catalog by Store ID, in the order of `ids`. Unknown ones are left out.
pub async fn products(
    client: &reqwest::Client,
    ids: &[String],
    market: &str,
) -> Result<Vec<Product>> {
    let mut batches = Vec::new();
    for batch in ids.chunks(BATCH) {
        let url = format!(
            "https://displaycatalog.mp.microsoft.com/v7.0/products?bigIds={}&market={market}&languages=en-US,neutral",
            batch.join(",")
        );
        batches.push(json_answer(client.get(url)).boxed());
    }
    let answers: Vec<Result<Value>> = futures_util::stream::iter(batches)
        .buffered(PARALLEL)
        .collect()
        .await;
    let mut found = HashMap::new();
    for answer in answers {
        for value in answer?["Products"].as_array().into_iter().flatten() {
            if let Some(product) = product(value) {
                found.insert(product.store_id.to_uppercase(), product);
            }
        }
    }
    Ok(ids
        .iter()
        .filter_map(|id| found.remove(&id.to_uppercase()))
        .collect())
}

async fn json_answer(request: reqwest::RequestBuilder) -> Result<Value> {
    serde_json::from_slice(&body(request.send().await?).await?)
        .map_err(|e| Error::msg(format!("unexpected Store answer: {e}")))
}

/// Store IDs on a page of the add-ons of a game and how many there are in all.
fn addon_page(answer: &Value) -> (Vec<String>, usize) {
    let payload = &answer["Payload"];
    let ids = payload["AddOns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["ProductId"].as_str())
        .filter(|id| crate::art::is_store_id(id))
        .map(str::to_string)
        .collect();
    (ids, payload["TotalItems"].as_u64().unwrap_or(0) as usize)
}

/// Store IDs of the add-ons of a product (public list of the Store).
pub async fn addon_ids(
    client: &reqwest::Client,
    store_id: &str,
    market: &str,
) -> Result<Vec<String>> {
    if !crate::art::is_store_id(store_id) {
        return Err("not a Store ID".into());
    }
    let page = |skip: usize| {
        let url = format!(
            "https://storeedgefd.dsx.mp.microsoft.com/v9.0/products/{store_id}/addOns?market={market}&locale=en-us&deviceFamily=Windows.Desktop&pageSize={ADDON_PAGE}&skipItems={skip}"
        );
        json_answer(client.get(url)).boxed()
    };
    let (mut ids, total) = addon_page(&page(0).await?);
    let rest: Vec<_> = (ADDON_PAGE..total.min(MAX_ADDONS))
        .step_by(ADDON_PAGE)
        .map(page)
        .collect();
    let answers: Vec<Result<Value>> = futures_util::stream::iter(rest)
        .buffered(PARALLEL)
        .collect()
        .await;
    for answer in answers {
        ids.extend(addon_page(&answer?).0);
    }
    let mut seen = HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));
    Ok(ids)
}

/// A client for requests that carry the user's Store token: never follows a redirect with it.
pub fn private_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(25))
        .build()?)
}

/// Everything in the user's collection.
pub async fn collection(
    client: &reqwest::Client,
    tokens: &TokenManager,
    market: &str,
) -> Result<Vec<Item>> {
    let store = crate::license::get_store_tokens(client, tokens).await?;
    let client = private_client()?;
    let mut request = json!({
        "maxPageSize": 100,
        "market": market,
        "validityType": "Valid",
        "productSkuIds": [],
        "beneficiaries": [{
            "identityType": "Msa",
            "identityValue": store.user,
            "localTicketReference": store.ticket_reference,
        }],
        "expandSatisfyingItems": false,
        "excludeDuplicates": true,
    });
    let mut items = Vec::new();
    // A safety limit: 100 pages of 100 items.
    for _ in 0..100 {
        let answer = json_answer(
            client
                .post("https://collections.mp.microsoft.com/v7.0/collections/query")
                .header("Authorization", &store.device)
                .json(&request),
        )
        .await?;
        for item in answer["items"].as_array().into_iter().flatten() {
            let product_id = string(&item["productId"]);
            if crate::art::is_store_id(&product_id) {
                items.push(Item {
                    product_id,
                    kind: string(&item["productKind"]),
                    quantity: item["quantity"].as_u64().unwrap_or(1),
                });
            }
        }
        match answer["continuationToken"].as_str() {
            Some(token) => request["continuationToken"] = token.into(),
            None => break,
        }
    }
    Ok(items)
}

/// Uses up `quantity` of a consumable and answers what is left. The Store counts a tracking ID only once.
pub async fn consume(
    client: &reqwest::Client,
    tokens: &TokenManager,
    store_id: &str,
    quantity: u32,
    tracking_id: uuid::Uuid,
) -> Result<u64> {
    let store = crate::license::get_store_tokens(client, tokens).await?;
    let response = private_client()?
        .post("https://collections.mp.microsoft.com/v7.0/collections/consume")
        .header("Authorization", &store.device)
        .json(&json!({
            "beneficiary": {
                "identityType": "Msa",
                "identityValue": store.user,
                "localTicketReference": store.ticket_reference,
            },
            "productId": store_id,
            "skuId": "0010",
            "trackingId": tracking_id,
            "removeQuantity": quantity,
        }))
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let answer: Value = response.json().await.unwrap_or_default();
        return Err(Error::msg(format!(
            "the Store did not accept it: {status} {}",
            answer["innererror"]["code"].as_str().unwrap_or_default()
        )));
    }
    let answer: Value = serde_json::from_slice(&body(response).await?).unwrap_or_default();
    answer["newQuantity"]
        .as_u64()
        .ok_or_else(|| Error::msg("the Store accepted it, but did not say what is left"))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Key {
    Collections,
    Purchase,
}

/// A Store ID key for the game's own service: lets it ask the Store about this user.
pub async fn user_key(
    client: &reqwest::Client,
    tokens: &TokenManager,
    key: Key,
    service_ticket: &str,
    publisher_user_id: &str,
) -> Result<String> {
    let store = crate::license::get_store_tokens(client, tokens).await?;
    let url = match key {
        Key::Collections => "https://collections.mp.microsoft.com/v7.0/beneficiaries/me/keys",
        Key::Purchase => "https://purchase.mp.microsoft.com/v7.0/users/me/keys",
    };
    let answer = json_answer(
        private_client()?
            .post(url)
            .header("Authorization", &store.user)
            .json(&json!({
                "serviceTicket": service_ticket,
                "publisherUserId": publisher_user_id,
            })),
    )
    .await?;
    answer["key"]
        .as_str()
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::msg("the Store sent no key"))
}

/// `products` as the runtime reads them, see bridge.rs.
pub fn encode(products: &[Product]) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect()
    };
    let mut text = String::new();
    for p in products {
        let fields = [
            clean(&p.store_id),
            clean(&p.kind),
            clean(&p.sku_id),
            p.quantity.to_string(),
            u8::from(p.has_download).to_string(),
            format!("{:.2}", p.base_price),
            format!("{:.2}", p.price),
            clean(&p.currency),
            p.offer_end.to_string(),
            clean(&p.availability_id),
            clean(&p.offer_token),
            clean(&p.image),
            p.image_width.to_string(),
            p.image_height.to_string(),
            clean(&p.title),
            clean(&p.description),
        ];
        text += &fields.join("\t");
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dlc() -> Value {
        json!({
            "ProductId": "9PCR7TSM1S96", "ProductKind": "Durable",
            "LocalizedProperties": [{"ProductTitle": "Rally\tAdventure", "ShortDescription": "", "ProductDescription": "Two\nlines",
                "Images": [{"ImagePurpose": "Poster", "Uri": "//store-images.s-microsoft.com/image/a", "Width": 10, "Height": 20}]}],
            "Properties": {"InAppOfferToken": "rally"},
            "MarketProperties": [{"RelatedProducts": [
                {"RelatedProductId": "9NNX1VVR3KNQ", "RelationshipType": "addOnParent"},
                {"RelatedProductId": "9MZ0SR207MG8", "RelationshipType": "Bundle"}]}],
            "DisplaySkuAvailabilities": [
                {"Sku": {"SkuId": "0011", "Properties": {"IsTrial": true}}, "Availabilities": [{"Actions": ["Purchase"]}]},
                {"Sku": {"SkuId": "0010", "Properties": {"IsTrial": false, "Packages": [{}]}}, "Availabilities": [
                    {"Actions": ["Purchase"], "AvailabilityId": "XBOX", "Conditions": {"ClientConditions": {"AllowedPlatforms": [{"PlatformName": "Windows.Xbox"}]}}},
                    {"Actions": ["Details", "Purchase"], "AvailabilityId": "9PC", "Conditions": {"EndDate": "2027-01-07T09:59:59.0000000Z"},
                     "OrderManagementData": {"Price": {"CurrencyCode": "EUR", "ListPrice": 7.99, "MSRP": 19.99}}}]}]
        })
    }

    #[test]
    fn reads_the_offer_for_pcs() {
        let p = product(&dlc()).unwrap();
        assert_eq!((p.kind.as_str(), p.sku_id.as_str()), ("Durable", "0010"));
        assert_eq!(p.availability_id, "9PC");
        assert_eq!((p.base_price, p.price), (19.99, 7.99));
        assert_eq!(p.offer_end, 1_799_315_999);
        assert_eq!(p.parents, ["9NNX1VVR3KNQ"]);
        assert!(p.has_download);
        let text = encode(&[p]);
        assert_eq!(text.lines().count(), 1);
        assert_eq!(text.trim_end().split('\t').count(), 16);
        assert!(text.contains("Rally Adventure\tTwo lines"));
    }

    #[test]
    fn products_without_an_offer_have_no_price() {
        let mut value = dlc();
        value["DisplaySkuAvailabilities"][1]["Availabilities"] = json!([
            {"Actions": ["License"], "AvailabilityId": "L", "OrderManagementData": {"Price": {"ListPrice": 5.0}}}]);
        let p = product(&value).unwrap();
        assert_eq!((p.availability_id.as_str(), p.price), ("", 0.0));
        value["ProductKind"] = "Application".into();
        assert_eq!(product(&value), None);
        assert_eq!(product(&json!({})), None);
    }

    #[test]
    fn offers_without_an_end() {
        assert_eq!(
            end_of(&json!({"Conditions": {"EndDate": "9998-12-30T00:00:00.0000000Z"}})),
            0
        );
        assert_eq!(end_of(&json!({})), 0);
    }

    #[test]
    fn reads_addon_pages() {
        let answer = json!({"Payload": {"TotalItems": 58, "AddOns": [{"ProductId": "9MT1H69FDCW0"}, {"ProductId": "bad"}]}});
        assert_eq!(addon_page(&answer), (vec!["9MT1H69FDCW0".to_string()], 58));
    }

    /// Asks the real, public Store services.
    #[ignore]
    #[tokio::test]
    async fn live_addons() {
        let client = reqwest::Client::new();
        let ids = addon_ids(&client, "9NNX1VVR3KNQ", "DE").await.unwrap();
        let list = products(&client, &ids, "DE").await.unwrap();
        println!("{} ids, {} products", ids.len(), list.len());
        println!("{}", encode(&list[..3.min(list.len())]));
        assert!(
            list.iter()
                .all(|p| p.parents.contains(&"9NNX1VVR3KNQ".to_string()))
        );
        assert!(list.len() > 20 && list.iter().any(|p| p.price > 0.0));
    }
}
