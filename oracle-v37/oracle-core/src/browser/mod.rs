//! Delphi's eyes and hands on the web — a minimal Chrome DevTools Protocol
//! client. CDP is just JSON over a WebSocket, so rather than pull in a heavy
//! browser crate we speak the few methods we need directly: launch (or reuse) a
//! Chrome started with remote debugging, then drive one tab — navigate it, read
//! the page (title + visible text + links, via injected JS), and click elements
//! by their visible text.
//!
//! Chrome ≥136 refuses `--remote-debugging-port` on the *default* profile, so we
//! drive a dedicated persistent profile dir (logins the user signs into there
//! survive across sessions). The browser is launched lazily on first use and
//! left running — it outlives core, like a browser should.

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

/// Where the managed Chrome lives and how to reach its debugger.
#[derive(Debug, Clone)]
pub struct BrowserConfig {
    /// Path to chrome.exe. Empty → auto-detect the common install locations.
    pub chrome_path: String,
    /// Persistent profile dir. A DEDICATED dir (not the user's default profile,
    /// which Chrome blocks from debugging) — the user signs into sites here once.
    pub user_data_dir: String,
    /// Remote-debugging port.
    pub port: u16,
    /// Run Chrome headless (no visible window). Off by default — this is the
    /// user's browser and they want to see it.
    pub headless: bool,
    /// Extra launch flags (escape hatch; e.g. `--no-sandbox` in a container).
    pub extra_args: Vec<String>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        BrowserConfig {
            chrome_path: String::new(),
            user_data_dir: default_profile_dir(),
            port: 9222,
            headless: false,
            extra_args: Vec::new(),
        }
    }
}

/// A concise view of a web page for the model to reason over.
#[derive(Debug, serde::Serialize)]
pub struct PageView {
    pub url: String,
    pub title: String,
    pub text: String,
    pub links: Vec<Link>,
}

#[derive(Debug, serde::Serialize)]
pub struct Link {
    pub text: String,
    pub href: String,
}

/// The managed browser. Lazily launches Chrome and drives one tab.
pub struct BrowserHandle {
    cfg: BrowserConfig,
    inner: Mutex<Inner>,
    http: reqwest::Client,
}

#[derive(Default)]
struct Inner {
    /// WebSocket debugger URL of the tab we control (None until first `open`).
    target_ws: Option<String>,
}

impl BrowserHandle {
    pub fn new(cfg: BrowserConfig) -> Self {
        BrowserHandle {
            cfg,
            inner: Mutex::new(Inner::default()),
            http: reqwest::Client::new(),
        }
    }

