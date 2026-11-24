//! Xbox achievements of a game: the list for the CLI and the launcher, and desktop notifications for achievements
//! unlocked while the game runs.
//!
//! Games unlock achievements themselves, with tokens from the game bridge; nothing here writes to the service. On
//! Windows the system shows the unlock. Here [`watch`] does: it is told about changes over Xbox Live's real time
//! activity service (RTA, a WebSocket), then reads the list again and reports what is newly unlocked, so it does
//! not depend on the contents of the notice. Without RTA it reads the list from time to time.

use std::collections::HashSet;
use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use xodus::tokens::TokenManager;

use crate::cloudsave::{Game, Result, xbox_authorization};

const HOST: &str = "https://achievements.xboxlive.com";
const RTA: &str = "wss://rta.xboxlive.com/connect";
const RTA_PROTOCOL: &str = "rta.xboxlive.com.V2";
const TIMEOUT: Duration = Duration::from_secs(30);
/// A list longer than this is cut; no game has that many.
const MAX_PAGES: usize = 40;
/// Reading the list while RTA is connected (a notice may get lost) and while it is not.
const POLL_WITH_RTA: Duration = Duration::from_secs(10 * 60);
const POLL_WITHOUT_RTA: Duration = Duration::from_secs(2 * 60);
/// The service needs a moment between its notice and the list showing the unlock.
const SETTLE: Duration = Duration::from_secs(2);
/// RTA server messages above this are not expected.
const MAX_RTA_MESSAGE: usize = 1 << 20;

/// One achievement, as shown to the user: name and description of a locked secret one are left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Achievement {
    pub id: String,
    pub name: String,
    pub description: String,
    pub unlocked: bool,
    /// When it was unlocked (RFC 3339); `None` while locked or when the service does not say.
    pub unlocked_at: Option<String>,
    pub gamerscore: u32,
    /// Locked with its details kept back by the game.
    pub secret: bool,
    /// Progress of a locked achievement in percent, when it has started.
    pub percent: Option<u8>,
}

/// Achievements of a game, unlocked ones first (newest first).
#[derive(Debug, Clone, Default)]
pub struct List {
    pub achievements: Vec<Achievement>,
    /// Service configuration the achievements belong to.
    scid: Option<String>,
}

impl List {
    pub fn unlocked(&self) -> usize {
        self.achievements.iter().filter(|a| a.unlocked).count()
    }

    pub fn gamerscore(&self) -> u32 {
        self.achievements
            .iter()
            .filter(|a| a.unlocked)
            .map(|a| a.gamerscore)
            .sum()
    }

