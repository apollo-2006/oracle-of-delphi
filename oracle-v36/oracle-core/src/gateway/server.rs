//! Live WebSocket gateway to the HUD (architecture §6.1).
//!
//! Binds a loopback TCP listener, upgrades to WebSocket, authenticates the
//! client with a per-launch bearer token (checked at the HTTP-upgrade stage,
//! before any frames flow), then:
//!   * pushes binary telemetry frames (FFT/SYS) and JSON agent events outward;
//!   * forwards inbound control messages (interrupt/mute/confirm) to core.
//!
//! Telemetry uses a broadcast channel: under backpressure the oldest frames are
//! dropped ("state, not history", §6.1). The server is loopback-only by
//! default — the HUD runs on the same machine or tunnels in.

use futures_util::{SinkExt, StreamExt};
use oracle_ipc::{HudCommand, HudEvent};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

/// A frame queued for the HUD.
#[derive(Debug, Clone)]
pub enum OutFrame {
    /// Binary telemetry (already-encoded FFT/SYS frame).
    Binary(Vec<u8>),
    /// A JSON agent event.
    Event(HudEvent),
}

/// Handle used by core to publish to the HUD and receive its commands.
pub struct HudGateway {
    out_tx: broadcast::Sender<OutFrame>,
    cmd_rx: mpsc::Receiver<HudCommand>,
    cmd_tx: mpsc::Sender<HudCommand>,
    token: String,
}

impl HudGateway {
    /// Create a gateway with a bearer token. Buffer sizes chosen so normal
    /// operation never drops; only pathological lag sheds telemetry.
    pub fn new(token: impl Into<String>) -> Self {
        let (out_tx, _) = broadcast::channel(256);
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        HudGateway {
            out_tx,
            cmd_rx,
            cmd_tx,
            token: token.into(),
        }
    }

    /// Publish a binary telemetry frame (non-blocking; dropped if no clients).
    pub fn send_binary(&self, bytes: Vec<u8>) {
        let _ = self.out_tx.send(OutFrame::Binary(bytes));
    }

    /// Publish a JSON agent event.
    pub fn send_event(&self, ev: HudEvent) {
        let _ = self.out_tx.send(OutFrame::Event(ev));
    }

    /// Receive the next inbound HUD command (interrupt/mute/confirm).
    pub async fn next_command(&mut self) -> Option<HudCommand> {
        self.cmd_rx.recv().await
    }

    /// A cloneable publisher handle for use inside the agent loop.
    pub fn publisher(&self) -> HudPublisher {
        HudPublisher {
            out_tx: self.out_tx.clone(),
        }
    }

    /// A cloneable inbound-command sender, so background producers (e.g. the
    /// wake-word listener) can inject commands into the same loop that services
    /// the HUD — a heard "Delphi, …" becomes an ordinary `UserText` turn.
    pub fn command_sender(&self) -> mpsc::Sender<HudCommand> {
        self.cmd_tx.clone()
    }

    /// Spawn the accept loop. Returns the bound address (useful when binding to
    /// port 0 in tests). The server runs until the returned task is dropped or
    /// the process exits.
    pub async fn serve(
        &self,
        bind_addr: &str,
    ) -> std::io::Result<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
        let listener = TcpListener::bind(bind_addr).await?;
        let addr = listener.local_addr()?;
        let out_tx = self.out_tx.clone();
        let cmd_tx = self.cmd_tx.clone();
        let token = Arc::new(self.token.clone());
        info!(%addr, "HUD gateway listening");

        let handle = tokio::spawn(async move {
            loop {
                let (stream, peer) = match listener.accept().await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("hud accept failed: {e}");
                        continue;
                    }
                };
                let out_rx = out_tx.subscribe();
                let cmd_tx = cmd_tx.clone();
                let token = token.clone();
                tokio::spawn(async move {
                    // Peek (without consuming) to route the connection: a
                    // WebSocket upgrade drives the live HUD gateway; any other
                    // GET is a request for a static HUD asset that core serves
                    // itself — so there's no separate web server or Tauri shell.
                    let mut probe = [0u8; 1024];
                    let n = stream.peek(&mut probe).await.unwrap_or(0);
                    // A connection that sends nothing is a liveness probe (e.g.
                    // core's single-instance check) — close it quietly.
                    if n == 0 {
                        return;
                    }
                    let head = String::from_utf8_lossy(&probe[..n]);
                    if is_websocket_upgrade(&head) {
                        if let Err(e) = handle_client(stream, out_rx, cmd_tx, token).await {
                            warn!(%peer, "hud client ended: {e}");
                        }
                    } else if let Err(e) = serve_static(stream, &head).await {
                        warn!(%peer, "hud static serve failed: {e}");
                    }
                });
            }
        });
        Ok((addr, handle))
    }
}

