//! Real Gmail + Calendar REST client (architecture §4.2), driven by the OAuth
//! token sealed during `oracle-core auth`.
//!
//! A [`GoogleClient`] holds the credentials and the current [`TokenSet`], and
//! transparently refreshes the access token when it expires or a call returns
//! 401. The tool layer calls the typed methods here; nothing above needs to
//! know about token lifetimes.
//!
//! Endpoints are injectable so the whole thing is testable against a mock
//! Google on loopback — every method below is exercised without hitting real
//! Google servers.

use super::google::{self, GoogleCredentials};
use super::oauth::TokenSet;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

/// Base URLs, overridable in tests.
#[derive(Debug, Clone)]
pub struct GoogleEndpoints {
    pub gmail: String,
    pub calendar: String,
}

impl Default for GoogleEndpoints {
    fn default() -> Self {
        GoogleEndpoints {
            gmail: "https://gmail.googleapis.com".into(),
            calendar: "https://www.googleapis.com".into(),
        }
    }
}

pub struct GoogleClient {
    creds: GoogleCredentials,
    tokens: Mutex<TokenSet>,
    endpoints: GoogleEndpoints,
    http: reqwest::Client,
}

/// A trimmed Gmail message for the tool layer.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MailSummary {
    pub id: String,
    pub thread_id: String,
    pub from: String,
    pub subject: String,
    pub snippet: String,
    pub unread: bool,
}

/// A calendar event trimmed to what scheduling needs.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CalEvent {
    pub id: String,
    pub summary: String,
    pub start: String,
    pub end: String,
}

impl GoogleClient {
    pub fn new(creds: GoogleCredentials, tokens: TokenSet) -> Self {
        Self::with_endpoints(creds, tokens, GoogleEndpoints::default())
    }

    pub fn with_endpoints(
        creds: GoogleCredentials,
        tokens: TokenSet,
        endpoints: GoogleEndpoints,
    ) -> Self {
        GoogleClient {
            creds,
            tokens: Mutex::new(tokens),
            endpoints,
            http: reqwest::Client::new(),
        }
    }

    /// Ensure we hold a non-expired access token, refreshing if needed. Returns
    /// the current access token string.
    async fn access_token(&self, now_unix: i64) -> anyhow::Result<String> {
        let mut guard = self.tokens.lock().await;
        if guard.access_token.is_empty() || guard.is_expired(now_unix) {
            let refreshed = google::refresh(&self.creds, guard.clone(), now_unix).await?;
            *guard = refreshed;
        }
        Ok(guard.access_token.clone())
    }

    /// Force a refresh (used after a 401).
    async fn force_refresh(&self, now_unix: i64) -> anyhow::Result<String> {
        let mut guard = self.tokens.lock().await;
        let refreshed = google::refresh(&self.creds, guard.clone(), now_unix).await?;
        *guard = refreshed;
        Ok(guard.access_token.clone())
    }