    pub fn total_gamerscore(&self) -> u32 {
        self.achievements.iter().map(|a| a.gamerscore).sum()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    #[serde(default)]
    achievements: Vec<Raw>,
    #[serde(default)]
    paging_info: Option<Paging>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Paging {
    #[serde(default)]
    continuation_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    id: String,
    #[serde(default)]
    service_config_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    progress_state: String,
    #[serde(default)]
    progression: Option<Progression>,
    #[serde(default)]
    is_secret: bool,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    locked_description: Option<String>,
    #[serde(default)]
    rewards: Option<Vec<Reward>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Progression {
    #[serde(default)]
    requirements: Option<Vec<Requirement>>,
    #[serde(default)]
    time_unlocked: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Requirement {
    #[serde(default)]
    current: Option<Value>,
    #[serde(default)]
    target: Option<Value>,
}

#[derive(Deserialize)]
struct Reward {
    #[serde(default)]
    value: Option<Value>,
    #[serde(rename = "type", default)]
    kind: String,
}

/// Numbers come as JSON numbers or as strings.
fn number(value: &Option<Value>) -> Option<f64> {
    match value.as_ref()? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

impl From<Raw> for Achievement {
    fn from(raw: Raw) -> Self {
        let unlocked = raw.progress_state == "Achieved";
        let secret = raw.is_secret && !unlocked;
        let requirements = raw
            .progression
            .as_ref()
            .and_then(|p| p.requirements.as_deref())
            .unwrap_or_default();
        // Only a single numeric requirement says how far the whole achievement is.
        let percent = match requirements {
            [one] if !unlocked && !secret => number(&one.current)
                .zip(number(&one.target))
                .filter(|(current, target)| *target > 0.0 && *current > 0.0)
                .map(|(current, target)| (current / target * 100.0).clamp(0.0, 99.0) as u8),
            _ => None,
        };
        Achievement {
            unlocked,
            unlocked_at: raw
                .progression
                .and_then(|p| p.time_unlocked)
                // The service sends year 1 for locked ones.
                .filter(|t| unlocked && !t.starts_with("0001-")),
            gamerscore: raw
                .rewards
                .unwrap_or_default()
                .iter()
                .filter(|r| r.kind == "Gamerscore")
                .filter_map(|r| number(&r.value))
                .sum::<f64>()
                .max(0.0) as u32,
            name: if secret {
                "Secret achievement".to_string()
            } else {
                raw.name.unwrap_or_else(|| raw.id.clone())
            },
            description: if secret {
                String::new()
            } else if unlocked {
                raw.description.unwrap_or_default()
            } else {
                raw.locked_description
                    .filter(|d| !d.is_empty())
                    .or(raw.description)
                    .unwrap_or_default()
            },
            secret,
            percent,
            id: raw.id,
        }
    }
}

/// A signed in connection to the achievements of one game.
pub struct Achievements {
    client: Client,
    authorization: String,
    xuid: String,
    title_id: u32,
    /// The service, [`HOST`] except in tests.
    base: String,
}

impl Achievements {
    pub async fn connect(
        client: &Client,
        tokens: &TokenManager,
        game: &Game,
    ) -> Result<Achievements> {
        let (authorization, xuid) = xbox_authorization(client, tokens, game).await?;
        Ok(Achievements::new(
            client,
            authorization,
            xuid,
            game.title_id,
        ))
    }

    pub(crate) fn new(
        client: &Client,
        authorization: String,
        xuid: String,
        title_id: u32,
    ) -> Achievements {
        Achievements {
            client: client.clone(),
            authorization,
            xuid,
            title_id,
            base: HOST.to_string(),
        }
    }

    /// All achievements of the game with the user's progress.
    pub async fn list(&self) -> Result<List> {
        let mut achievements: Vec<Achievement> = Vec::new();
        let mut scid = None;
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut url = reqwest::Url::parse(&format!(
                "{}/users/xuid({})/achievements",
                self.base, self.xuid
            ))
            .map_err(|e| e.to_string())?;
            url.query_pairs_mut()
                .append_pair("titleId", &self.title_id.to_string())
                .append_pair("maxItems", "100");
            if let Some(token) = &token {
                url.query_pairs_mut()
                    .append_pair("continuationToken", token);
            }
            let response = self
                .client
                .get(url)
                .timeout(TIMEOUT)
                .header("x-xbl-contract-version", "2")
                .header("Accept-Language", language())
                .header("Authorization", &self.authorization)
                .send()
                .await
                .map_err(|e| e.without_url().to_string())?;
            let status = response.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                break;
            }
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(format!(
                    "reading the achievements failed: {status} {}",
                    body.chars().take(300).collect::<String>()
                ));
            }
            let page: Page = response
                .json()
                .await
                .map_err(|e| e.without_url().to_string())?;
            if scid.is_none() {
                scid = page
                    .achievements
                    .first()
                    .and_then(|a| a.service_config_id.clone())
                    .filter(|s| crate::cloudsave::is_scid(s));
            }
            achievements.extend(page.achievements.into_iter().map(Achievement::from));
            token = page
                .paging_info
                .and_then(|p| p.continuation_token)
                .filter(|t| !t.is_empty());
            if token.is_none() {
                break;
            }
        }
        achievements.sort_by(|a, b| {
            (b.unlocked, &b.unlocked_at, b.percent).cmp(&(a.unlocked, &a.unlocked_at, a.percent))
        });
        Ok(List { achievements, scid })
    }
}

/// The list as text for a window of the game (`XGameUiShowAchievementsAsync`); the first line is its title.
pub(crate) fn to_text(game: &str, list: &List) -> String {
    let one_line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let game = one_line(game);
    let mut text = if game.is_empty() {
        "Achievements\n".to_string()
    } else {
        format!("{game} - Achievements\n")
    };
    if list.achievements.is_empty() {
        text.push_str("There are no achievements.");
        return text;
    }
    text.push_str(&format!(
        "{} of {} unlocked, {} of {} Gamerscore\n",
        list.unlocked(),
        list.achievements.len(),
        list.gamerscore(),
        list.total_gamerscore()
    ));
    for a in &list.achievements {
        let state = match (a.unlocked, a.percent) {
            (true, _) => "[x]".to_string(),
            (false, Some(percent)) => format!("[{percent} %]"),
            (false, None) => "[ ]".to_string(),
        };
        text.push_str(&format!(
            "\n{state} {} ({} G)\n",
            one_line(&a.name),
            a.gamerscore
        ));
        if !a.description.trim().is_empty() {
            text.push_str(&format!("     {}\n", one_line(&a.description)));
        }
    }
    text
}

/// Language of the names: the one of the desktop, then English.
fn language() -> String {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|l| !l.is_empty()))
        .unwrap_or_default();
    accept_language(&locale)
}