/// Cloneable publish-only handle.
#[derive(Clone)]
pub struct HudPublisher {
    out_tx: broadcast::Sender<OutFrame>,
}

impl HudPublisher {
    pub fn send_binary(&self, bytes: Vec<u8>) {
        let _ = self.out_tx.send(OutFrame::Binary(bytes));
    }
    pub fn send_event(&self, ev: HudEvent) {
        let _ = self.out_tx.send(OutFrame::Event(ev));
    }
}

// The tungstenite callback signature returns Result<_, ErrorResponse>, whose
// Err variant is inherently large; that's the library's contract, not ours.
#[allow(clippy::result_large_err)]
async fn handle_client(
    stream: tokio::net::TcpStream,
    mut out_rx: broadcast::Receiver<OutFrame>,
    cmd_tx: mpsc::Sender<HudCommand>,
    token: Arc<String>,
) -> anyhow::Result<()> {
    // Authenticate at the HTTP-upgrade stage. If the configured token is empty,
    // auth is disabled — the loopback bind is the security boundary for local
    // use (set a token only when exposing the gateway beyond localhost). If a
    // token IS set, the client must present a matching `?token=...`.
    //
    // On rejection we MUST return a non-2xx status: tokio-tungstenite refuses an
    // Err response whose status is "successful" ("Custom response must not be
    // successful"), which otherwise surfaces as a confusing protocol error
    // instead of a clean 403.
    let expected = token.clone();
    let ws = tokio_tungstenite::accept_hdr_async(stream, move |req: &Request, resp: Response| {
        let authorized =
            expected.is_empty() || request_token(req).as_deref() == Some(expected.as_str());
        if authorized {
            Ok(resp)
        } else {
            let mut err = ErrorResponse::new(Some("invalid or missing token".to_string()));
            *err.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::FORBIDDEN;
            Err(err)
        }
    })
    .await?;

    let (mut write, mut read) = ws.split();

    // Greet the freshly-connected client with the current state so the HUD
    // immediately shows a live link instead of "awaiting telemetry". Broadcast
    // events emitted before this client subscribed were never delivered to it.
    if let Ok(json) = serde_json::to_string(&HudEvent::State {
        turn: uuid::Uuid::nil(),
        state: "idle".to_string(),
    }) {
        let _ = write.send(Message::Text(json)).await;
    }

    loop {
        tokio::select! {
            // Outbound: telemetry + events → client.
            frame = out_rx.recv() => {
                match frame {
                    Ok(OutFrame::Binary(b)) => write.send(Message::Binary(b)).await?,
                    Ok(OutFrame::Event(ev)) => {
                        let json = serde_json::to_string(&ev)?;
                        write.send(Message::Text(json)).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Shed stale telemetry — expected under load.
                        tracing::debug!("hud client lagged, dropped {n} frames");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // Inbound: control messages ← client.
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(cmd) = serde_json::from_str::<HudCommand>(&t) {
                            let _ = cmd_tx.send(cmd).await;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {} // ignore ping/pong/binary from client
                    Some(Err(e)) => return Err(e.into()),
                }
            }
        }
    }
    Ok(())
}

/// Extract the `token` query parameter from the upgrade request URI.
fn request_token(req: &Request) -> Option<String> {
    let uri = req.uri();
    let query = uri.query()?;
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("token=") {
            return Some(v.to_string());
        }
    }
    None
}

// --- Static HUD serving ---------------------------------------------------
//
// The built HUD (oracle-hud/dist) is embedded into the binary, so `oracle-core`
// serves its own frontend on the same loopback port as the WebSocket gateway.
// Open http://127.0.0.1:8770/ and the whole Oracle face loads — no vite, no
// static file server, no desktop shell required.

#[derive(rust_embed::RustEmbed)]
#[folder = "../oracle-hud/dist"]
struct HudAssets;

/// Does this peeked HTTP request head look like a WebSocket upgrade?
fn is_websocket_upgrade(head: &str) -> bool {
    let lower = head.to_ascii_lowercase();
    lower.contains("upgrade: websocket") || lower.contains("upgrade:websocket")
}

/// Serve a single static asset from the embedded HUD and close the connection.
/// `head` is the peeked request head; we only need its request-line path.
async fn serve_static(mut stream: TcpStream, head: &str) -> std::io::Result<()> {
    // Drain the request bytes off the socket first. We only *peeked* them to
    // route the connection, so they're still in the receive buffer. Closing a
    // TCP socket that still holds unread data makes the OS send RST instead of
    // FIN, which truncates the response we just wrote — that's why large assets
    // (the ~500KB JS bundle) arrived cut off. Reading the request drains the
    // buffer so the close is graceful and the whole body is delivered.
    consume_request_head(&mut stream).await;

    let path = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    // Strip query/fragment, then map "/" to index.html.
    let clean = path.split(['?', '#']).next().unwrap_or("/");
    let rel = clean.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };

    let (status, ctype, body): (&str, &str, Vec<u8>) = match HudAssets::get(rel) {
        Some(file) => ("200 OK", content_type(rel), file.data.into_owned()),
        None => {
            // Deep-link fallback: paths without an extension get index.html so
            // the single-page HUD still boots. A missing asset is a real 404.
            if !rel.contains('.') {
                if let Some(index) = HudAssets::get("index.html") {
                    (
                        "200 OK",
                        "text/html; charset=utf-8",
                        index.data.into_owned(),
                    )
                } else {
                    (
                        "404 Not Found",
                        "text/html; charset=utf-8",
                        not_built_page(),
                    )
                }
            } else if HudAssets::get("index.html").is_none() {
                // The HUD was never built into this binary.
                ("200 OK", "text/html; charset=utf-8", not_built_page())
            } else {
                (
                    "404 Not Found",
                    "text/html; charset=utf-8",
                    b"<h1>404</h1>".to_vec(),
                )
            }
        }
    };

    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    // Graceful half-close: flush the write side and send FIN, so the client
    // receives the complete body before the socket is dropped.
    let _ = stream.shutdown().await;
    Ok(())
}

