//! Terminal front end for the package lookup: lets the user pick an edition when a product is a bundle.

use inquire::Select;
use xodus::models::packagespc::PackageDetails;
use xodus::tokens::TokenManager;
use xtux_games::package::{self, ContentLookup};

pub async fn get_content_id(
    client: &reqwest::Client,
    product: String,
    market: Option<String>,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut product = product;
    loop {
        match package::find_content_id(client, product, market.clone()).await? {
            ContentLookup::Found(content_id) => return Ok(content_id),
            ContentLookup::Choose(products) => {
                product = Select::new("Select files to download", products)
                    .with_page_size(30)
                    .prompt()
                    .map_err(|_| std::io::Error::other("Selection failed"))?;
            }
        }
    }
}

pub async fn get_packages(
    client: &reqwest::Client,
    tokens: &TokenManager,
    content_id: String,
) -> Result<PackageDetails, Box<dyn std::error::Error>> {
    Ok(package::get_packages(client, tokens, content_id).await?)
}
