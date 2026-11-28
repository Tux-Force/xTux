//! In-game events (`XGameEventWrite`): statistics and achievements some games unlock by them.
//!
//! They go to Microsoft's event collector the way the Xbox Services API (MIT licensed) uploads them: one JSON object
//! per line, with the device's and the user's Xbox tokens as tickets. A failed upload is tried again; events the
//! collector refuses are dropped.

use std::collections::VecDeque;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, Notify};

pub const DEVICE_RELYING_PARTY: &str = "http://vortex.microsoft.com";
pub const USER_RELYING_PARTY: &str = "http://events.xboxlive.com";
const COLLECTOR: &str = "https://vortex.data.microsoft.com/collect/v1";
/// Events kept while the collector cannot be reached; the oldest go first.
const MAX_QUEUED: usize = 1000;
const MAX_BATCH: usize = 50;
/// A JSON text of one event.
const MAX_JSON: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);

/// An event as the game wrote it.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    scid: String,
    session: String,
    name: String,
    dimensions: Option<Map<String, Value>>,
    measurements: Option<Map<String, Value>>,
    time: String,
}

/// `[A-Za-z]+[A-Za-z0-9_]*`
fn is_event_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.len() <= 128
}

fn object(text: &str, what: &str) -> Result<Option<Map<String, Value>>, String> {
    if text.len() > MAX_JSON {
        return Err(format!("the {what} are too long"));
    }
    match text {
        "" => Ok(None),
        text => serde_json::from_str(text)
            .map(Some)
            .map_err(|_| format!("the {what} are not a JSON object")),
    }
}

impl Event {
    /// From the bridge: service config id, play session, name, dimensions and measurements (JSON), a line each.
    pub fn parse(body: &str) -> Result<Event, String> {
        let mut lines = body.split('\n').map(str::trim);
        let mut next = || lines.next().unwrap_or_default();
        let (scid, session, name) = (next(), next(), next());
        if uuid::Uuid::parse_str(scid).is_err() {
            return Err("invalid service config id".into());
        }
        if !is_event_name(name) {
            return Err("invalid event name".into());
        }
        Ok(Event {
            scid: scid.to_string(),
            session: session.chars().take(64).collect(),
            name: name.to_string(),
            dimensions: object(next(), "dimensions")?,
            measurements: object(next(), "measurements")?,
            time: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        })
    }

    fn to_json(&self, title_id: u32, xuid: &str) -> Value {
        let mut data = json!({
            "name": self.name,
            "serviceConfigId": self.scid,
            "playerSessionId": self.session,
            "titleId": title_id.to_string(),
            "userId": xuid,
            "ver": 1,
        });
        if let Some(dimensions) = &self.dimensions {
            data["properties"] = dimensions.clone().into();
        }
        if let Some(measurements) = &self.measurements {
            data["measurements"] = measurements.clone().into();
        }
        json!({
            "ver": "2.1",
            "name": format!("Microsoft.XboxLive.T{title_id}.{}", self.name),
            "time": self.time,
            "iKey": format!("P-XBL-T{title_id}"),
            "ext": {
                "user": { "localId": format!("x:{xuid}") },
                "protocol": { "ticketKeys": [["2"]] },
            },
            "data": {
                "baseType": "Microsoft.XboxLive.InGame",
                "baseData": data,
            },
        })
    }
}

/// What an upload needs besides the events.
#[derive(Clone)]
pub struct Tickets {
    pub title_id: u32,
    pub xuid: String,
    /// `XBL3.0 x=…` for [`DEVICE_RELYING_PARTY`] and [`USER_RELYING_PARTY`].
    pub device: String,
    pub user: String,
}

#[derive(Debug, PartialEq)]
pub enum UploadError {
    /// Worth another try.
    Retry(String),
    /// The tickets are no longer accepted.
    SignIn(String),
    /// The collector does not take these events.
    Refused(String),
}

pub async fn upload(
    client: &reqwest::Client,
    url: &str,
    tickets: &Tickets,
    events: &[Event],
) -> Result<(), UploadError> {
    let mut body = String::new();
    for event in events {
        body += &event.to_json(tickets.title_id, &tickets.xuid).to_string();
        body += "\n";
    }
    let mut response = client
        .post(url)
        .timeout(TIMEOUT)
        .header("Content-Type", "application/x-json-stream; charset=utf-8")
        .header(
            "X-UploadTime",
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        )
        .header("X-AuthXToken", &tickets.device)
        .header("X-Tickets", format!("\"2\"=\"x:{}\"", tickets.user))
        .body(body)
        .send()
        .await
        .map_err(|e| UploadError::Retry(e.without_url().to_string()))?;
    let status = response.status();
    let reason = response
        .headers()
        .get("collector-error")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let mut answer = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        answer.extend_from_slice(&chunk);
        if answer.len() > 64 * 1024 {
            break;
        }
    }
    let counts: Value = serde_json::from_slice(&answer).unwrap_or_default();
    let detail = format!("{status} {reason} {counts}");
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(UploadError::SignIn(detail));
    }
    if status.is_server_error() || matches!(status.as_u16(), 408 | 429) {
        return Err(UploadError::Retry(detail));
    }
    if !status.is_success() {
        return Err(UploadError::Refused(detail));
    }
    if counts["rej"].as_u64().unwrap_or(0) > 0 {
        tracing::warn!("the event collector refused events of the game: {detail}");
    } else {
        tracing::info!("{} events of the game uploaded: {detail}", events.len());
    }
    Ok(())
}

pub fn collector() -> &'static str {
    COLLECTOR
}