/// Read and discard the HTTP request head (up to the blank line) so the socket's
/// receive buffer is empty before we close — preventing an RST that would
/// truncate the response. GET requests have no body, so header-end is request-end.
async fn consume_request_head(stream: &mut TcpStream) {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 2048];
    let mut seen = 0usize;
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break, // peer closed
            Ok(n) => {
                seen += n;
                // Once we've read past the header terminator, or a sane cap, stop.
                if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") || seen > 16_384 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// Minimal MIME mapping for the asset types the HUD ships.
fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        _ => "application/octet-stream",
    }
}

/// Shown when the binary was built without a HUD (oracle-hud/dist was empty).
fn not_built_page() -> Vec<u8> {
    br#"<!doctype html><html><body style="font-family:sans-serif;background:#04070d;color:#cfe9ff;display:flex;height:100vh;align-items:center;justify-content:center;text-align:center">
<div><h2>Oracle of Delphi</h2><p>The HUD wasn't built into this binary.<br>Run <code>npm --prefix oracle-hud run build</code>, then rebuild oracle-core.</p></div>
</body></html>"#.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oracle_ipc::audio::{encode_fft, FFT_BANDS};
    use tokio_tungstenite::tungstenite::Message as CMsg;

    #[tokio::test]
    async fn serves_the_embedded_hud_over_http() {
        let gw = HudGateway::new("");
        let (addr, _h) = gw.serve("127.0.0.1:0").await.unwrap();
        // A plain browser GET on the same port must return the HUD, not a WS
        // protocol error — this is what lets core serve its own frontend.
        let body = reqwest::get(format!("http://{addr}/"))
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap();
        // The HUD shell always contains the scene canvas element.
        assert!(
            body.contains("id=\"scene\"") || body.contains("Oracle"),
            "unexpected body: {}",
            &body[..body.len().min(200)]
        );

        // Regression: the JS bundle must arrive in FULL, not truncated. A close
        // with unread request bytes used to RST the socket and cut large assets
        // off (~192KB of a 497KB bundle), leaving the browser with a broken
        // script and a blank page.
        if let Some(rest) = body.split("/assets/").nth(1) {
            let file = rest.split(['"', '\'']).next().unwrap_or("");
            let asset = format!("/assets/{file}");
            let want = HudAssets::get(asset.trim_start_matches('/'))
                .expect("asset must be embedded")
                .data
                .len();
            let got = reqwest::get(format!("http://{addr}{asset}"))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            assert_eq!(
                got.len(),
                want,
                "asset {asset} truncated: got {} of {want} bytes",
                got.len()
            );
        }
    }

    #[test]
    fn routing_distinguishes_upgrade_from_plain_get() {
        assert!(is_websocket_upgrade(
            "GET /hud HTTP/1.1\r\nUpgrade: websocket\r\n"
        ));
        assert!(is_websocket_upgrade(
            "GET /hud HTTP/1.1\r\nupgrade: WebSocket\r\n"
        ));
        assert!(!is_websocket_upgrade("GET / HTTP/1.1\r\nHost: x\r\n"));
    }

    #[tokio::test]
    async fn client_authenticates_and_receives_frames() {
        let gw = HudGateway::new("secret-token");
        let pubh = gw.publisher();
        let (addr, _h) = gw.serve("127.0.0.1:0").await.unwrap();

        // Connect a real WS client with the correct token.
        let url = format!("ws://{addr}/hud?token=secret-token");
        let (mut ws, _resp) = tokio_tungstenite::connect_async(url).await.unwrap();

        // Publish a telemetry frame and an event after the client is up.
        // Small delay so the subscription is registered.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let bands = [0.5f32; FFT_BANDS];
        pubh.send_binary(encode_fft(&bands, 1, 1000, true));
        pubh.send_event(HudEvent::Caption {
            text: "hello".into(),
        });

        // Expect a binary frame then a text event.
        let mut saw_binary = false;
        let mut saw_text = false;
        for _ in 0..4 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await {
                Ok(Some(Ok(CMsg::Binary(b)))) => {
                    assert_eq!(b.len(), 8 + FFT_BANDS * 4);
                    saw_binary = true;
                }
                // The first text frame is the on-connect state greeting; only
                // the caption we published contains "hello".
                Ok(Some(Ok(CMsg::Text(t)))) if t.contains("hello") => {
                    saw_text = true;
                }
                _ => {}
            }
            if saw_binary && saw_text {
                break;
            }
        }
        assert!(saw_binary, "should receive a binary FFT frame");
        assert!(saw_text, "should receive a JSON event");
    }

    #[tokio::test]
    async fn wrong_token_is_rejected() {
        let gw = HudGateway::new("right");
        let (addr, _h) = gw.serve("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{addr}/hud?token=wrong");
        let res = tokio_tungstenite::connect_async(url).await;
        assert!(res.is_err(), "handshake must fail with a bad token");
    }

    #[tokio::test]
    async fn empty_token_disables_auth() {
        // An empty configured token means no auth: the HUD connects to
        // ws://<bind>/hud with no token and the handshake succeeds. This is the
        // local-dev default (loopback bind is the boundary).
        let gw = HudGateway::new("");
        let pubh = gw.publisher();
        let (addr, _h) = gw.serve("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{addr}/hud"); // no ?token
        let (mut ws, _resp) = tokio_tungstenite::connect_async(url)
            .await
            .expect("no-token handshake must succeed when auth is disabled");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        pubh.send_event(HudEvent::Caption {
            text: "connected".into(),
        });
        // We should receive the event, proving the socket is live end-to-end.
        let mut got = false;
        for _ in 0..4 {
            if let Ok(Some(Ok(CMsg::Text(t)))) =
                tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await
            {
                if t.contains("connected") {
                    got = true;
                    break;
                }
            }
        }
        assert!(got, "should receive events over the no-auth socket");
    }

    #[tokio::test]
    async fn inbound_interrupt_reaches_core() {
        let mut gw = HudGateway::new("t");
        let (addr, _h) = gw.serve("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{addr}/hud?token=t");
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        ws.send(CMsg::Text("{\"type\":\"interrupt\"}".into()))
            .await
            .unwrap();

        let cmd = tokio::time::timeout(std::time::Duration::from_secs(2), gw.next_command())
            .await
            .unwrap();
        assert!(matches!(cmd, Some(HudCommand::Interrupt)));
    }
}