    fn dev_url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.cfg.port, path)
    }

    /// Ensure Chrome is up with remote debugging on our port; launch it if not.
    async fn ensure_running(&self) -> Result<()> {
        if self.is_up().await {
            return Ok(());
        }
        let exe = resolve_chrome(&self.cfg.chrome_path).ok_or_else(|| {
            anyhow!("could not find Chrome; set [browser] chrome_path in oracle.toml")
        })?;
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg(format!("--remote-debugging-port={}", self.cfg.port))
            .arg(format!("--user-data-dir={}", self.cfg.user_data_dir))
            // A first-run/new-tab page keeps the window clean and predictable.
            .arg("--no-first-run")
            .arg("--no-default-browser-check");
        if self.cfg.headless {
            cmd.arg("--headless=new");
        }
        for a in &self.cfg.extra_args {
            cmd.arg(a);
        }
        cmd.arg("about:blank");
        // Chrome must be VISIBLE (this is the user's browser), so no window-hiding.
        cmd.spawn()
            .with_context(|| format!("launching Chrome at {exe}"))?;
        // Drop the child handle: Chrome should outlive us.

        // Poll for the debugger to come up (cold start can take a few seconds).
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(400)).await;
            if self.is_up().await {
                return Ok(());
            }
        }
        Err(anyhow!(
            "Chrome did not expose its debugger on port {} — is another Chrome already using that profile?",
            self.cfg.port
        ))
    }

    async fn is_up(&self) -> bool {
        self.http
            .get(self.dev_url("/json/version"))
            .timeout(Duration::from_millis(800))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    /// The ws URL of our controlled tab, creating a fresh tab if we don't have a
    /// live one yet.
    async fn target(&self) -> Result<String> {
        {
            let g = self.inner.lock().await;
            if let Some(ws) = &g.target_ws {
                return Ok(ws.clone());
            }
        }
        let ws = self.new_tab("about:blank").await?;
        self.inner.lock().await.target_ws = Some(ws.clone());
        Ok(ws)
    }

    /// Create a new tab at `url`; return its debugger ws URL. Chrome ≥111 wants
    /// PUT for /json/new.
    async fn new_tab(&self, url: &str) -> Result<String> {
        let resp = self
            .http
            .put(self.dev_url(&format!("/json/new?{url}")))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .context("creating a new tab")?;
        let v: Value = resp.json().await.context("parsing new-tab response")?;
        v.get("webSocketDebuggerUrl")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("new tab had no debugger URL"))
    }

    /// Navigate the controlled tab to `url` (creating it if needed), wait for the
    /// load to settle, and return the resulting page view.
    pub async fn open(&self, url: &str) -> Result<PageView> {
        self.ensure_running().await?;
        let url = normalize_url(url);
        // Get (or create) the tab, then navigate via CDP so the URL travels as a
        // JSON param — no query-string encoding pitfalls (data: URLs, spaces, &).
        let ws = self.target().await?;
        cdp_call(&ws, "Page.navigate", json!({ "url": url })).await?;
        self.wait_for_load(&ws).await;
        self.read_view(&ws).await
    }

    /// Read the current page without navigating.
    pub async fn read(&self) -> Result<PageView> {
        self.ensure_running().await?;
        let ws = self.target().await?;
        self.read_view(&ws).await
    }

    /// Click the first visible link/button whose text contains `text`.
    pub async fn click(&self, text: &str) -> Result<PageView> {
        self.ensure_running().await?;
        let ws = self.target().await?;
        let needle = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
        let js = format!(
            r#"(() => {{
                const n = {needle}.toLowerCase();
                const els = [...document.querySelectorAll('a,button,[role=button],[role=link],input[type=submit],input[type=button],[onclick]')];
                const el = els.find(e => ((e.innerText||e.textContent||e.value||e.getAttribute('aria-label')||'').toLowerCase().includes(n)));
                if (!el) return JSON.stringify({{clicked:false}});
                el.scrollIntoView({{block:'center'}}); el.click();
                return JSON.stringify({{clicked:true, label:(el.innerText||el.textContent||el.value||'').trim().slice(0,120)}});
            }})()"#
        );
        let val = self.eval_string(&ws, &js).await?;
        let parsed: Value = serde_json::from_str(&val).unwrap_or(Value::Null);
        if parsed.get("clicked").and_then(|c| c.as_bool()) != Some(true) {
            return Err(anyhow!("no clickable element matching '{text}'"));
        }
        // A click often navigates; give it a moment, then report the new page.
        tokio::time::sleep(Duration::from_millis(600)).await;
        self.wait_for_load(&ws).await;
        self.read_view(&ws).await
    }

    /// Poll document.readyState until complete (bounded), so reads see a loaded
    /// page rather than a blank in-flight one.
    async fn wait_for_load(&self, ws: &str) {
        for _ in 0..25 {
            if let Ok(s) = self.eval_string(ws, "document.readyState").await {
                if s == "complete" {
                    // A short settle for SPA content to paint after 'complete'.
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    /// Extract {title,url,text,links} from the live page.
    async fn read_view(&self, ws: &str) -> Result<PageView> {
        let js = r#"(() => {
            const links = [...document.querySelectorAll('a[href]')]
                .map(a => ({text:(a.innerText||a.textContent||a.getAttribute('aria-label')||'').trim().replace(/\s+/g,' ').slice(0,120), href:a.href}))
                .filter(l => l.text && l.href && !l.href.startsWith('javascript:'))
                .slice(0, 40);
            const text = (document.body ? document.body.innerText : '').replace(/\n{3,}/g,'\n\n').trim().slice(0, 4000);
            return JSON.stringify({title: document.title, url: location.href, text, links});
        })()"#;
        let raw = self.eval_string(ws, js).await?;
        let v: Value = serde_json::from_str(&raw).context("parsing page view")?;
        Ok(PageView {
            url: v["url"].as_str().unwrap_or_default().to_string(),
            title: v["title"].as_str().unwrap_or_default().to_string(),
            text: v["text"].as_str().unwrap_or_default().to_string(),
            links: v["links"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|l| Link {
                            text: l["text"].as_str().unwrap_or_default().to_string(),
                            href: l["href"].as_str().unwrap_or_default().to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// Run a JS expression in the page and return its string result.
    async fn eval_string(&self, ws: &str, expression: &str) -> Result<String> {
        let result = cdp_call(
            ws,
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
        )
        .await?;
        if let Some(exc) = result.get("exceptionDetails") {
            return Err(anyhow!("page script error: {exc}"));
        }
        Ok(result
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string())
    }
}

/// One CDP request/response over a fresh WebSocket to a target. Reads messages
/// until the reply with our id arrives (skipping unsolicited events).
async fn cdp_call(ws_url: &str, method: &str, params: Value) -> Result<Value> {
    let (mut socket, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .with_context(|| format!("connecting to CDP target {ws_url}"))?;
    let id: u64 = 1;
    let msg = json!({ "id": id, "method": method, "params": params }).to_string();
    socket.send(Message::Text(msg)).await?;
    loop {
        match tokio::time::timeout(Duration::from_secs(20), socket.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: Value = serde_json::from_str(&t)?;
                if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                    if let Some(err) = v.get("error") {
                        return Err(anyhow!("CDP {method} error: {err}"));
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
                // otherwise it's an event — keep reading.
            }
            Ok(Some(Ok(_))) => {} // ping/pong/binary
            Ok(Some(Err(e))) => return Err(e.into()),
            Ok(None) => return Err(anyhow!("CDP socket closed before reply")),
            Err(_) => return Err(anyhow!("CDP {method} timed out")),
        }
    }
}

/// Add a scheme if the user/model gave a bare host ("youtube.com" → https://…).
fn normalize_url(url: &str) -> String {
    let u = url.trim();
    if u.starts_with("http://")
        || u.starts_with("https://")
        || u.starts_with("about:")
        || u.starts_with("file:")
    {
        u.to_string()
    } else {
        format!("https://{u}")
    }
}

/// Default dedicated profile dir: %LOCALAPPDATA%\oracle\chrome on Windows, else
/// a temp-based path (used for tests/dev).
fn default_profile_dir() -> String {
    #[cfg(windows)]
    {
        let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
        format!(r"{base}\oracle\chrome")
    }
    #[cfg(not(windows))]
    {
        std::env::temp_dir()
            .join("oracle-chrome")
            .to_string_lossy()
            .into_owned()
    }
}

/// Find chrome.exe: an explicit path, then the common Windows install locations,
/// then `chrome`/`google-chrome`/`chromium` on PATH.
fn resolve_chrome(configured: &str) -> Option<String> {
    if !configured.trim().is_empty() {
        return Some(configured.to_string());
    }
    #[cfg(windows)]
    {
        for var in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
            if let Ok(base) = std::env::var(var) {
                let p = format!(r"{base}\Google\Chrome\Application\chrome.exe");
                if std::path::Path::new(&p).exists() {
                    return Some(p);
                }
            }
        }
        Some("chrome".to_string())
    }
    #[cfg(not(windows))]
    {
        for p in [
            "/opt/pw-browsers/chromium",
            "/usr/bin/google-chrome",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
        ] {
            if std::path::Path::new(p).exists() {
                return Some(p.to_string());
            }
        }
        Some("chromium".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_normalization() {
        assert_eq!(normalize_url("youtube.com"), "https://youtube.com");
        assert_eq!(normalize_url("https://x.com"), "https://x.com");
        assert_eq!(normalize_url("  http://a.b "), "http://a.b");
        assert_eq!(normalize_url("about:blank"), "about:blank");
    }

    /// End-to-end CDP smoke test against a real (headless) Chromium. Ignored by
    /// default (needs a browser + spawns a process); run explicitly:
    ///   cargo test -p oracle-core browser:: -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn cdp_open_read_click_roundtrip() {
        let dir = std::env::temp_dir().join(format!("oracle-cdp-test-{}", uuid::Uuid::new_v4()));
        let cfg = BrowserConfig {
            chrome_path: String::new(), // resolve_chrome finds /opt/pw-browsers/chromium
            user_data_dir: dir.to_string_lossy().into_owned(),
            port: 9333,
            headless: true,
            extra_args: vec![
                "--no-sandbox".into(),
                "--disable-gpu".into(),
                "--disable-dev-shm-usage".into(),
            ],
        };
        let b = BrowserHandle::new(cfg);
        let html = "<title>T</title><h1>Hello Oracle</h1><a href=\"https://example.com/vid\">Watch This Video</a>";
        let file = std::env::temp_dir().join(format!("oracle-cdp-{}.html", uuid::Uuid::new_v4()));
        std::fs::write(&file, html).unwrap();
        let page = format!("file://{}", file.to_string_lossy());
        let view = b.open(&page).await.expect("open");
        assert!(view.text.contains("Hello Oracle"), "text: {}", view.text);
        assert!(
            view.links
                .iter()
                .any(|l| l.text.contains("Watch This Video")),
            "links: {:?}",
            view.links
        );
        let after = b.click("Watch This Video").await.expect("click");
        assert!(
            after.url.contains("example.com"),
            "navigated to: {}",
            after.url
        );
    }
}