fn accept_language(locale: &str) -> String {
    let tag = locale
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .replace('_', "-");
    if tag.len() < 2
        || tag == "POSIX"
        || !tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return "en-US".to_string();
    }
    format!("{tag},en-US;q=0.5")
}

// ---------------------------------------------------------------------------------------------------------------
// Unlocks while a game runs
// ---------------------------------------------------------------------------------------------------------------

/// Which unlocked achievements were not known yet.
#[derive(Default)]
struct Known(HashSet<String>);

impl Known {
    fn new(list: &List) -> Known {
        let mut known = Known::default();
        known.take_new(list);
        known
    }

    fn take_new(&mut self, list: &List) -> Vec<Achievement> {
        list.achievements
            .iter()
            .filter(|a| a.unlocked && self.0.insert(a.id.clone()))
            .cloned()
            .collect()
    }
}

/// Reports achievements unlocked from now on to `on_unlock`, until `stop`; then looks once more, for what was
/// unlocked right before the game closed. Problems are logged only, a game never depends on this.
pub async fn watch(
    client: Client,
    tokens: TokenManager,
    game: Game,
    stop: tokio_util::sync::CancellationToken,
    on_unlock: impl Fn(Achievement) + Send + Sync,
) {
    let mut reader = Reader {
        client: &client,
        tokens: &tokens,
        game: &game,
        session: None,
    };
    let first = tokio::select! {
        list = reader.list() => list,
        _ = stop.cancelled() => return,
    };
    let (mut known, scid) = match first {
        Ok(list) if list.achievements.is_empty() => {
            tracing::info!("{} has no achievements", game.name);
            return;
        }
        Ok(list) => (
            Known::new(&list),
            list.scid.unwrap_or_else(|| game.default_scid()),
        ),
        Err(err) => {
            tracing::warn!("achievements: {err}");
            return;
        }
    };
    tracing::info!(
        "watching the achievements of {}, {} unlocked",
        game.name,
        known.0.len()
    );

    while !stop.is_cancelled() {
        let rta = tokio::select! {
            rta = Rta::connect(&client, &tokens, &game, &scid) => rta,
            _ = stop.cancelled() => break,
        };
        match rta {
            Ok(mut rta) => {
                tracing::info!("told about unlocks by Xbox Live");
                loop {
                    tokio::select! {
                        notice = rta.next() => match notice {
                            Ok(()) => tokio::time::sleep(SETTLE).await,
                            Err(err) => {
                                tracing::info!("achievement notices ended: {err}");
                                break;
                            }
                        },
                        _ = tokio::time::sleep(POLL_WITH_RTA) => {}
                        _ = stop.cancelled() => break,
                    }
                    report(reader.list().await, &mut known, &on_unlock);
                }
            }
            Err(err) => tracing::info!("no achievement notices, asking now and then: {err}"),
        }
        tokio::select! {
            _ = tokio::time::sleep(POLL_WITHOUT_RTA) => {
                report(reader.list().await, &mut known, &on_unlock)
            }
            _ = stop.cancelled() => {}
        }
    }
    report(reader.list().await, &mut known, &on_unlock);
}

/// Reads the list with one sign-in for as long as it is accepted.
struct Reader<'a> {
    client: &'a Client,
    tokens: &'a TokenManager,
    game: &'a Game,
    session: Option<Achievements>,
}

