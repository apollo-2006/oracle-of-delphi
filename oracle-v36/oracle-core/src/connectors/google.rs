//! Google Workspace glue: load a Desktop-app `credentials.json`, run the full
//! authorization (browser → loopback → token exchange), and seal the tokens
//! into the vault (architecture §4.1).
//!
//! The `credentials.json` is the standard Google Cloud "OAuth client — Desktop
//! app" download. We parse the `installed` object. The client_secret it carries
//! is not a true confidential secret for installed apps, but Google's token
//! endpoint requires it in the exchange, so we load and forward it — and we
//! never log it (all logging goes through `security::redact`).

use super::oauth::{PkcePair, TokenSet};
use super::oauth_flow::{exchange_code, refresh_token, LoopbackServer};
use super::vault::{SealedToken, TokenVault};
use serde::Deserialize;

/// Parsed Google Desktop-app OAuth client credentials.
#[derive(Debug, Clone)]
pub struct GoogleCredentials {
    pub client_id: String,
    pub client_secret: String,
    pub auth_uri: String,
    pub token_uri: String,
    pub project_id: String,
}

#[derive(Deserialize)]
struct CredsFile {
    installed: Option<InstalledClient>,
    web: Option<InstalledClient>,
}

#[derive(Deserialize)]
struct InstalledClient {
    client_id: String,
    #[serde(default)]
    client_secret: String,
    auth_uri: String,
    token_uri: String,
    #[serde(default)]
    project_id: String,
}

impl GoogleCredentials {
    /// Load from a `credentials.json` path.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::from_json(&text)
    }

    /// Parse from JSON text (supports both `installed` and `web` client shapes).
    pub fn from_json(text: &str) -> anyhow::Result<Self> {
        let f: CredsFile = serde_json::from_str(text)?;
        let c = f.installed.or(f.web).ok_or_else(|| {
            anyhow::anyhow!("credentials.json has no 'installed' or 'web' object")
        })?;
        if c.client_id.is_empty() {
            anyhow::bail!("credentials.json missing client_id");
        }
        Ok(GoogleCredentials {
            client_id: c.client_id,
            client_secret: c.client_secret,
            auth_uri: c.auth_uri,
            token_uri: c.token_uri,
            project_id: c.project_id,
        })
    }

    /// The default Workspace scopes Oracle of Delphi requests (minimal, incremental).
    pub fn default_scopes() -> Vec<String> {
        [
            "https://www.googleapis.com/auth/gmail.modify",
            "https://www.googleapis.com/auth/calendar.events",
            "https://www.googleapis.com/auth/tasks",
            "https://www.googleapis.com/auth/contacts.readonly",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }
}

/// Run the interactive authorization: bind loopback, print the URL for the user
/// to open, receive the code, exchange it, and return the tokens. `open_browser`
/// is called with the auth URL (the CLI passes a function that launches the
/// system browser; tests pass a closure that drives the redirect directly).
pub async fn authorize<F, Fut>(
    creds: &GoogleCredentials,
    scopes: &[String],
    now_unix: i64,
    open_browser: F,
) -> anyhow::Result<TokenSet>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    // A random state for CSRF protection.
    let state = random_state();
    let server = LoopbackServer::bind(state.clone()).await?;
    let redirect_uri = server.redirect_uri()?;
    let pkce = PkcePair::generate();
    let auth_url = pkce.auth_url(
        &creds.auth_uri,
        &creds.client_id,
        &redirect_uri,
        scopes,
        &state,
    );

    // Hand the URL to the caller (opens the browser in production).
    open_browser(auth_url).await;

    // Wait for the redirect with the code.
    let code = server.wait_for_code().await?;

    let client = reqwest::Client::new();
    exchange_code(
        &client,
        &creds.token_uri,
        &creds.client_id,
        Some(&creds.client_secret),
        &code,
        &pkce,
        &redirect_uri,
        scopes,
        now_unix,
    )
    .await
}

/// Refresh using stored credentials + the Google client secret.
pub async fn refresh(
    creds: &GoogleCredentials,
    current: TokenSet,
    now_unix: i64,
) -> anyhow::Result<TokenSet> {
    let Some(rt) = current.refresh_token.clone() else {
        anyhow::bail!("no refresh token stored; re-authorize");
    };
    let client = reqwest::Client::new();
    refresh_token(
        &client,
        &creds.token_uri,
        &creds.client_id,
        Some(&creds.client_secret),
        &rt,
        now_unix,
        current,
    )
    .await
}