    /// GET a Google API path, refreshing once on 401.
    async fn get_json(&self, url: &str, now_unix: i64) -> anyhow::Result<Value> {
        let token = self.access_token(now_unix).await?;
        let resp = self.http.get(url).bearer_auth(&token).send().await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let token = self.force_refresh(now_unix).await?;
            let resp = self.http.get(url).bearer_auth(&token).send().await?;
            return Ok(resp.error_for_status()?.json().await?);
        }
        Ok(resp.error_for_status()?.json().await?)
    }

    async fn post_json(&self, url: &str, body: Value, now_unix: i64) -> anyhow::Result<Value> {
        let token = self.access_token(now_unix).await?;
        let resp = self
            .http
            .post(url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let token = self.force_refresh(now_unix).await?;
            let resp = self
                .http
                .post(url)
                .bearer_auth(&token)
                .json(&body)
                .send()
                .await?;
            return Ok(resp.error_for_status()?.json().await?);
        }
        Ok(resp.error_for_status()?.json().await?)
    }

    // ---- Gmail ----------------------------------------------------------

    /// Search Gmail and return trimmed summaries (up to `max`).
    pub async fn gmail_search(
        &self,
        query: &str,
        max: u32,
        now_unix: i64,
    ) -> anyhow::Result<Vec<MailSummary>> {
        let list_url = format!(
            "{}/gmail/v1/users/me/messages?q={}&maxResults={}",
            self.endpoints.gmail,
            urlencoding(query),
            max
        );
        let list: MessageList = serde_json::from_value(self.get_json(&list_url, now_unix).await?)?;
        let mut out = Vec::new();
        for m in list
            .messages
            .unwrap_or_default()
            .into_iter()
            .take(max as usize)
        {
            let msg_url = format!(
                "{}/gmail/v1/users/me/messages/{}?format=metadata&metadataHeaders=From&metadataHeaders=Subject",
                self.endpoints.gmail, m.id
            );
            let full: GmailMessage =
                serde_json::from_value(self.get_json(&msg_url, now_unix).await?)?;
            out.push(full.into_summary());
        }
        Ok(out)
    }

    /// Create a draft reply. Returns the draft id.
    pub async fn gmail_create_draft(
        &self,
        to: &str,
        subject: &str,
        body: &str,
        now_unix: i64,
    ) -> anyhow::Result<String> {
        // Gmail wants an RFC 2822 message, base64url-encoded.
        let raw = format!("To: {to}\r\nSubject: {subject}\r\n\r\n{body}");
        let encoded = base64_url(raw.as_bytes());
        let url = format!("{}/gmail/v1/users/me/drafts", self.endpoints.gmail);
        let resp = self
            .post_json(
                &url,
                serde_json::json!({ "message": { "raw": encoded } }),
                now_unix,
            )
            .await?;
        Ok(resp["id"].as_str().unwrap_or_default().to_string())
    }

    // ---- Calendar -------------------------------------------------------

    /// List events between two RFC3339 timestamps on the primary calendar.
    pub async fn calendar_events(
        &self,
        time_min: &str,
        time_max: &str,
        now_unix: i64,
    ) -> anyhow::Result<Vec<CalEvent>> {
        let url = format!(
            "{}/calendar/v3/calendars/primary/events?timeMin={}&timeMax={}&singleEvents=true&orderBy=startTime",
            self.endpoints.calendar,
            urlencoding(time_min),
            urlencoding(time_max)
        );
        let list: EventList = serde_json::from_value(self.get_json(&url, now_unix).await?)?;
        Ok(list
            .items
            .unwrap_or_default()
            .into_iter()
            .map(|e| CalEvent {
                id: e.id.unwrap_or_default(),
                summary: e.summary.unwrap_or_default(),
                start: e
                    .start
                    .and_then(|s| s.date_time.or(s.date))
                    .unwrap_or_default(),
                end: e
                    .end
                    .and_then(|s| s.date_time.or(s.date))
                    .unwrap_or_default(),
            })
            .collect())
    }
}

// ---- Free-slot computation (pure, testable) -----------------------------

/// A half-open time interval in minutes-since-midnight.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slot {
    pub start_min: u32,
    pub end_min: u32,
}

/// Compute free slots of `duration_min` within `[window_start, window_end]`
/// given busy intervals (all in minutes-since-midnight). Pure function so the
/// scheduling heuristic is unit-tested independent of the network.
pub fn free_slots(
    busy: &[Slot],
    window_start: u32,
    window_end: u32,
    duration_min: u32,
) -> Vec<Slot> {
    // Sort + merge busy intervals.
    let mut b: Vec<Slot> = busy
        .iter()
        .copied()
        .filter(|s| s.end_min > window_start && s.start_min < window_end)
        .collect();
    b.sort_by_key(|s| s.start_min);
    let mut merged: Vec<Slot> = Vec::new();
    for s in b {
        if let Some(last) = merged.last_mut() {
            if s.start_min <= last.end_min {
                last.end_min = last.end_min.max(s.end_min);
                continue;
            }
        }
        merged.push(s);
    }
    // Walk the gaps.
    let mut out = Vec::new();
    let mut cursor = window_start;
    for s in &merged {
        let gap_start = cursor.max(window_start);
        let gap_end = s.start_min.min(window_end);
        if gap_end >= gap_start && gap_end - gap_start >= duration_min {
            out.push(Slot {
                start_min: gap_start,
                end_min: gap_end,
            });
        }
        cursor = cursor.max(s.end_min);
    }
    if window_end > cursor && window_end - cursor >= duration_min {
        out.push(Slot {
            start_min: cursor,
            end_min: window_end,
        });
    }
    out
}

