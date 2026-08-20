//! The runnable half of OAuth (architecture §4.1): a loopback redirect server
//! and the authorization-code→token exchange. `oauth.rs` holds the pure PKCE +
//! token bookkeeping; this module does the I/O.
//!
//! Flow:
//!   1. [`LoopbackServer::bind`] on `127.0.0.1:0` (ephemeral).
//!   2. Open the system browser at the auth URL (redirect_uri points back here).
//!   3. [`LoopbackServer::wait_for_code`] receives the provider's GET, validates
//!      `state`, returns a friendly HTML page, and yields the code.
//!   4. [`exchange_code`] POSTs code + PKCE verifier to the token endpoint.
//!
//! Everything here is exercised in tests against a spun-up mock token server —
//! no live Google round-trip required to prove it works.

use super::oauth::{PkcePair, TokenSet};
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A one-shot loopback server for the OAuth redirect.
pub struct LoopbackServer {
    listener: TcpListener,
    expected_state: String,
}

impl LoopbackServer {
    pub async fn bind(expected_state: impl Into<String>) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        Ok(LoopbackServer {
            listener,
            expected_state: expected_state.into(),
        })
    }

    /// The redirect URI to register in the auth request.
    pub fn redirect_uri(&self) -> io::Result<String> {
        Ok(format!(
            "http://127.0.0.1:{}/callback",
            self.listener.local_addr()?.port()
        ))
    }

    pub fn port(&self) -> u16 {
        self.listener.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// Wait for the provider's redirect and return the authorization code.
    /// Validates `state` (CSRF defense) before accepting the code.
    pub async fn wait_for_code(self) -> anyhow::Result<String> {
        let (mut stream, _) = self.listener.accept().await?;
        let request_line = read_request_line(&mut stream).await?;
        // GET /callback?code=...&state=... HTTP/1.1
        let target = request_line
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("malformed request line"))?;
        let params = parse_query(target);

        // Error response from the provider?
        if let Some(err) = params.iter().find(|(k, _)| k == "error") {
            respond(&mut stream, "Authorization failed. You can close this tab.").await?;
            anyhow::bail!("authorization error: {}", err.1);
        }

        let state = params
            .iter()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        if state != self.expected_state {
            respond(&mut stream, "State mismatch — request rejected.").await?;
            anyhow::bail!("state mismatch (possible CSRF): got {state:?}");
        }

        let code = params
            .iter()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.clone())
            .ok_or_else(|| anyhow::anyhow!("no code in redirect"))?;

        respond(
            &mut stream,
            "Oracle of Delphi is now connected. You can close this tab and return to the assistant.",
        )
        .await?;
        Ok(code)
    }
}

/// Exchange an authorization code for tokens at the provider token endpoint.
/// `now_unix` is injected so the result's timing is deterministic in tests.
#[allow(clippy::too_many_arguments)] // OAuth code exchange has this many inherent params
pub async fn exchange_code(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    pkce: &PkcePair,
    redirect_uri: &str,
    scopes: &[String],
    now_unix: i64,
) -> anyhow::Result<TokenSet> {
    // Google's "Desktop app" client type requires the client_secret in the
    // token exchange even with PKCE (it is not a true confidential secret for
    // installed apps, but the endpoint expects it). Pure public clients omit it.
    let mut params: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("code_verifier", &pkce.verifier),
    ];
    if let Some(secret) = client_secret {
        params.push(("client_secret", secret));
    }
    let resp = client
        .post(token_endpoint)
        .form(&params)
        .send()
        .await?
        .error_for_status()?;
    let body: TokenResponse = resp.json().await?;
    Ok(TokenSet {
        access_token: body.access_token,
        refresh_token: body.refresh_token,
        expires_in_s: body.expires_in.unwrap_or(3600),
        obtained_at_unix: now_unix,
        scopes: scopes.to_vec(),
    })
}

