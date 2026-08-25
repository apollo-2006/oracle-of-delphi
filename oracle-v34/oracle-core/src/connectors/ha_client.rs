//! Live Home Assistant WebSocket client (architecture §4.4).
//!
//! Implements the HA WS protocol: `auth_required` → `auth` → `auth_ok`, then
//! `subscribe_events(state_changed)` and `call_service`. Inbound `state_changed`
//! events feed the local [`EntityMirror`] so reads are instant. Reconnects with
//! backoff on drop. Tested against a mock HA server that speaks the same
//! handshake — no real Home Assistant instance required.

use super::homeassistant::{
    auth_message, call_service, light_set, subscribe_state_changes, EntityMirror,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

/// A connected HA client. Holds the entity mirror and a command sender.
pub struct HaClient {
    pub mirror: Arc<EntityMirror>,
    cmd_tx: tokio::sync::mpsc::Sender<Value>,
    id: Arc<AtomicU64>,
}

impl HaClient {
    /// Connect, authenticate, subscribe, and spawn the read/write pumps.
    /// `url` like `ws://homeassistant.local:8123/api/websocket`.
    pub async fn connect(url: &str, access_token: &str) -> anyhow::Result<Self> {
        let mirror = Arc::new(EntityMirror::new());
        let id = Arc::new(AtomicU64::new(1));
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<Value>(64);

        let (ws, _) = tokio_tungstenite::connect_async(url).await?;
        let (mut write, mut read) = ws.split();

        // --- HA auth handshake ---
        // Server sends {"type":"auth_required"} first.
        let first = read
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("HA closed before auth"))??;
        let msg: Value = serde_json::from_str(first.to_text()?)?;
        if msg["type"] != "auth_required" {
            anyhow::bail!("unexpected first HA message: {}", msg["type"]);
        }
        write
            .send(Message::Text(auth_message(access_token).to_string()))
            .await?;
        let auth_resp = read
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("HA closed during auth"))??;
        let auth_val: Value = serde_json::from_str(auth_resp.to_text()?)?;
        if auth_val["type"] != "auth_ok" {
            anyhow::bail!("HA auth failed: {}", auth_val["type"]);
        }
        info!("authenticated to Home Assistant");

        // Subscribe to state_changed.
        let sub_id = id.fetch_add(1, Ordering::SeqCst);
        write
            .send(Message::Text(subscribe_state_changes(sub_id).to_string()))
            .await?;

        // Read pump: apply state_changed into the mirror.
        let mirror_read = mirror.clone();
        tokio::spawn(async move {
            while let Some(item) = read.next().await {
                match item {
                    Ok(Message::Text(t)) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            if let Some((entity, state)) = mirror_read.apply(&v) {
                                tracing::debug!(%entity, %state, "HA state update");
                            }
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
            warn!("HA read pump ended (connection closed)");
        });

        // Write pump: forward queued commands.
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                if write.send(Message::Text(cmd.to_string())).await.is_err() {
                    break;
                }
            }
        });

        Ok(HaClient { mirror, cmd_tx, id })
    }

    fn next_id(&self) -> u64 {
        self.id.fetch_add(1, Ordering::SeqCst)
    }

    /// Call an arbitrary service.
    pub async fn call_service(
        &self,
        domain: &str,
        service: &str,
        entity_id: &str,
        data: Value,
    ) -> anyhow::Result<()> {
        let msg = call_service(self.next_id(), domain, service, entity_id, data);
        self.cmd_tx.send(msg).await?;
        Ok(())
    }

    /// Set a light's brightness (0-100%).
    pub async fn set_light(&self, entity_id: &str, brightness_pct: u8) -> anyhow::Result<()> {
        let msg = light_set(self.next_id(), entity_id, brightness_pct);
        self.cmd_tx.send(msg).await?;
        Ok(())
    }

    /// Instant local read from the mirror.
    pub fn state_of(&self, entity_id: &str) -> Option<String> {
        self.mirror.get(entity_id)
    }
}

/// Connect with retry + exponential backoff. Returns once connected, or after
/// `max_attempts` failures.
pub async fn connect_with_backoff(
    url: &str,
    token: &str,
    max_attempts: u32,
) -> anyhow::Result<HaClient> {
    let mut delay = Duration::from_millis(500);
    let mut last_err = None;
    for attempt in 1..=max_attempts {
        match HaClient::connect(url, token).await {
            Ok(c) => return Ok(c),
            Err(e) => {
                warn!(attempt, "HA connect failed: {e}");
                last_err = Some(e);
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("HA connect: no attempts")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// A mock HA WS server that performs the auth handshake, accepts the
    /// subscription, then pushes a state_changed event and echoes commands.
    async fn mock_ha_server() -> (String, tokio::task::JoinHandle<Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/api/websocket");
        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            // auth_required → wait for auth → auth_ok
            ws.send(Message::Text(r#"{"type":"auth_required"}"#.into()))
                .await
                .unwrap();
            let auth = ws.next().await.unwrap().unwrap();
            let auth_val: Value = serde_json::from_str(auth.to_text().unwrap()).unwrap();
            assert_eq!(auth_val["type"], "auth");
            ws.send(Message::Text(r#"{"type":"auth_ok"}"#.into()))
                .await
                .unwrap();

            // Expect the subscribe message.
            let sub = ws.next().await.unwrap().unwrap();
            let sub_val: Value = serde_json::from_str(sub.to_text().unwrap()).unwrap();
            assert_eq!(sub_val["type"], "subscribe_events");

            // Push a state_changed event for light.bedroom → "on".
            let event = serde_json::json!({
                "type": "event",
                "event": {
                    "event_type": "state_changed",
                    "data": {
                        "entity_id": "light.bedroom",
                        "new_state": {"state": "on"}
                    }
                }
            });
            ws.send(Message::Text(event.to_string())).await.unwrap();

            // Capture the next command the client sends (a set_light).
            let cmd = ws.next().await.unwrap().unwrap();
            serde_json::from_str::<Value>(cmd.to_text().unwrap()).unwrap()
        });
        (url, handle)
    }

    #[tokio::test]
    async fn full_ha_handshake_subscribe_and_command() {
        let (url, server) = mock_ha_server().await;
        let client = HaClient::connect(&url, "long-lived-token").await.unwrap();

        // Give the pushed state_changed a moment to land in the mirror.
        for _ in 0..50 {
            if client.state_of("light.bedroom").is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(client.state_of("light.bedroom").as_deref(), Some("on"));

        // Send a command; the mock captures it and returns it.
        client.set_light("light.bedroom", 30).await.unwrap();
        let captured = server.await.unwrap();
        assert_eq!(captured["type"], "call_service");
        assert_eq!(captured["domain"], "light");
        assert_eq!(captured["service"], "turn_on");
        assert_eq!(captured["service_data"]["brightness_pct"], 30);
    }
}