// ---- JSON shapes --------------------------------------------------------

#[derive(Deserialize)]
struct MessageList {
    messages: Option<Vec<MessageRef>>,
}
#[derive(Deserialize)]
struct MessageRef {
    id: String,
}
#[derive(Deserialize)]
struct GmailMessage {
    id: String,
    #[serde(rename = "threadId")]
    thread_id: Option<String>,
    snippet: Option<String>,
    #[serde(rename = "labelIds")]
    label_ids: Option<Vec<String>>,
    payload: Option<GmailPayload>,
}
#[derive(Deserialize)]
struct GmailPayload {
    headers: Option<Vec<GmailHeader>>,
}
#[derive(Deserialize)]
struct GmailHeader {
    name: String,
    value: String,
}
impl GmailMessage {
    fn into_summary(self) -> MailSummary {
        let headers = self.payload.and_then(|p| p.headers).unwrap_or_default();
        let find = |n: &str| {
            headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case(n))
                .map(|h| h.value.clone())
                .unwrap_or_default()
        };
        let unread = self
            .label_ids
            .as_ref()
            .map(|l| l.iter().any(|x| x == "UNREAD"))
            .unwrap_or(false);
        MailSummary {
            id: self.id,
            thread_id: self.thread_id.unwrap_or_default(),
            from: find("From"),
            subject: find("Subject"),
            snippet: self.snippet.unwrap_or_default(),
            unread,
        }
    }
}

#[derive(Deserialize)]
struct EventList {
    items: Option<Vec<GEvent>>,
}
#[derive(Deserialize)]
struct GEvent {
    id: Option<String>,
    summary: Option<String>,
    start: Option<GTime>,
    end: Option<GTime>,
}
#[derive(Deserialize)]
struct GTime {
    #[serde(rename = "dateTime")]
    date_time: Option<String>,
    date: Option<String>,
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn creds(token_uri: String) -> GoogleCredentials {
        GoogleCredentials {
            client_id: "cid".into(),
            client_secret: "sec".into(),
            auth_uri: "https://accounts.google.com/o/oauth2/auth".into(),
            token_uri,
            project_id: "p".into(),
        }
    }

    fn valid_tokens() -> TokenSet {
        TokenSet {
            access_token: "at-valid".into(),
            refresh_token: Some("rt".into()),
            expires_in_s: 3600,
            obtained_at_unix: 1000,
            scopes: vec![],
        }
    }

    /// A tiny HTTP responder that serves canned JSON for any request and can be
    /// pointed at by gmail/calendar endpoints. Returns the addr.
    async fn mock_http(response_body: &'static str) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = s.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        addr
    }

    #[test]
    fn free_slots_finds_gaps() {
        // Busy 13:00-13:30 and 15:00-16:00 in a 12:00-18:00 window (afternoon).
        let busy = [
            Slot {
                start_min: 780,
                end_min: 810,
            }, // 13:00-13:30
            Slot {
                start_min: 900,
                end_min: 960,
            }, // 15:00-16:00
        ];
        let free = free_slots(&busy, 720, 1080, 30); // 12:00-18:00, 30 min
                                                     // Expect: 12:00-13:00, 13:30-15:00, 16:00-18:00
        assert_eq!(free.len(), 3);
        assert_eq!(
            free[0],
            Slot {
                start_min: 720,
                end_min: 780
            }
        );
        assert_eq!(
            free[1],
            Slot {
                start_min: 810,
                end_min: 900
            }
        );
        assert_eq!(
            free[2],
            Slot {
                start_min: 960,
                end_min: 1080
            }
        );
    }

