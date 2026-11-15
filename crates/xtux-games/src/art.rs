//! Game artwork: cover, hero image, a blurred backdrop and a logo.
//!
//! Images come from the public Microsoft Store catalog by the game's Store ID (no sign-in, only the Store ID and
//! market are sent) and are cached in `$XDG_CACHE_HOME/xtux/art/<StoreID>/`. Without network the images inside the
//! game folder (`ShellVisuals` of `MicrosoftGame.config`) are used.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Error, Result};

/// Largest image file read or downloaded; catalog images at the requested sizes are far smaller.
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
/// Largest catalog answer.
const MAX_CATALOG_BYTES: usize = 4 * 1024 * 1024;
/// Hosts art requests may reach, also through redirects.
const HOSTS: [&str; 2] = [
    "displaycatalog.mp.microsoft.com",
    "store-images.s-microsoft.com",
];

/// HTTP client for art: https to the Store hosts only, also when redirected.
pub fn client(user_agent: &str) -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let url = attempt.url();
            if attempt.previous().len() < 5
                && url.scheme() == "https"
                && url.host_str().is_some_and(|h| HOSTS.contains(&h))
            {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .unwrap_or_default()
}

/// Decoding limits: no image of the Store or a game folder needs more.
fn limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(256 * 1024 * 1024);
    limits
}

fn decode_bytes(bytes: &[u8]) -> Result<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Error::msg(format!("not an image: {e}")))?;
    reader.limits(limits());
    reader
        .decode()
        .map_err(|e| Error::msg(format!("not an image: {e}")))
}

/// Opens an image file with size and decoding limits; regular files only.
pub fn open_image(path: &Path) -> Result<image::DynamicImage> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.len() as usize > MAX_IMAGE_BYTES {
        return Err(Error::msg(format!(
            "{} is not a usable image file",
            path.display()
        )));
    }
    decode_bytes(&std::fs::read(path)?)
}

/// Images of one game; `None` where there is none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Artwork {
    /// Portrait cover (2:3).
    pub cover: Option<PathBuf>,
    /// Wide hero image (16:9).
    pub hero: Option<PathBuf>,
    /// Small, blurred version of the hero image for glass backgrounds.
    pub backdrop: Option<PathBuf>,
    /// Square logo or icon.
    pub logo: Option<PathBuf>,
}

impl Artwork {
    fn or(self, other: Artwork) -> Artwork {
        Artwork {
            cover: self.cover.or(other.cover),
            hero: self.hero.or(other.hero),
            backdrop: self.backdrop.or(other.backdrop),
            logo: self.logo.or(other.logo),
        }
    }
}

/// Store IDs are 12 letters and digits; anything else never becomes a path or URL.
pub fn is_store_id(id: &str) -> bool {
    id.len() == 12 && id.chars().all(|c| c.is_ascii_alphanumeric())
}

/// `$XDG_CACHE_HOME/xtux/art`.
pub fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("xtux/art"))
}

fn existing(path: PathBuf) -> Option<PathBuf> {
    path.is_file().then_some(path)
}

/// Cached catalog images of a Store ID.
pub fn cached(store_id: &str) -> Artwork {
    let Some(dir) = cache_dir()
        .filter(|_| is_store_id(store_id))
        .map(|d| d.join(store_id))
    else {
        return Artwork::default();
    };
    Artwork {
        cover: existing(dir.join("cover.jpg")),
        hero: existing(dir.join("hero.jpg")),
        backdrop: existing(dir.join("backdrop.jpg")),
        logo: existing(dir.join("logo.png")),
    }
}

/// Images inside the game folder: the splash screen as hero, the largest logo as logo and cover.
pub fn local(game_dir: &Path) -> Artwork {
    let Some(layout) = crate::layout::detect(game_dir) else {
        return Artwork::default();
    };
    let Some(config) = crate::library::read_config(&layout.root) else {
        return Artwork::default();
    };
    let image = |attr: &str| {
        let relative = crate::library::attribute(&config, "ShellVisuals", attr)?;
        // Package relative paths only.
        let relative = Path::new(&relative);
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return None;
        }
        // Stays inside the game folder also through links.
        let root = std::fs::canonicalize(&layout.root).ok()?;
        let path = std::fs::canonicalize(layout.root.join(relative)).ok()?;
        path.starts_with(&root).then_some(path).and_then(existing)
    };
    let logo = image("Square480x480Logo")
        .or_else(|| image("Square150x150Logo"))
        .or_else(|| image("StoreLogo"));
    Artwork {
        cover: None,
        hero: image("SplashScreenImage"),
        backdrop: None,
        logo,
    }
}

