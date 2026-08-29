//! Personal workspace + IoT integration (architecture §4).
//!
//! This module holds the *real* infrastructure: an AES-GCM token vault, the
//! OAuth2 + PKCE helpers, token-lifecycle logic, and message builders/parsers
//! for the Home Assistant WebSocket API and MQTT. The network round-trips
//! themselves are thin; the parts worth testing (and the parts that are easy to
//! get subtly wrong) are the crypto, the PKCE derivation, the refresh-timing
//! decision, and the protocol framing — those are all unit-tested here.

pub mod actd_client;
pub mod google;
pub mod google_api;
pub mod ha_client;
pub mod homeassistant;
pub mod mqtt_client;
pub mod oauth;
pub mod oauth_flow;
pub mod vault;

pub use oauth::{PkcePair, TokenSet};
pub use vault::TokenVault;

/// Decide whether a token should be proactively refreshed. We refresh at 80% of
/// the lifetime (architecture §4.1) with a small skew guard, so tools never
/// race an expiry mid-call.
pub fn should_refresh(obtained_at_unix: i64, expires_in_s: i64, now_unix: i64) -> bool {
    if expires_in_s <= 0 {
        return true;
    }
    let age = now_unix - obtained_at_unix;
    let threshold = (expires_in_s as f64 * 0.8) as i64;
    age >= threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_triggers_past_80_percent() {
        // 3600s token obtained at t=0; at t=2880 (80%) it should refresh.
        assert!(!should_refresh(0, 3600, 2879));
        assert!(should_refresh(0, 3600, 2880));
        assert!(should_refresh(0, 3600, 5000));
    }

    #[test]
    fn zero_or_negative_lifetime_forces_refresh() {
        assert!(should_refresh(0, 0, 0));
        assert!(should_refresh(0, -5, 0));
    }
}
