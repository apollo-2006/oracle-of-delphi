//! Process lifecycle: graceful shutdown, session persistence, reconnection
//! (architecture §7).
//!
//! A [`ShutdownController`] broadcasts a single "time to stop" signal to every
//! subsystem (audio pumps, gateway, actd client, sync jobs) so the process
//! drains in order instead of being killed mid-write. Session state is snapshot
//! to disk on graceful exit and restored on boot, giving warm restarts.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tokio::sync::watch;

/// Broadcasts shutdown to all subsystems and lets the main task await drain.
#[derive(Clone)]
pub struct ShutdownController {
    tx: watch::Sender<bool>,
}

pub struct ShutdownListener {
    rx: watch::Receiver<bool>,
}

impl ShutdownController {
    pub fn new() -> (Self, ShutdownListener) {
        let (tx, rx) = watch::channel(false);
        (ShutdownController { tx }, ShutdownListener { rx })
    }

    /// Additional listeners for other subsystems.
    pub fn listener(&self) -> ShutdownListener {
        ShutdownListener {
            rx: self.tx.subscribe(),
        }
    }

    /// Signal all listeners to stop.
    pub fn trigger(&self) {
        let _ = self.tx.send(true);
    }

    pub fn is_shutting_down(&self) -> bool {
        *self.tx.borrow()
    }
}

impl Default for ShutdownController {
    fn default() -> Self {
        Self::new().0
    }
}

impl ShutdownListener {
    /// Resolves once shutdown is triggered. Cheap to hold in a `select!`.
    pub async fn wait(&mut self) {
        // If already shutting down, return immediately.
        if *self.rx.borrow() {
            return;
        }
        while self.rx.changed().await.is_ok() {
            if *self.rx.borrow() {
                return;
            }
        }
    }

    pub fn is_shutting_down(&self) -> bool {
        *self.rx.borrow()
    }
}

/// Install OS signal handlers (SIGINT/SIGTERM on unix, Ctrl-C on windows) that
/// trigger the controller. Returns a task handle.
pub fn install_signal_handlers(controller: ShutdownController) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        tracing::info!("shutdown signal received; draining");
        controller.trigger();
    })
}

/// Persisted session state for warm restarts (architecture §5.1: KV snapshot +
/// session journal). We persist the conversation summary and turn count; the
/// LLM KV cache itself is snapshotted separately by the backend.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SessionSnapshot {
    pub turn_count: u64,
    pub rolling_summary: String,
    pub last_saved_unix: i64,
}

impl SessionSnapshot {
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(self)?;
        // Atomic write: temp + rename, so a crash mid-write can't corrupt it.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Option<SessionSnapshot> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

/// Exponential backoff helper for reconnection loops (actd, HA, MQTT, gateway).
pub struct Backoff {
    current: Duration,
    max: Duration,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration) -> Self {
        Backoff {
            current: initial,
            max,
        }
    }

    /// Next delay, doubling up to the cap.
    pub fn next_delay(&mut self) -> Duration {
        let d = self.current;
        self.current = (self.current * 2).min(self.max);
        d
    }

    pub fn reset(&mut self, initial: Duration) {
        self.current = initial;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_broadcasts_to_all_listeners() {
        let (ctrl, mut l1) = ShutdownController::new();
        let mut l2 = ctrl.listener();
        assert!(!l1.is_shutting_down());
        ctrl.trigger();
        // Both listeners observe it.
        tokio::time::timeout(Duration::from_secs(1), l1.wait())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), l2.wait())
            .await
            .unwrap();
        assert!(l1.is_shutting_down());
        assert!(l2.is_shutting_down());
    }

    #[tokio::test]
    async fn wait_returns_immediately_if_already_down() {
        let (ctrl, mut l) = ShutdownController::new();
        ctrl.trigger();
        // Should not hang.
        tokio::time::timeout(Duration::from_millis(100), l.wait())
            .await
            .unwrap();
    }

    #[test]
    fn session_snapshot_roundtrips_atomically() {
        let dir = std::env::temp_dir().join(format!("oracle-sess-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        let snap = SessionSnapshot {
            turn_count: 42,
            rolling_summary: "discussed thesis scheduling".into(),
            last_saved_unix: 1000,
        };
        snap.save(&path).unwrap();
        let loaded = SessionSnapshot::load(&path).unwrap();
        assert_eq!(loaded, snap);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_snapshot_is_none() {
        assert!(SessionSnapshot::load(Path::new("/nonexistent/session.json")).is_none());
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_secs(2));
        assert_eq!(b.next_delay(), Duration::from_millis(100));
        assert_eq!(b.next_delay(), Duration::from_millis(200));
        assert_eq!(b.next_delay(), Duration::from_millis(400));
        // ... caps at 2s
        for _ in 0..10 {
            b.next_delay();
        }
        assert_eq!(b.next_delay(), Duration::from_secs(2));
        b.reset(Duration::from_millis(50));
        assert_eq!(b.next_delay(), Duration::from_millis(50));
    }
}
