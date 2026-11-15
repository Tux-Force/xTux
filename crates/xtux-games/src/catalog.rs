//! The store catalog: PC Game Pass titles and the games the signed in account owns, with what xTux can do with
//! them.
//!
//! Game Pass lists come from the public Game Pass catalog, owned games from the Store collections service (with the
//! account's tokens), details from the public Store display catalog. The result is cached for a day in
//! `$XDG_CACHE_HOME/xtux/catalog-<market>.json` (private: it holds the account's game list).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use xodus::tokens::TokenManager;

use crate::{Error, Result};

/// "All PC Games" of PC Game Pass.
const GAME_PASS_PC: &str = "fdd9e2a7-0fee-49f6-ad69-4354098401ff";
/// Details are asked for in batches of this many products.
const BATCH: usize = 20;
/// The cache is refreshed after this time.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_ANSWER_BYTES: usize = 8 * 1024 * 1024;

/// Titles guessed to use a kernel anti-cheat, by the start of their name. xTux's own list, written from general
/// knowledge and not verified per title, so it is off unless the user turns it on (`[ui] anticheat_guess`);
/// installed games are checked by their files instead (anticheat.rs).
const KNOWN_WINDOWS_ONLY: &[(&str, &str)] = &[
    ("Call of Duty", "Ricochet (Activision)"),
    ("Battlefield 2042", "EA AntiCheat"),
    ("Battlefield 6", "EA AntiCheat"),
    ("EA SPORTS FC", "EA AntiCheat"),
    ("Madden NFL", "EA AntiCheat"),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Title {
    /// Store ID (product ID).
    pub product_id: String,
    pub name: String,
    pub publisher: String,
    pub description: String,
    pub category: String,
    /// Content ID of the PC package: what an install downloads and what installed games are named by.
    pub content_id: Option<String>,
    /// Package format of the PC package, for example `MSIXVC`.
    pub package_format: Option<String>,
    pub download_size: Option<u64>,
    /// Image URLs of the Store (store-images.s-microsoft.com).
    pub poster_url: Option<String>,
    pub hero_url: Option<String>,
    pub in_game_pass: bool,
    pub owned: bool,
    /// xTux can install it: a GDK package (MSIXVC) for Windows PCs.
    pub installable: bool,
    /// Kernel anti-cheat known for this title: it runs on Windows only.
    pub windows_only: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub titles: Vec<Title>,
    /// Seconds since the Unix epoch when it was loaded.
    pub fetched_at: u64,
}

/// One cache per market and account, so another account never sees this one's games.
fn cache_file(market: &str, account: &str) -> Option<PathBuf> {
    use sha1::Digest;
    let hash = sha1::Sha1::digest(account.as_bytes());
    let short: String = hash.iter().take(6).map(|b| format!("{b:02x}")).collect();
    Some(
        crate::art::cache_dir()?
            .parent()?
            .join(format!("catalog-{market}-{short}.json")),
    )
}

fn account(tokens: &TokenManager) -> String {
    tokens
        .get_user()
        .map(|u| u.puid)
        .unwrap_or_else(|_| "signed-out".into())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn valid_market(market: Option<&str>) -> String {
    market
        .filter(|m| m.len() == 2 && m.chars().all(|c| c.is_ascii_alphabetic()))
        .unwrap_or("US")
        .to_ascii_uppercase()
}

fn read_cache(market: &str, account: &str) -> Option<Catalog> {
    let bytes = std::fs::read(cache_file(market, account)?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_cache(market: &str, account: &str, catalog: &Catalog) {
    use std::io::Write;
    let Some(file) = cache_file(market, account) else {
        return;
    };
    let Some(dir) = file.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // A unique private temporary file (0600), renamed into place.
    let Ok(mut temp) = tempfile::NamedTempFile::new_in(dir) else {
        return;
    };
    if temp
        .write_all(&serde_json::to_vec(catalog).unwrap_or_default())
        .is_ok()
    {
        let _ = temp.persist(&file);
    }
}

/// Reads an answer of at most `MAX_ANSWER_BYTES`.
async fn body(response: reqwest::Response) -> Result<Vec<u8>> {
    let mut response = response.error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > MAX_ANSWER_BYTES {
            return Err(Error::msg("answer too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Product IDs of PC Game Pass (public list).
pub async fn game_pass_ids(client: &reqwest::Client, market: &str) -> Result<Vec<String>> {
    let url = format!(
        "https://catalog.gamepass.com/sigls/v2?id={GAME_PASS_PC}&language=en-us&market={market}"
    );
    let list: Vec<serde_json::Value> =
        serde_json::from_slice(&body(client.get(url).send().await?).await?)
            .map_err(|e| Error::msg(format!("unexpected Game Pass list: {e}")))?;
    Ok(list
        .iter()
        .filter_map(|entry| entry.get("id")?.as_str())
        .filter(|id| crate::art::is_store_id(id))
        .map(str::to_string)
        .collect())
}

/// Product IDs of the games the signed in account owns (Store collections service).
pub async fn owned_ids(
    client: &reqwest::Client,
    tokens: &TokenManager,
    market: &str,
) -> Result<Vec<String>> {
    let store = crate::license::get_store_tokens(client, tokens).await?;
    // The user token travels in the body: never follow a redirect with it.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()?;
    let mut request = serde_json::json!({
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
    let mut ids = BTreeSet::new();
    // A safety limit: 100 pages of 100 items.
    for _ in 0..100 {
        let answer: serde_json::Value = serde_json::from_slice(
            &body(
                client
                    .post("https://collections.mp.microsoft.com/v7.0/collections/query")
                    .header("Authorization", &store.device)
                    .json(&request)
                    .send()
                    .await?,
            )
            .await?,
        )
        .map_err(|e| Error::msg(format!("unexpected collections answer: {e}")))?;
        for item in answer["items"].as_array().into_iter().flatten() {
            let id = item["productId"].as_str().unwrap_or_default();
            if item["productKind"] == "Game" && crate::art::is_store_id(id) {
                ids.insert(id.to_string());
            }
        }
        match answer["continuationToken"].as_str() {
            Some(token) => request["continuationToken"] = token.into(),
            None => break,
        }
    }
    Ok(ids.into_iter().collect())
}

/// The catalog sends `null` for some texts; they count as empty.
fn text<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Products {
    #[serde(default)]
    products: Vec<Product>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Product {
    #[serde(default, deserialize_with = "text")]
    product_id: String,
    #[serde(default)]
    localized_properties: Vec<Localized>,
    #[serde(default)]
    properties: Option<ProductProperties>,
    #[serde(default)]
    display_sku_availabilities: Vec<SkuAvailability>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Localized {
    #[serde(default, deserialize_with = "text")]
    product_title: String,
    #[serde(default, deserialize_with = "text")]
    publisher_name: String,
    #[serde(default, deserialize_with = "text")]
    short_description: String,
    #[serde(default, deserialize_with = "text")]
    product_description: String,
    #[serde(default)]
    images: Vec<Image>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Image {
    #[serde(default, deserialize_with = "text")]
    image_purpose: String,
    #[serde(default, deserialize_with = "text")]
    uri: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ProductProperties {
    #[serde(default)]
    category: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SkuAvailability {
    sku: Sku,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Sku {
    properties: SkuProperties,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SkuProperties {
    #[serde(default)]
    packages: Vec<Package>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Package {
    #[serde(default)]
    content_id: Option<String>,
    #[serde(default)]
    package_format: Option<String>,
    #[serde(default)]
    max_download_size_in_bytes: Option<u64>,
    #[serde(default)]
    platform_dependencies: Vec<Platform>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Platform {
    #[serde(default, deserialize_with = "text")]
    platform_name: String,
}

fn image_url(images: &[Image], purposes: &[&str]) -> Option<String> {
    let uri = purposes
        .iter()
        .find_map(|p| images.iter().find(|i| i.image_purpose == *p))?
        .uri
        .clone();
    Some(match uri.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => uri,
    })
}

fn windows_only(name: &str) -> Option<String> {
    KNOWN_WINDOWS_ONLY
        .iter()
        .find(|(prefix, _)| name.starts_with(prefix))
        .map(|(_, anticheat)| anticheat.to_string())
}

fn to_title(product: Product) -> Option<Title> {
    let localized = product.localized_properties.into_iter().next()?;
    // The package for Windows PCs; consoles have their own.
    let pc = product
        .display_sku_availabilities
        .iter()
        .flat_map(|a| a.sku.properties.packages.iter())
        .find(|p| {
            p.platform_dependencies
                .iter()
                .any(|d| d.platform_name == "Windows.Desktop")
        });
    let package_format = pc.and_then(|p| p.package_format.clone());
    let description: String = if localized.short_description.is_empty() {
        localized.product_description
    } else {
        localized.short_description
    }
    .chars()
    .take(1200)
    .collect();
    Some(Title {
        windows_only: windows_only(&localized.product_title),
        installable: package_format.as_deref() == Some("MSIXVC"),
        product_id: product.product_id,
        name: localized.product_title,
        publisher: localized.publisher_name,
        description,
        category: product
            .properties
            .and_then(|p| p.category)
            .unwrap_or_default(),
        content_id: pc
            .and_then(|p| p.content_id.clone())
            .map(|c| c.to_uppercase()),
        package_format,
        download_size: pc.and_then(|p| p.max_download_size_in_bytes),
        poster_url: image_url(&localized.images, &["Poster", "BoxArt"]),
        hero_url: image_url(&localized.images, &["SuperHeroArt", "TitledHeroArt"]),
        in_game_pass: false,
        owned: false,
    })
}

/// Details of products from the Store display catalog.
pub async fn details(client: &reqwest::Client, ids: &[String], market: &str) -> Result<Vec<Title>> {
    let mut titles = Vec::new();
    for batch in ids.chunks(BATCH) {
        // Store IDs and the market are checked letters and digits.
        let url = format!(
            "https://displaycatalog.mp.microsoft.com/v7.0/products?bigIds={}&market={market}&languages=en-US,neutral",
            batch.join(",")
        );
        let products: Products =
            serde_json::from_slice(&body(client.get(url).send().await?).await?)
                .map_err(|e| Error::msg(format!("unexpected catalog answer: {e}")))?;
        titles.extend(products.products.into_iter().filter_map(to_title));
    }
    Ok(titles)
}

/// The catalog for `market`: from the cache when it is younger than a day and `refresh` is false. Owned games are
/// left out when nobody is signed in.
pub async fn load(
    client: &reqwest::Client,
    tokens: &TokenManager,
    market: Option<&str>,
    refresh: bool,
) -> Result<Catalog> {
    let market = valid_market(market);
    let account = account(tokens);
    if !refresh {
        if let Some(cached) = read_cache(&market, &account) {
            if now().saturating_sub(cached.fetched_at) < MAX_AGE.as_secs() {
                return Ok(cached);
            }
        }
    }
    let game_pass: BTreeSet<String> = game_pass_ids(client, &market).await?.into_iter().collect();
    let owned: BTreeSet<String> = match owned_ids(client, tokens, &market).await {
        Ok(ids) => ids.into_iter().collect(),
        Err(err) => {
            tracing::warn!("owned games not loaded: {err}");
            BTreeSet::new()
        }
    };
    let ids: Vec<String> = game_pass.union(&owned).cloned().collect();
    let mut by_id: BTreeMap<String, Title> = details(client, &ids, &market)
        .await?
        .into_iter()
        .map(|t| (t.product_id.clone(), t))
        .collect();
    for (id, title) in &mut by_id {
        title.in_game_pass = game_pass.contains(id);
        title.owned = owned.contains(id);
    }
    let mut titles: Vec<Title> = by_id.into_values().collect();
    titles.sort_by_key(|t| t.name.to_lowercase());
    let catalog = Catalog {
        titles,
        fetched_at: now(),
    };
    write_cache(&market, &account, &catalog);
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRODUCT: &str = r#"{"Products":[{"ProductId":"9PM709J61828",
        "LocalizedProperties":[{"ProductTitle":"Farming Simulator 25 (PC)","PublisherName":"GIANTS Software",
            "ShortDescription":"","ProductDescription":"Farm.",
            "Images":[{"ImagePurpose":"Poster","Uri":"//store-images.s-microsoft.com/image/a"}]}],
        "Properties":{"Category":"Simulation"},
        "DisplaySkuAvailabilities":[{"Sku":{"Properties":{"Packages":[
            {"ContentId":"aa","PackageFormat":"MSIXVC","MaxDownloadSizeInBytes":1,"PlatformDependencies":[{"PlatformName":"Windows.Xbox"}]},
            {"ContentId":"fd1a3647-8cbf-439e-b7f9-b67af942ba4a","PackageFormat":"MSIXVC","MaxDownloadSizeInBytes":42,"PlatformDependencies":[{"PlatformName":"Windows.Desktop"}]}
        ]}}}]}]}"#;

    #[test]
    fn reads_the_pc_package() {
        let products: Products = serde_json::from_str(PRODUCT).unwrap();
        let title = to_title(products.products.into_iter().next().unwrap()).unwrap();
        assert_eq!(title.name, "Farming Simulator 25 (PC)");
        assert_eq!(
            title.content_id.as_deref(),
            Some("FD1A3647-8CBF-439E-B7F9-B67AF942BA4A")
        );
        assert_eq!(title.download_size, Some(42));
        assert!(title.installable);
        assert_eq!(title.description, "Farm.");
        assert_eq!(
            title.poster_url.as_deref(),
            Some("https://store-images.s-microsoft.com/image/a")
        );
        assert_eq!(title.windows_only, None);
    }

    #[test]
    fn knows_kernel_anti_cheat_titles() {
        assert!(windows_only("Call of Duty®: Black Ops 6").is_some());
        assert!(windows_only("Farming Simulator 25 (PC)").is_none());
    }

    #[test]
    fn markets_are_checked() {
        assert_eq!(valid_market(Some("de")), "DE");
        assert_eq!(valid_market(Some("../x")), "US");
    }

    /// Loads the real catalog: `cargo test -p xtux-games loads_the_catalog -- --ignored --nocapture`.
    #[ignore]
    #[tokio::test]
    async fn loads_the_catalog() {
        xodus::secrets::init_secrets().unwrap();
        let tokens = TokenManager::with_keychain_and_memory();
        let catalog = load(&reqwest::Client::new(), &tokens, Some("DE"), true)
            .await
            .unwrap();
        let count = |f: &dyn Fn(&Title) -> bool| catalog.titles.iter().filter(|t| f(t)).count();
        println!(
            "{} titles: {} Game Pass, {} owned, {} installable, {} Windows only",
            catalog.titles.len(),
            count(&|t| t.in_game_pass),
            count(&|t| t.owned),
            count(&|t| t.installable),
            count(&|t| t.windows_only.is_some()),
        );
    }
}