impl Reader<'_> {
    async fn list(&mut self) -> Result<List> {
        if let Some(session) = &self.session
            && let Ok(list) = session.list().await
        {
            return Ok(list);
        }
        // Expired or failed: once more with a new sign-in.
        self.session = None;
        let session = Achievements::connect(self.client, self.tokens, self.game).await?;
        let list = session.list().await?;
        self.session = Some(session);
        Ok(list)
    }
}

fn report(list: Result<List>, known: &mut Known, on_unlock: &impl Fn(Achievement)) {
    match list {
        Ok(list) => {
            for achievement in known.take_new(&list) {
                tracing::info!("achievement unlocked: {}", achievement.name);
                on_unlock(achievement);
            }
        }
        Err(err) => tracing::warn!("achievements: {err}"),
    }
}

/// Connection to the real time activity service, subscribed to the user's achievements in one game.
struct Rta {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    subscription: i64,
}

/// What a message of the service means: `[1, sequence, code, subscription, data]` answers a subscribe request,
/// `[3, subscription, data]` is a change, `[4]` asks to read everything again.
#[derive(Debug, PartialEq, Eq)]
enum RtaMessage {
    Subscribed(i64),
    Refused(i64),
    Changed(i64),
    /// A change that unlocks nothing (games report progress often).
    Progress,
    Resync,
    Other,
}

fn parse_rta(text: &str) -> RtaMessage {
    let Ok(Value::Array(parts)) = serde_json::from_str::<Value>(text) else {
        return RtaMessage::Other;
    };
    let int = |i: usize| parts.get(i).and_then(Value::as_i64);
    match int(0) {
        Some(1) => match (int(2), int(3)) {
            (Some(0), Some(id)) => RtaMessage::Subscribed(id),
            (code, _) => RtaMessage::Refused(code.unwrap_or(-1)),
        },
        Some(3) => {
            let changes = parts
                .get(2)
                .and_then(|d| d.get("progression"))
                .and_then(Value::as_array);
            let unlocks = |c: &Value| {
                c.get("progressState")
                    .and_then(Value::as_str)
                    .is_none_or(|s| s == "Achieved")
            };
            match changes {
                Some(changes) if !changes.iter().any(unlocks) => RtaMessage::Progress,
                _ => int(1).map_or(RtaMessage::Other, RtaMessage::Changed),
            }
        }
        Some(4) => RtaMessage::Resync,
        _ => RtaMessage::Other,
    }
}

impl Rta {
    async fn connect(
        client: &Client,
        tokens: &TokenManager,
        game: &Game,
        scid: &str,
    ) -> Result<Rta> {
        tokio::time::timeout(TIMEOUT, Self::subscribe(client, tokens, game, scid))
            .await
            .map_err(|_| "no answer in time".to_string())?
    }

    async fn subscribe(
        client: &Client,
        tokens: &TokenManager,
        game: &Game,
        scid: &str,
    ) -> Result<Rta> {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::http::HeaderValue;
        use tokio_tungstenite::tungstenite::{Message, protocol::WebSocketConfig};

        let (authorization, xuid) = xbox_authorization(client, tokens, game).await?;
        let mut request = RTA.into_client_request().map_err(|e| e.to_string())?;
        let mut header =
            HeaderValue::from_str(&authorization).map_err(|_| "unusable token".to_string())?;
        header.set_sensitive(true);
        request.headers_mut().insert("Authorization", header);
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static(RTA_PROTOCOL),
        );
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_RTA_MESSAGE))
            .max_frame_size(Some(MAX_RTA_MESSAGE));
        let (socket, _) =
            tokio_tungstenite::connect_async_with_config(request, Some(config), false)
                .await
                .map_err(|e| e.to_string())?;
        let mut rta = Rta {
            socket,
            subscription: 0,
        };
        let uri = format!("{HOST}/users/xuid({xuid})/achievements/{scid}");
        rta.socket
            .send(Message::text(json!([1, 1, uri]).to_string()))
            .await
            .map_err(|e| e.to_string())?;
        loop {
            match rta.message().await? {
                RtaMessage::Subscribed(id) => {
                    rta.subscription = id;
                    return Ok(rta);
                }
                RtaMessage::Refused(code) => return Err(format!("subscription refused ({code})")),
                _ => {}
            }
        }
    }

    async fn message(&mut self) -> Result<RtaMessage> {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;
        match self.socket.next().await {
            Some(Ok(Message::Text(text))) => Ok(parse_rta(text.as_str())),
            Some(Ok(Message::Close(_))) | None => Err("connection closed".to_string()),
            Some(Ok(_)) => Ok(RtaMessage::Other),
            Some(Err(err)) => Err(err.to_string()),
        }
    }

    /// Waits for a change of the achievements.
    async fn next(&mut self) -> Result<()> {
        loop {
            match self.message().await? {
                RtaMessage::Changed(id) if id == self.subscription => return Ok(()),
                RtaMessage::Resync => return Ok(()),
                _ => {}
            }
        }
    }
}