/// Refresh an access token using a stored refresh token.
pub async fn refresh_token(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    refresh_token: &str,
    now_unix: i64,
    mut current: TokenSet,
) -> anyhow::Result<TokenSet> {
    let mut params: Vec<(&str, &str)> = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    if let Some(secret) = client_secret {
        params.push(("client_secret", secret));
    }
    let resp = client
        .post(token_endpoint)
        .form(&params)
        .send()
        .await?
        .error_for_status()?;
    let body: TokenResponse = resp.json().await?;
    // apply_refresh keeps the existing refresh token if the provider omits one.
    current.apply_refresh(
        body.access_token,
        body.refresh_token,
        body.expires_in.unwrap_or(3600),
        now_unix,
    );
    Ok(current)
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

async fn read_request_line(stream: &mut TcpStream) -> io::Result<String> {
    // Read until the first CRLF; the request line is all we need.
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        if byte[0] != b'\r' {
            buf.push(byte[0]);
        }
        if buf.len() > 8192 {
            break; // guard against a runaway line
        }
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
}

async fn respond(stream: &mut TcpStream, message: &str) -> io::Result<()> {
    let html = format!(
        "<!doctype html><html><body style=\"font-family:sans-serif;background:#04070d;color:#cfe9ff;display:flex;height:100vh;align-items:center;justify-content:center\"><h2>{message}</h2></body></html>"
    );
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.flush().await
}

/// Parse the query string out of a request target like `/callback?a=1&b=2`.
fn parse_query(target: &str) -> Vec<(String, String)> {
    let Some(q) = target.split('?').nth(1) else {
        return vec![];
    };
    q.split('&')
        .filter_map(|pair| {
            let mut it = pair.splitn(2, '=');
            let k = it.next()?.to_string();
            let v = urldecode(it.next().unwrap_or(""));
            Some((k, v))
        })
        .collect()
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::oauth::PkcePair;

    #[test]
    fn query_parsing_handles_encoding() {
        let q = parse_query("/callback?code=abc%2F123&state=xyz&scope=a+b");
        assert_eq!(q.iter().find(|(k, _)| k == "code").unwrap().1, "abc/123");
        assert_eq!(q.iter().find(|(k, _)| k == "scope").unwrap().1, "a b");
    }

    /// Spin up a mock token endpoint on loopback and run the full exchange,
    /// asserting the request body carries the PKCE verifier AND the Google
    /// client_secret (the Desktop-app requirement).
    #[tokio::test]
    async fn end_to_end_code_exchange_sends_secret_and_verifier() {
        let token_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let token_addr = token_listener.local_addr().unwrap();
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let captured2 = captured.clone();
        tokio::spawn(async move {
            let (mut s, _) = token_listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = s.read(&mut buf).await.unwrap();
            *captured2.lock().await = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = r#"{"access_token":"at-123","refresh_token":"rt-456","expires_in":3600}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            s.write_all(resp.as_bytes()).await.unwrap();
            s.flush().await.unwrap();
        });

        let pkce = PkcePair::generate();
        let client = reqwest::Client::new();
        let tokens = exchange_code(
            &client,
            &format!("http://{token_addr}/token"),
            "client-id",
            Some("GOCSPX-test-secret"),
            "auth-code-xyz",
            &pkce,
            "http://127.0.0.1:9999/callback",
            &["gmail.modify".into()],
            1000,
        )
        .await
        .unwrap();

        assert_eq!(tokens.access_token, "at-123");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt-456"));
        assert_eq!(tokens.obtained_at_unix, 1000);
        // The POST body must include the secret and the code_verifier.
        let body = captured.lock().await.clone();
        assert!(
            body.contains("client_secret=GOCSPX-test-secret"),
            "body: {body}"
        );
        assert!(body.contains("code_verifier="), "body: {body}");
    }

    /// Drive the loopback redirect server: simulate the browser hitting the
    /// callback URL and assert the code comes back with state validated.
    #[tokio::test]
    async fn loopback_receives_code_and_validates_state() {
        let server = LoopbackServer::bind("state-abc").await.unwrap();
        let redirect = server.redirect_uri().unwrap();

        // Simulate the provider's browser redirect.
        let sim = tokio::spawn(async move {
            let client = reqwest::Client::new();
            let url = format!("{redirect}?code=the-code&state=state-abc");
            let resp = client.get(url).send().await.unwrap();
            assert!(resp.status().is_success());
            let text = resp.text().await.unwrap();
            assert!(text.contains("connected"));
        });

        let code = server.wait_for_code().await.unwrap();
        assert_eq!(code, "the-code");
        sim.await.unwrap();
    }

    #[tokio::test]
    async fn loopback_rejects_bad_state() {
        let server = LoopbackServer::bind("good-state").await.unwrap();
        let redirect = server.redirect_uri().unwrap();
        let sim = tokio::spawn(async move {
            let client = reqwest::Client::new();
            let _ = client
                .get(format!("{redirect}?code=x&state=WRONG"))
                .send()
                .await;
        });
        let res = server.wait_for_code().await;
        assert!(res.is_err(), "state mismatch must be rejected");
        let _ = sim.await;
    }

    /// Refresh flow keeps an existing refresh token when the provider omits one.
    #[tokio::test]
    async fn refresh_preserves_refresh_token_when_omitted() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            // No refresh_token in the response.
            let body = r#"{"access_token":"new-at","expires_in":3600}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            s.write_all(resp.as_bytes()).await.unwrap();
        });

        let current = TokenSet {
            access_token: "old-at".into(),
            refresh_token: Some("keep-me".into()),
            expires_in_s: 3600,
            obtained_at_unix: 0,
            scopes: vec![],
        };
        let client = reqwest::Client::new();
        let refreshed = refresh_token(
            &client,
            &format!("http://{addr}/token"),
            "cid",
            Some("secret"),
            "keep-me",
            500,
            current,
        )
        .await
        .unwrap();
        assert_eq!(refreshed.access_token, "new-at");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("keep-me"));
        assert_eq!(refreshed.obtained_at_unix, 500);
    }
}
