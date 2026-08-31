//! Idle tracking and on-demand LLM loading.
//!
//! A 14B model at Q4 holds 10-12 GB of VRAM for as long as it is loaded, and on
//! a desktop assistant that is overwhelmingly time spent idle. The wake-word
//! path does not need the LLM at all — it is a separate small recognizer — so
//! there is no reason for the model to be resident between conversations.
//!
//! This module tracks when Pythia was last used and, past a threshold, pauses
//! the supervised `llama-server` child. The next turn resumes it and waits for
//! the server to report healthy before the request goes out.
//!
//! The trade is a few seconds on the first turn after a lull, in exchange for
//! the GPU being free the rest of the time. For something you talk to a dozen
//! times a day, that is the right side of the trade.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use crate::supervisor::ChildHandle;

/// When Pythia was last used, as unix seconds.
///
/// Time is passed in rather than read from a clock so the policy is a pure
/// function of its inputs and can be tested without sleeping.
pub struct IdleTracker {
    last_active: AtomicI64,
    idle_after_secs: i64,
}

impl IdleTracker {
    pub fn new(now: i64, idle_after_secs: i64) -> Self {
        IdleTracker {
            last_active: AtomicI64::new(now),
            idle_after_secs,
        }
    }

    /// Mark activity. Called on every turn and every wake word.
    pub fn touch(&self, now: i64) {
        // Monotonic: a late-arriving older timestamp must not drag the mark
        // backwards and make an active session look idle.
        self.last_active.fetch_max(now, Ordering::SeqCst);
    }

    pub fn idle_secs(&self, now: i64) -> i64 {
        (now - self.last_active.load(Ordering::SeqCst)).max(0)
    }

    /// Whether the model should be unloaded now.
    ///
    /// A non-positive threshold disables unloading entirely, which is what
    /// `idle_unload_secs = 0` means in the config.
    pub fn is_idle(&self, now: i64) -> bool {
        self.idle_after_secs > 0 && self.idle_secs(now) >= self.idle_after_secs
    }
}

/// Owns the LLM child handle and the readiness probe.
pub struct LlmLifecycle {
    handle: Option<ChildHandle>,
    /// `{backend}/health` — llama-server answers 503 while loading, 200 when it
    /// can serve. None when the backend is the mock, or not an http URL.
    health_url: Option<String>,
    /// How long to wait for the server after a resume before giving up.
    ready_timeout: Duration,
    http: reqwest::Client,
}

impl LlmLifecycle {
    pub fn new(handle: Option<ChildHandle>, backend: &str, ready_timeout: Duration) -> Self {
        LlmLifecycle {
            handle,
            health_url: health_url_for(backend),
            ready_timeout,
            http: reqwest::Client::new(),
        }
    }

    /// True when there is nothing to manage (mock backend, or autostart off).
    pub fn is_inert(&self) -> bool {
        self.handle.is_none()
    }

    /// Pause the model. Returns true if this actually unloaded it.
    pub fn unload(&self) -> bool {
        self.handle.as_ref().map(|h| h.stop()).unwrap_or(false)
    }

    /// Ensure the model is up and answering before a turn goes out.
    ///
    /// Returns false only if it was resumed and never became healthy inside
    /// `ready_timeout`; the caller should surface that rather than let the turn
    /// fail with a bare connection error.
    pub async fn ensure_ready(&self) -> bool {
        let Some(handle) = self.handle.as_ref() else {
            return true; // nothing supervised; caller's own retry applies
        };
        let was_down = handle.start();
        if !was_down {
            return true; // already up
        }
        tracing::info!("[idle] reloading the LLM for a new turn");

        let Some(url) = self.health_url.as_ref() else {
            // No probe available. Give it a moment so the first request is not
            // guaranteed to hit a closed port.
            tokio::time::sleep(Duration::from_secs(2)).await;
            return true;
        };

        let deadline = std::time::Instant::now() + self.ready_timeout;
        while std::time::Instant::now() < deadline {
            if let Ok(resp) = self.http.get(url).send().await {
                if resp.status().is_success() {
                    tracing::info!("[idle] LLM ready");
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        tracing::warn!(%url, "[idle] LLM did not become healthy in time");
        false
    }
}

/// Derive llama-server's health endpoint from the configured backend.
///
/// Returns None for the mock backend and for anything that is not http(s), so
/// a non-server backend is never probed.
pub fn health_url_for(backend: &str) -> Option<String> {
    let b = backend.trim().trim_end_matches('/');
    if !(b.starts_with("http://") || b.starts_with("https://")) {
        return None;
    }
    Some(format!("{b}/health"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_tracker_is_not_idle() {
        let t = IdleTracker::new(1_000, 300);
        assert!(!t.is_idle(1_000));
        assert_eq!(t.idle_secs(1_000), 0);
    }

    #[test]
    fn idleness_is_reached_at_the_threshold() {
        let t = IdleTracker::new(1_000, 300);
        assert!(!t.is_idle(1_299));
        assert!(t.is_idle(1_300), "threshold is inclusive");
        assert!(t.is_idle(9_999));
    }

    #[test]
    fn touching_resets_the_clock() {
        let t = IdleTracker::new(1_000, 300);
        assert!(t.is_idle(1_400));
        t.touch(1_400);
        assert!(!t.is_idle(1_400));
        assert!(!t.is_idle(1_600));
        assert!(t.is_idle(1_700));
    }

    #[test]
    fn a_stale_touch_cannot_drag_the_mark_backwards() {
        // Two turns can finish out of order; the older one must not make an
        // active session look idle and unload the model mid-conversation.
        let t = IdleTracker::new(1_000, 300);
        t.touch(2_000);
        t.touch(1_500);
        assert_eq!(t.idle_secs(2_000), 0);
    }

    #[test]
    fn a_zero_threshold_disables_unloading() {
        let t = IdleTracker::new(0, 0);
        assert!(!t.is_idle(1_000_000));
        let neg = IdleTracker::new(0, -1);
        assert!(!neg.is_idle(1_000_000));
    }

    #[test]
    fn clock_skew_does_not_produce_negative_idleness() {
        let t = IdleTracker::new(1_000, 300);
        assert_eq!(t.idle_secs(500), 0);
    }

    #[test]
    fn health_url_is_derived_only_for_http_backends() {
        assert_eq!(
            health_url_for("http://127.0.0.1:8080"),
            Some("http://127.0.0.1:8080/health".into())
        );
        // A trailing slash must not produce a double slash.
        assert_eq!(
            health_url_for("http://127.0.0.1:8080/"),
            Some("http://127.0.0.1:8080/health".into())
        );
        assert_eq!(
            health_url_for("https://box.local:9999"),
            Some("https://box.local:9999/health".into())
        );
        // The mock backend has no server to probe.
        assert_eq!(health_url_for("mock"), None);
        assert_eq!(health_url_for(""), None);
    }
}