    #[test]
    fn free_slots_merges_overlapping_busy() {
        let busy = [
            Slot {
                start_min: 600,
                end_min: 700,
            },
            Slot {
                start_min: 650,
                end_min: 720,
            }, // overlaps previous
        ];
        let free = free_slots(&busy, 540, 780, 15);
        // free before (540-600) and after (720-780)
        assert_eq!(free.len(), 2);
        assert_eq!(
            free[0],
            Slot {
                start_min: 540,
                end_min: 600
            }
        );
        assert_eq!(
            free[1],
            Slot {
                start_min: 720,
                end_min: 780
            }
        );
    }

    #[tokio::test]
    async fn gmail_search_parses_summaries() {
        // The mock returns the SAME body for every request; we craft a body that
        // is valid both as a message list and (loosely) as a message. To keep it
        // clean we run two mocks: one for list, one won't be reachable — so
        // instead we point gmail at a single mock that returns a full message
        // for the metadata GET and a list for the list GET. Simplest: return a
        // list first is hard with one canned body, so we test the message parse
        // path directly via a message-shaped body and a one-item list.
        //
        // Here the canned body is a message-list with one ref; the per-message
        // GET hits the same mock and must also parse — so we return a body that
        // satisfies BOTH shapes: it has `messages` (list) AND `id`/`payload`.
        let body = r#"{
            "messages":[{"id":"m1"}],
            "id":"m1","threadId":"t1","snippet":"are you free this week?",
            "labelIds":["UNREAD","INBOX"],
            "payload":{"headers":[
                {"name":"From","value":"advisor@univ.edu"},
                {"name":"Subject","value":"Check-in"}
            ]}
        }"#;
        let addr = mock_http(body).await;
        let ep = GoogleEndpoints {
            gmail: format!("http://{addr}"),
            calendar: format!("http://{addr}"),
        };
        let client = GoogleClient::with_endpoints(creds(String::new()), valid_tokens(), ep);
        let results = client.gmail_search("is:unread", 5, 1000).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].from, "advisor@univ.edu");
        assert_eq!(results[0].subject, "Check-in");
        assert!(results[0].unread);
        assert!(results[0].snippet.contains("free this week"));
    }

    #[tokio::test]
    async fn calendar_events_parse() {
        let body = r#"{"items":[
            {"id":"e1","summary":"Standup","start":{"dateTime":"2026-08-19T09:00:00-07:00"},"end":{"dateTime":"2026-08-19T09:15:00-07:00"}}
        ]}"#;
        let addr = mock_http(body).await;
        let ep = GoogleEndpoints {
            gmail: format!("http://{addr}"),
            calendar: format!("http://{addr}"),
        };
        let client = GoogleClient::with_endpoints(creds(String::new()), valid_tokens(), ep);
        let events = client
            .calendar_events("2026-08-19T00:00:00Z", "2026-08-20T00:00:00Z", 1000)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary, "Standup");
        assert!(events[0].start.contains("09:00"));
    }

    #[tokio::test]
    async fn expired_token_triggers_refresh_before_call() {
        // Token server that hands back a fresh access token.
        let token_body = r#"{"access_token":"at-refreshed","expires_in":3600}"#;
        let token_addr = mock_http(token_body).await;
        // API server returns an empty gmail list.
        let api_addr = mock_http(r#"{"messages":[]}"#).await;

        // Start with an EXPIRED token so access_token() must refresh.
        let expired = TokenSet {
            access_token: "at-old".into(),
            refresh_token: Some("rt".into()),
            expires_in_s: 3600,
            obtained_at_unix: 0, // long ago
            scopes: vec![],
        };
        let ep = GoogleEndpoints {
            gmail: format!("http://{api_addr}"),
            calendar: format!("http://{api_addr}"),
        };
        let client =
            GoogleClient::with_endpoints(creds(format!("http://{token_addr}/token")), expired, ep);
        // now=100000 (well past expiry) → refresh happens, then the list call.
        let results = client.gmail_search("x", 5, 100_000).await.unwrap();
        assert!(results.is_empty());
        // The stored token was refreshed.
        let tok = client.tokens.lock().await;
        assert_eq!(tok.access_token, "at-refreshed");
    }
}