/// Events waiting for their upload, in the order the game wrote them.
#[derive(Default)]
pub struct Queue {
    events: Mutex<VecDeque<Event>>,
    /// Held during an upload, so nothing overtakes it.
    uploading: Mutex<()>,
    added: Notify,
}

impl Queue {
    pub async fn push(&self, event: Event) {
        let mut events = self.events.lock().await;
        if events.len() >= MAX_QUEUED {
            tracing::warn!("too many events of the game wait for upload, the oldest is dropped");
            events.pop_front();
        }
        events.push_back(event);
        self.added.notify_one();
    }

    pub async fn wait(&self) {
        self.added.notified().await;
    }

    /// Uploads everything with `send`. Events of a failed try stay in the queue.
    pub async fn flush<F>(&self, send: impl Fn(Vec<Event>) -> F) -> Result<(), UploadError>
    where
        F: Future<Output = Result<(), UploadError>>,
    {
        let _uploading = self.uploading.lock().await;
        loop {
            let mut batch: Vec<Event> = {
                let mut events = self.events.lock().await;
                let count = events.len().min(MAX_BATCH);
                events.drain(..count).collect()
            };
            if batch.is_empty() {
                return Ok(());
            }
            match send(batch.clone()).await {
                Ok(()) => {}
                Err(UploadError::Refused(detail)) => {
                    tracing::warn!(
                        "the event collector refused {} events: {detail}",
                        batch.len()
                    );
                }
                Err(err) => {
                    let mut events = self.events.lock().await;
                    while let Some(event) = batch.pop() {
                        if events.len() >= MAX_QUEUED {
                            break;
                        }
                        events.push_front(event);
                    }
                    return Err(err);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const BODY: &str = "00000000-0000-0000-0000-00006D0FD16E\nplay-1\nEnemyDefeated\n{\"Map\":\"A\"}\n{\"Count\":2}";

    #[test]
    fn reads_an_event() {
        let event = Event::parse(BODY).unwrap();
        let json = event.to_json(42, "2533");
        assert_eq!(json["name"], "Microsoft.XboxLive.T42.EnemyDefeated");
        assert_eq!(json["iKey"], "P-XBL-T42");
        let data = &json["data"]["baseData"];
        assert_eq!(data["properties"]["Map"], "A");
        assert_eq!(data["measurements"]["Count"], 2);
        assert_eq!(data["userId"], "2533");
        assert_eq!(data["playerSessionId"], "play-1");
        // Without dimensions and measurements.
        let plain = Event::parse("00000000-0000-0000-0000-00006D0FD16E\n\nStarted\n\n").unwrap();
        assert!(plain.to_json(1, "1")["data"]["baseData"]["properties"].is_null());
    }

    #[test]
    fn refuses_bad_events() {
        for bad in [
            BODY.replace("EnemyDefeated", "1Enemy"),
            BODY.replace("EnemyDefeated", "Enemy Defeated"),
            BODY.replace("EnemyDefeated", ""),
            BODY.replace("{\"Map\":\"A\"}", "[1]"),
            BODY.replace("{\"Count\":2}", "{"),
            BODY.replace("00000000-0000", "x"),
        ] {
            assert!(Event::parse(&bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn keeps_events_in_order_until_they_are_uploaded() {
        let queue = Queue::default();
        for i in 0..60 {
            queue
                .push(Event::parse(&BODY.replace("EnemyDefeated", &format!("E{i}"))).unwrap())
                .await;
        }
        let failed = queue
            .flush(|_| async { Err(UploadError::Retry("offline".into())) })
            .await;
        assert_eq!(failed, Err(UploadError::Retry("offline".into())));
        let names = std::sync::Mutex::new(Vec::new());
        queue
            .flush(|events| {
                names
                    .lock()
                    .unwrap()
                    .extend(events.into_iter().map(|e| e.name));
                async { Ok(()) }
            })
            .await
            .unwrap();
        let expected: Vec<String> = (0..60).map(|i| format!("E{i}")).collect();
        assert_eq!(*names.lock().unwrap(), expected);
    }

    #[tokio::test]
    async fn drops_refused_events() {
        let queue = Queue::default();
        queue.push(Event::parse(BODY).unwrap()).await;
        let calls = AtomicUsize::new(0);
        for _ in 0..2 {
            queue
                .flush(|_| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    async { Err(UploadError::Refused("400".into())) }
                })
                .await
                .unwrap();
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn uploads_with_tickets() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, body: String| async move {
                    let refused = headers["x-authxtoken"] != "XBL3.0 x=1;device";
                    log.lock().unwrap().push((headers, body));
                    if refused {
                        (axum::http::StatusCode::UNAUTHORIZED, "{}")
                    } else {
                        (axum::http::StatusCode::OK, "{\"acc\":2,\"rej\":0}")
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let event = Event::parse(BODY).unwrap();
        let mut tickets = Tickets {
            title_id: 42,
            xuid: "2533".into(),
            device: "XBL3.0 x=1;device".into(),
            user: "XBL3.0 x=2;user".into(),
        };
        let client = reqwest::Client::new();
        upload(&client, &url, &tickets, &[event.clone(), event.clone()])
            .await
            .unwrap();
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen[0].0["x-tickets"], "\"2\"=\"x:XBL3.0 x=2;user\"");
            assert_eq!(seen[0].1.lines().count(), 2);
        }
        tickets.device = "old".into();
        assert!(matches!(
            upload(&client, &url, &tickets, &[event]).await,
            Err(UploadError::SignIn(_))
        ));
    }
}