/// Shows an unlocked achievement as a desktop notification.
#[cfg(target_os = "linux")]
pub async fn notify(game: &str, achievement: &Achievement) {
    use std::collections::HashMap;
    let (summary, body) = notification_text(game, achievement);
    let hints: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::from([(
        "desktop-entry",
        zbus::zvariant::Value::from("io.github.tux_force.xTux"),
    )]);
    let result = async {
        zbus::Connection::session()
            .await?
            .call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &(
                    "xTux",
                    0u32,
                    "io.github.tux_force.xTux",
                    summary.as_str(),
                    body.as_str(),
                    Vec::<&str>::new(),
                    hints,
                    8000i32,
                ),
            )
            .await
    }
    .await;
    if let Err(err) = result {
        tracing::warn!("the achievement could not be shown on the desktop: {err}");
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn notify(_game: &str, _achievement: &Achievement) {}

/// Title and text; the text of a notification is markup, so names from the service are escaped there.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn notification_text(game: &str, achievement: &Achievement) -> (String, String) {
    let escape = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let score = match achievement.gamerscore {
        0 => String::new(),
        n => format!(" ({n} G)"),
    };
    let mut body = escape(&achievement.description);
    if !body.is_empty() {
        body.push('\n');
    }
    body.push_str(&escape(game));
    (
        format!("Achievement unlocked: {}{score}", achievement.name),
        body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: Value) -> Achievement {
        serde_json::from_value::<Raw>(value).unwrap().into()
    }

    #[test]
    fn unlocked_achievement() {
        let a = parse(json!({
            "id": "1", "name": "First", "progressState": "Achieved", "isSecret": true,
            "description": "done", "lockedDescription": "do it",
            "progression": {"requirements": [], "timeUnlocked": "2026-01-02T03:04:05.0000000Z"},
            "rewards": [{"value": "15", "type": "Gamerscore"}, {"value": "x", "type": "Art"}]
        }));
        assert_eq!((a.name.as_str(), a.description.as_str()), ("First", "done"));
        assert!(a.unlocked && !a.secret);
        assert_eq!(a.gamerscore, 15);
        assert!(a.unlocked_at.is_some());
    }

    #[test]
    fn locked_secret_is_not_spoiled() {
        let a = parse(json!({
            "id": "2", "name": "Spoiler", "progressState": "NotStarted", "isSecret": true,
            "description": "spoiler", "lockedDescription": "spoiler",
            "progression": {"requirements": [{"current": "1", "target": "2"}],
                            "timeUnlocked": "0001-01-01T00:00:00.0000000Z"},
            "rewards": null
        }));
        assert_eq!(a.name, "Secret achievement");
        assert!(a.description.is_empty() && a.secret && !a.unlocked);
        assert_eq!((a.percent, a.unlocked_at, a.gamerscore), (None, None, 0));
    }

    #[test]
    fn progress_of_a_single_requirement() {
        let a = parse(json!({
            "id": "3", "name": "N", "progressState": "InProgress",
            "progression": {"requirements": [{"current": 5, "target": "20"}]}
        }));
        assert_eq!(a.percent, Some(25));
    }

    #[test]
    fn text_for_the_game() {
        let list = List {
            achievements: vec![
                parse(
                    json!({"id": "1", "name": "A\nB", "description": "d", "progressState": "Achieved",
                             "rewards": [{"value": "5", "type": "Gamerscore"}]}),
                ),
                parse(
                    json!({"id": "2", "name": "Spoiler", "isSecret": true, "progressState": "NotStarted"}),
                ),
            ],
            scid: None,
        };
        assert_eq!(
            to_text("Game", &list),
            "Game - Achievements\n1 of 2 unlocked, 5 of 5 Gamerscore\n\n[x] A B (5 G)\n     d\n\n\
             [ ] Secret achievement (0 G)\n"
        );
        assert_eq!(
            to_text("", &List::default()),
            "Achievements\nThere are no achievements."
        );
    }

    #[test]
    fn only_new_unlocks_are_reported() {
        let achievement = |id: &str, state: &str| parse(json!({"id": id, "progressState": state}));
        let before = List {
            achievements: vec![achievement("1", "Achieved"), achievement("2", "NotStarted")],
            scid: None,
        };
        let after = List {
            achievements: vec![achievement("1", "Achieved"), achievement("2", "Achieved")],
            scid: None,
        };
        let mut known = Known::new(&before);
        let new = known.take_new(&after);
        assert_eq!(new.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["2"]);
        assert!(known.take_new(&after).is_empty());
    }

    #[test]
    fn rta_messages() {
        assert_eq!(parse_rta("[1,1,0,7,{}]"), RtaMessage::Subscribed(7));
        assert_eq!(parse_rta("[1,1,2,\"no\"]"), RtaMessage::Refused(2));
        assert_eq!(parse_rta("[3,7,{}]"), RtaMessage::Changed(7));
        assert_eq!(
            parse_rta(r#"[3,7,{"progression":[{"id":"1","progressState":"InProgress"}]}]"#),
            RtaMessage::Progress
        );
        assert_eq!(
            parse_rta(r#"[3,7,{"progression":[{"id":"1","progressState":"Achieved"}]}]"#),
            RtaMessage::Changed(7)
        );
        assert_eq!(parse_rta("[4]"), RtaMessage::Resync);
        assert_eq!(parse_rta("nonsense"), RtaMessage::Other);
    }

    #[test]
    fn languages() {
        assert_eq!(accept_language("de_DE.UTF-8"), "de-DE,en-US;q=0.5");
        assert_eq!(accept_language("C.UTF-8"), "en-US");
        assert_eq!(accept_language("POSIX"), "en-US");
        assert_eq!(accept_language(""), "en-US");
    }

    #[test]
    fn notification_is_escaped() {
        let mut a = parse(
            json!({"id": "1", "name": "A & B", "progressState": "Achieved",
            "description": "<b>x</b>", "rewards": [{"value": 10, "type": "Gamerscore"}]}),
        );
        let (summary, body) = notification_text("G<1>", &a);
        assert_eq!(summary, "Achievement unlocked: A & B (10 G)");
        assert_eq!(body, "&lt;b&gt;x&lt;/b&gt;\nG&lt;1&gt;");
        a.description.clear();
        assert_eq!(notification_text("G", &a).1, "G");
    }

    #[tokio::test]
    async fn reads_all_pages() {
        use axum::{Json, Router, extract::Query, routing::get};
        let app = Router::new().route(
            "/users/{xuid}/achievements",
            get(
                |Query(q): Query<std::collections::HashMap<String, String>>| async move {
                    assert_eq!(q["titleId"], "5");
                    Json(match q.get("continuationToken").map(String::as_str) {
                        None => json!({"achievements": [
                            {"id": "1", "progressState": "NotStarted"}],
                            "pagingInfo": {"continuationToken": "1", "totalRecords": 2}}),
                        Some(_) => json!({"achievements": [
                            {"id": "2", "progressState": "Achieved",
                             "rewards": [{"value": "20", "type": "Gamerscore"}]}],
                            "pagingInfo": {"continuationToken": null, "totalRecords": 2}}),
                    })
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let list = Achievements {
            client: Client::new(),
            authorization: "XBL3.0 x=1;t".into(),
            xuid: "1".into(),
            title_id: 5,
            base,
        }
        .list()
        .await
        .unwrap();
        // Unlocked first.
        assert_eq!(list.achievements[0].id, "2");
        assert_eq!(list.achievements.len(), 2);
        assert_eq!((list.unlocked(), list.gamerscore()), (1, 20));
    }
}
