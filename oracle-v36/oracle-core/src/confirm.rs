//! Interactive confirmation for irreversible actions (architecture §3.4).
//!
//! When a tool hits the daemon's `needs_confirmation`, it asks a [`Confirmer`]
//! for the user's decree before sending the `Confirm` RPC that actually
//! executes. Three implementations:
//!   * [`DenyConfirmer`] — the safe default; refuses everything (used when no
//!     interactive channel exists, e.g. a headless run).
//!   * [`StdinConfirmer`] — a y/N prompt on the terminal (the REPL).
//!   * [`HudConfirmer`] — raises the Apollo confirmation modal in the HUD and
//!     awaits the user's Sanction/Forbid over the WebSocket.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::oneshot;
use uuid::Uuid;

/// A decision source for irreversible actions.
#[async_trait]
pub trait Confirmer: Send + Sync {
    /// Ask the user to sanction `prompt`. Returns true = proceed.
    async fn request(&self, prompt: &str, severity: &str) -> bool;
}

/// Safe default: refuse everything. Nothing irreversible happens unless an
/// interactive confirmer is wired.
pub struct DenyConfirmer;

#[async_trait]
impl Confirmer for DenyConfirmer {
    async fn request(&self, _prompt: &str, _severity: &str) -> bool {
        false
    }
}

/// Terminal confirmer for the REPL. Reads a line from stdin. Safe because the
/// REPL's main loop is blocked awaiting the turn while a tool prompts, so stdin
/// is free.
pub struct StdinConfirmer;

#[async_trait]
impl Confirmer for StdinConfirmer {
    async fn request(&self, prompt: &str, severity: &str) -> bool {
        use std::io::{stdin, stdout, Write};
        // Run the blocking read on a blocking thread so we don't stall the runtime.
        let prompt = prompt.to_string();
        let severity = severity.to_string();
        tokio::task::spawn_blocking(move || {
            eprintln!("\n  ⚠ [{severity}] {prompt}");
            eprint!("  Sanction this action? [y/N] ");
            let _ = stdout().flush();
            let mut line = String::new();
            if stdin().read_line(&mut line).is_err() {
                return false;
            }
            matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
        })
        .await
        .unwrap_or(false)
    }
}

/// HUD confirmer: raises the confirmation modal and awaits the reply. Holds a
/// map of pending decisions keyed by request id; the run loop calls [`resolve`]
/// when a `HudCommand::Confirm` arrives.
pub struct HudConfirmer {
    publisher: crate::gateway::server::HudPublisher,
    pending: Mutex<HashMap<Uuid, oneshot::Sender<bool>>>,
    timeout: Duration,
}

impl HudConfirmer {
    pub fn new(publisher: crate::gateway::server::HudPublisher) -> Self {
        HudConfirmer {
            publisher,
            pending: Mutex::new(HashMap::new()),
            // If the user never answers, the decree lapses (deny) after this.
            timeout: Duration::from_secs(120),
        }
    }

    /// Resolve a pending confirmation from the HUD's reply.
    pub fn resolve(&self, request_id: Uuid, allow: bool) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&request_id) {
            let _ = tx.send(allow);
        }
    }
}

#[async_trait]
impl Confirmer for HudConfirmer {
    async fn request(&self, prompt: &str, severity: &str) -> bool {
        let id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.publisher.send_event(oracle_ipc::HudEvent::Confirm {
            request_id: id,
            prompt: prompt.to_string(),
            severity: severity.to_string(),
        });
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(allow)) => allow,
            _ => {
                // Timed out or channel dropped → lapse to deny; clean up.
                self.pending.lock().unwrap().remove(&id);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deny_confirmer_always_refuses() {
        let c = DenyConfirmer;
        assert!(!c.request("kill firefox", "irreversible").await);
    }

    #[tokio::test]
    async fn hud_confirmer_resolves_with_the_reply() {
        let gw = crate::gateway::server::HudGateway::new("");
        let confirmer = std::sync::Arc::new(HudConfirmer::new(gw.publisher()));

        // Kick off the request on a task; it parks a pending entry and emits a
        // Confirm event to the HUD.
        let c2 = confirmer.clone();
        let handle =
            tokio::spawn(async move { c2.request("terminate pid 5", "irreversible").await });

        // Grab the parked request id and resolve it as the HUD would.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let id = *confirmer.pending.lock().unwrap().keys().next().unwrap();
        confirmer.resolve(id, true);

        let allowed = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
        assert!(allowed, "resolve(true) must let the action proceed");
    }

    #[tokio::test]
    async fn hud_confirmer_denies_on_resolve_false() {
        let gw = crate::gateway::server::HudGateway::new("");
        let confirmer = std::sync::Arc::new(HudConfirmer::new(gw.publisher()));
        let c2 = confirmer.clone();
        let handle = tokio::spawn(async move { c2.request("rm -rf build", "irreversible").await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let id = *confirmer.pending.lock().unwrap().keys().next().unwrap();
        confirmer.resolve(id, false);
        let allowed = handle.await.unwrap();
        assert!(!allowed);
    }
}
