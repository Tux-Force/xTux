//! Xbox profiles: what a game's profile card shows, and the gamer picture.

use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;

const HOST: &str = "https://profile.xboxlive.com";
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PICTURE: usize = 5 * 1024 * 1024;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Profile {
    pub gamertag: String,
    pub gamerscore: Option<u32>,
    pub location: String,
    pub bio: String,
    picture: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Answer {
    #[serde(default)]
    profile_users: Vec<User>,
}

#[derive(Deserialize)]
struct User {
    #[serde(default)]
    settings: Vec<Setting>,
}

#[derive(Deserialize)]
struct Setting {
    id: String,
    #[serde(default)]
    value: Option<String>,
}

impl From<User> for Profile {
    fn from(user: User) -> Self {
        let mut profile = Profile::default();
        for Setting { id, value } in user.settings {
            let value = value.unwrap_or_default();
            match id.as_str() {
                "Gamertag" => profile.gamertag = value,
                "Gamerscore" => profile.gamerscore = value.parse().ok(),
                "Location" => profile.location = value,
                "Bio" => profile.bio = value,
                "GameDisplayPicRaw" => profile.picture = Some(value).filter(|v| !v.is_empty()),
                _ => {}
            }
        }
        profile
    }
}

impl Profile {
    /// Text for the profile card; the first line is its title.
    pub fn to_text(&self) -> String {
        let one_line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut lines = vec![one_line(&self.gamertag)];
        if let Some(score) = self.gamerscore {
            lines.push(format!("Gamerscore: {score}"));
        }
        if !self.location.trim().is_empty() {
            lines.push(one_line(&self.location));
        }
        if !self.bio.trim().is_empty() {
            lines.push(String::new());
            lines.push(self.bio.trim().to_string());
        }
        lines.join("\n")
    }
}

/// The profile of the player `xuid`.
pub async fn fetch(client: &Client, authorization: &str, xuid: &str) -> Result<Profile, String> {
    fetch_from(client, HOST, authorization, xuid).await
}

async fn fetch_from(
    client: &Client,
    base: &str,
    authorization: &str,
    xuid: &str,
) -> Result<Profile, String> {
    if xuid.is_empty() || xuid.len() > 20 || !xuid.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid XUID".to_string());
    }
    let response = client
        .get(format!(
            "{base}/users/xuid({xuid})/profile/settings?settings=Gamertag,Gamerscore,Location,Bio,GameDisplayPicRaw"
        ))
        .timeout(TIMEOUT)
        .header("x-xbl-contract-version", "2")
        .header("Authorization", authorization)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !response.status().is_success() {
        return Err(format!("reading the profile failed: {}", response.status()));
    }
    let answer: Answer = response
        .json()
        .await
        .map_err(|e| e.without_url().to_string())?;
    answer
        .profile_users
        .into_iter()
        .next()
        .map(Profile::from)
        .filter(|p| !p.gamertag.is_empty())
        .ok_or_else(|| "no such player".to_string())
}

/// Address of the picture in a size; only Microsoft's own hosts are asked.
fn picture_url(raw: &str, pixels: u32) -> Option<reqwest::Url> {
    let mut url = reqwest::Url::parse(raw).ok()?;
    let host = url.host_str()?;
    let known = ["xboxlive.com", "xbox.com", "microsoft.com"]
        .iter()
        .any(|d| host.strip_suffix(d).is_some_and(|h| h.ends_with('.')));
    if !known {
        return None;
    }
    if url.scheme() != "https" {
        url.set_scheme("https").ok()?;
    }
    let pixels = pixels.to_string();
    url.query_pairs_mut()
        .append_pair("w", &pixels)
        .append_pair("h", &pixels)
        .append_pair("format", "png");
    Some(url)
}

/// The gamer picture as PNG.
pub async fn picture(client: &Client, profile: &Profile, pixels: u32) -> Result<Vec<u8>, String> {
    let url = profile
        .picture
        .as_deref()
        .and_then(|raw| picture_url(raw, pixels))
        .ok_or("the profile has no picture")?;
    let mut response = client
        .get(url)
        .timeout(TIMEOUT)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.without_url().to_string())?;
    let mut data = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| e.without_url().to_string())?
    {
        data.extend_from_slice(&chunk);
        if data.len() > MAX_PICTURE {
            return Err("the picture is too large".to_string());
        }
    }
    if !data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("the picture is not a PNG".to_string());
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_text() {
        let user: User = serde_json::from_value(serde_json::json!({"settings": [
            {"id": "Gamertag", "value": "Some\nOne"},
            {"id": "Gamerscore", "value": "1234"},
            {"id": "Location", "value": ""},
            {"id": "Bio", "value": " hi\nthere "},
            {"id": "GameDisplayPicRaw", "value": "https://images-eds-ssl.xboxlive.com/image?url=x"},
            {"id": "Other"}
        ]}))
        .unwrap();
        let profile = Profile::from(user);
        assert_eq!(profile.to_text(), "Some One\nGamerscore: 1234\n\nhi\nthere");
        assert!(profile.picture.is_some());
    }

    #[test]
    fn picture_hosts() {
        assert_eq!(
            picture_url("http://images-eds.xboxlive.com/image?url=a%2Fb", 208)
                .unwrap()
                .as_str(),
            "https://images-eds.xboxlive.com/image?url=a%2Fb&w=208&h=208&format=png"
        );
        assert!(picture_url("https://xboxlive.com.example.org/x", 64).is_none());
        assert!(picture_url("https://evilxboxlive.com/x", 64).is_none());
        assert!(picture_url("file:///etc/passwd", 64).is_none());
    }

    #[tokio::test]
    async fn refuses_other_xuids() {
        for xuid in ["", "1)/../x", "me", "123456789012345678901"] {
            assert!(
                fetch_from(&Client::new(), "http://127.0.0.1:1", "x", xuid)
                    .await
                    .is_err_and(|e| e == "invalid XUID")
            );
        }
    }
}