/// Seal a token set's refresh token into the vault under the Google account.
pub fn seal_tokens(
    vault: &TokenVault,
    account: &str,
    tokens: &TokenSet,
) -> anyhow::Result<Option<SealedToken>> {
    match &tokens.refresh_token {
        Some(rt) => {
            let sealed = vault
                .seal("google", account, &tokens.scopes, rt.as_bytes())
                .map_err(|e| anyhow::anyhow!("vault seal: {e}"))?;
            Ok(Some(sealed))
        }
        None => Ok(None),
    }
}

fn random_state() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"installed":{"client_id":"123.apps.googleusercontent.com","project_id":"proj-1","auth_uri":"https://accounts.google.com/o/oauth2/auth","token_uri":"https://oauth2.googleapis.com/token","client_secret":"GOCSPX-xxx","redirect_uris":["http://localhost"]}}"#;

    #[test]
    fn parses_installed_credentials() {
        let c = GoogleCredentials::from_json(SAMPLE).unwrap();
        assert_eq!(c.client_id, "123.apps.googleusercontent.com");
        assert_eq!(c.client_secret, "GOCSPX-xxx");
        assert_eq!(c.token_uri, "https://oauth2.googleapis.com/token");
        assert_eq!(c.project_id, "proj-1");
    }

    #[test]
    fn rejects_malformed_credentials() {
        assert!(GoogleCredentials::from_json("{}").is_err());
        assert!(GoogleCredentials::from_json("{\"installed\":{}}").is_err());
    }

    #[test]
    fn default_scopes_are_minimal() {
        let s = GoogleCredentials::default_scopes();
        assert!(s.iter().any(|x| x.contains("gmail.modify")));
        assert!(s.iter().any(|x| x.contains("calendar.events")));
        assert!(!s.iter().any(|x| x.contains("gmail.send"))); // not requested
    }

    /// Full authorize flow against the loopback server, driving the redirect
    /// ourselves in place of a browser, then a mock token endpoint.
    #[tokio::test]
    async fn authorize_end_to_end_with_mock_browser_and_token_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // Mock Google token endpoint.
        let token_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let token_addr = token_listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = token_listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = s.read(&mut buf).await;
            let body = r#"{"access_token":"at","refresh_token":"rt","expires_in":3600}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes()).await;
        });

        let creds = GoogleCredentials {
            client_id: "cid".into(),
            client_secret: "GOCSPX-secret".into(),
            auth_uri: "https://accounts.google.com/o/oauth2/auth".into(),
            token_uri: format!("http://{token_addr}/token"),
            project_id: "p".into(),
        };

        // "open_browser" instead drives the redirect back to the loopback,
        // extracting the redirect_uri + state from the auth URL.
        let tokens = authorize(
            &creds,
            &["gmail.modify".into()],
            1000,
            |auth_url| async move {
                // A real browser returns control immediately; we must NOT await the
                // redirect's response here (the loopback only replies once
                // wait_for_code runs, which is after this returns). Spawn it.
                let redirect = extract_param(&auth_url, "redirect_uri");
                let state = extract_param(&auth_url, "state");
                tokio::spawn(async move {
                    let client = reqwest::Client::new();
                    let url = format!("{redirect}?code=the-code&state={state}");
                    let _ = client.get(url).send().await;
                });
            },
        )
        .await
        .unwrap();

        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt"));

        // And the tokens seal into the vault.
        let vault = TokenVault::new(&[9u8; 32]);
        let sealed = seal_tokens(&vault, "abir@gmail.com", &tokens)
            .unwrap()
            .unwrap();
        let opened = vault
            .open("google", "abir@gmail.com", &tokens.scopes, &sealed)
            .unwrap();
        assert_eq!(opened, b"rt");
    }

    fn extract_param(url: &str, key: &str) -> String {
        let q = url.split('?').nth(1).unwrap_or("");
        for pair in q.split('&') {
            if let Some(v) = pair.strip_prefix(&format!("{key}=")) {
                return urldecode(v);
            }
        }
        String::new()
    }

    fn urldecode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).to_string()
    }
}