/// Everything available without network: cached catalog images, then the game folder's own.
pub fn available(game_dir: &Path, store_id: Option<&str>) -> Artwork {
    let cached = store_id.map(cached).unwrap_or_default();
    cached.or(local(game_dir))
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Catalog {
    #[serde(default)]
    products: Vec<Product>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Product {
    #[serde(default)]
    localized_properties: Vec<Localized>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Localized {
    #[serde(default)]
    images: Vec<CatalogImage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CatalogImage {
    image_purpose: String,
    uri: String,
}

/// Catalog image URLs only point to the Store's image host.
fn image_url(uri: &str, query: &str) -> Option<String> {
    let url = if let Some(rest) = uri.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        uri.to_string()
    };
    let parsed = reqwest::Url::parse(&url).ok()?;
    (parsed.scheme() == "https" && parsed.host_str() == Some("store-images.s-microsoft.com"))
        .then(|| format!("{}?{query}", url.split('?').next().unwrap_or(&url)))
}

/// Downloads at most `max` bytes, also when the server sends no length.
async fn download(client: &reqwest::Client, url: &str, max: usize) -> Result<Vec<u8>> {
    let mut response = client.get(url).send().await?.error_for_status()?;
    if response.content_length().is_some_and(|l| l as usize > max) {
        return Err(Error::msg("answer too large"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > max {
            return Err(Error::msg("answer too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Writes a temporary file next to the target and renames it, so readers never see half an image and two writers
/// never mix their files.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().ok_or("no folder for the image")?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(bytes)?;
    file.persist(path).map_err(|e| Error::Io(e.error))?;
    Ok(())
}

/// Decodes `bytes` and stores them as `path` (JPEG or PNG by extension), so only valid images reach the cache.
fn store_image(bytes: &[u8], path: &Path) -> Result<image::DynamicImage> {
    let image = decode_bytes(bytes)?;
    let format = if path.extension().is_some_and(|e| e == "png") {
        image::ImageFormat::Png
    } else {
        image::ImageFormat::Jpeg
    };
    let mut encoded = std::io::Cursor::new(Vec::new());
    let rgb: image::DynamicImage = if format == image::ImageFormat::Jpeg {
        image.to_rgb8().into()
    } else {
        image.clone()
    };
    rgb.write_to(&mut encoded, format)
        .map_err(|e| Error::msg(format!("could not encode {}: {e}", path.display())))?;
    write_atomic(path, encoded.get_ref())?;
    Ok(image)
}

/// A small, strongly blurred copy of the hero image for glass backgrounds.
fn store_backdrop(hero: &image::DynamicImage, path: &Path) -> Result<()> {
    let small = hero.resize_to_fill(640, 360, image::imageops::FilterType::Triangle);
    let blurred = image::imageops::fast_blur(&small.to_rgb8(), 18.0);
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(blurred)
        .write_to(&mut encoded, image::ImageFormat::Jpeg)
        .map_err(|e| Error::msg(format!("could not encode the backdrop: {e}")))?;
    write_atomic(path, encoded.get_ref())
}

/// Loads the catalog images of `store_id` into the cache (only missing ones) and returns what is cached then.
pub async fn fetch(
    client: &reqwest::Client,
    store_id: &str,
    market: Option<&str>,
) -> Result<Artwork> {
    if !is_store_id(store_id) {
        return Err(Error::msg(format!("{store_id:?} is not a Store ID")));
    }
    let have = cached(store_id);
    if have.cover.is_some() && have.hero.is_some() && have.backdrop.is_some() && have.logo.is_some()
    {
        return Ok(have);
    }
    let dir = cache_dir()
        .ok_or("cannot determine the cache folder")?
        .join(store_id);
    std::fs::create_dir_all(&dir)?;

    let market = market
        .filter(|m| m.len() == 2 && m.chars().all(|c| c.is_ascii_alphabetic()))
        .unwrap_or("US");
    // Both values are checked letters and digits, so they need no escaping.
    let url = format!(
        "https://displaycatalog.mp.microsoft.com/v7.0/products?bigIds={store_id}&market={market}&languages=en-US,neutral"
    );
    let catalog: Catalog =
        serde_json::from_slice(&download(client, &url, MAX_CATALOG_BYTES).await?)
            .map_err(|e| Error::msg(format!("unexpected catalog answer: {e}")))?;
    let images: Vec<CatalogImage> = catalog
        .products
        .into_iter()
        .flat_map(|p| p.localized_properties)
        .flat_map(|l| l.images)
        .collect();
    let find = |purposes: &[&str]| {
        purposes
            .iter()
            .find_map(|purpose| images.iter().find(|i| i.image_purpose == *purpose))
            .map(|i| i.uri.clone())
    };

    // Each image on its own: one failing does not stop the others.
    let mut failed = None;
    if have.cover.is_none() {
        if let Some(url) =
            find(&["Poster", "BoxArt"]).and_then(|u| image_url(&u, "w=480&h=720&q=88&mode=scale"))
        {
            if let Err(err) = save_download(client, &url, &dir.join("cover.jpg"), None).await {
                failed = Some(err);
            }
        }
    }
    if have.hero.is_none() || have.backdrop.is_none() {
        let hero_uri = find(&["SuperHeroArt", "TitledHeroArt", "Screenshot"]);
        if let Some(url) = hero_uri.and_then(|u| image_url(&u, "w=1920&h=1080&q=85&mode=scale")) {
            let backdrop = dir.join("backdrop.jpg");
            if let Err(err) =
                save_download(client, &url, &dir.join("hero.jpg"), Some(&backdrop)).await
            {
                failed = Some(err);
            }
        }
    }
    if have.logo.is_none() {
        if let Some(url) =
            find(&["Logo", "BoxArt"]).and_then(|u| image_url(&u, "w=300&h=300&q=90&mode=scale"))
        {
            if let Err(err) = save_download(client, &url, &dir.join("logo.png"), None).await {
                failed = Some(err);
            }
        }
    }
    let art = cached(store_id);
    match failed {
        Some(err) if art == Artwork::default() => Err(err),
        Some(err) => {
            tracing::warn!("some art of {store_id} is missing: {err}");
            Ok(art)
        }
        None => Ok(art),
    }
}

/// Cover and hero of a catalog title from URLs the catalog gave, into the cache (only missing ones); for titles
/// that are not installed yet. Returns what is cached then.
pub async fn fetch_from(
    client: &reqwest::Client,
    store_id: &str,
    cover_url: Option<&str>,
    hero_url: Option<&str>,
) -> Result<Artwork> {
    if !is_store_id(store_id) {
        return Err(Error::msg(format!("{store_id:?} is not a Store ID")));
    }
    let have = cached(store_id);
    let dir = cache_dir()
        .ok_or("cannot determine the cache folder")?
        .join(store_id);
    std::fs::create_dir_all(&dir)?;
    if have.cover.is_none() {
        if let Some(url) = cover_url.and_then(|u| image_url(u, "w=480&h=720&q=88&mode=scale")) {
            save_download(client, &url, &dir.join("cover.jpg"), None).await?;
        }
    }
    if have.hero.is_none() || have.backdrop.is_none() {
        if let Some(url) = hero_url.and_then(|u| image_url(u, "w=1920&h=1080&q=85&mode=scale")) {
            let backdrop = dir.join("backdrop.jpg");
            save_download(client, &url, &dir.join("hero.jpg"), Some(&backdrop)).await?;
        }
    }
    Ok(cached(store_id))
}

/// Downloads an image into the cache, and its blurred backdrop when asked for.
async fn save_download(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    backdrop: Option<&Path>,
) -> Result<()> {
    let image = store_image(&download(client, url, MAX_IMAGE_BYTES).await?, path)?;
    if let Some(backdrop) = backdrop {
        store_backdrop(&image, backdrop)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_ids() {
        assert!(is_store_id("9PM709J61828"));
        assert!(!is_store_id("../../etc/xx"));
        assert!(!is_store_id("9PM709J6182"));
    }

    #[test]
    fn image_urls_stay_on_the_store_host() {
        assert_eq!(
            image_url("//store-images.s-microsoft.com/image/apps.1.2", "w=1").as_deref(),
            Some("https://store-images.s-microsoft.com/image/apps.1.2?w=1")
        );
        assert_eq!(image_url("//evil.example/x", "w=1"), None);
        assert_eq!(
            image_url("http://store-images.s-microsoft.com/x", "w=1"),
            None
        );
    }

    #[test]
    fn blurred_backdrop_is_small() {
        let dir = tempfile::tempdir().unwrap();
        let hero = image::DynamicImage::new_rgb8(1920, 1080);
        let path = dir.path().join("backdrop.jpg");
        store_backdrop(&hero, &path).unwrap();
        let back = image::open(&path).unwrap();
        assert_eq!((back.width(), back.height()), (640, 360));
    }

    /// Talks to the Store catalog: `cargo test -p xtux-games fetches_catalog_art -- --ignored`.
    #[ignore]
    #[tokio::test]
    async fn fetches_catalog_art() {
        let art = fetch(&client("xtux-test"), "9PM709J61828", Some("DE"))
            .await
            .unwrap();
        println!("{art:?}");
        assert!(
            art.cover.is_some()
                && art.hero.is_some()
                && art.backdrop.is_some()
                && art.logo.is_some()
        );
    }
}
