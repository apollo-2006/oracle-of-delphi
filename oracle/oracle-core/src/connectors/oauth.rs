//! OAuth2 Authorization Code + PKCE (architecture §4.1).
//!
//! Native-app profile: no client secret, PKCE on a loopback redirect. The parts
//! that must be exactly right — the S256 challenge derivation and the token
//! bookkeeping — are here and unit-tested. The actual browser pop + loopback
//! listener are a thin `authorize()` shell (not exercised in offline tests).

use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// A PKCE verifier/challenge pair. The verifier is kept secret in memory; the
/// challenge goes on the authorization URL.
#[derive(Debug, Clone)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
    pub method: &'static str,
}

impl PkcePair {
    /// Generate a fresh pair. Verifier is 43-128 chars of unreserved base64url
    /// (RFC 7636 §4.1); challenge = BASE64URL(SHA256(verifier)).
    pub fn generate() -> Self {
        let mut raw = [0u8; 32]; // 32 bytes → 43 base64url chars
        rand::thread_rng().fill_bytes(&mut raw);
        let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        let challenge = Self::derive_challenge(&verifier);
        PkcePair {
            verifier,
            challenge,
            method: "S256",
        }
    }

    /// Pure derivation, so it can be tested against RFC test vectors.
    pub fn derive_challenge(verifier: &str) -> String {
        let digest = Sha256::digest(verifier.as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
    }

    /// Build the authorization URL query for a provider.
    pub fn auth_url(
        &self,
        auth_endpoint: &str,
        client_id: &str,
        redirect_uri: &str,
        scopes: &[String],
        state: &str,
    ) -> String {
        let scope = scopes.join(" ");
        let q = [
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("scope", &scope),
            ("code_challenge", &self.challenge),
            ("code_challenge_method", self.method),
            ("state", state),
            ("access_type", "offline"), // ask Google for a refresh token
            ("prompt", "consent"),
        ]
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
        format!("{auth_endpoint}?{q}")
    }
}

/// Tokens as returned by the provider, plus the local bookkeeping we add.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in_s: i64,
    pub obtained_at_unix: i64,
    pub scopes: Vec<String>,
}

impl TokenSet {
    /// Merge a refresh response into the current set. Google often omits the
    /// refresh_token on refresh; we must KEEP the existing one in that case —
    /// dropping it silently orphans the grant (a classic bug this guards).
    pub fn apply_refresh(
        &mut self,
        new_access: String,
        new_refresh: Option<String>,
        expires_in_s: i64,
        now: i64,
    ) {
        self.access_token = new_access;
        if let Some(rt) = new_refresh {
            self.refresh_token = Some(rt);
        }
        self.expires_in_s = expires_in_s;
        self.obtained_at_unix = now;
    }

    pub fn is_expired(&self, now: i64) -> bool {
        now - self.obtained_at_unix >= self.expires_in_s
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7636_s256_test_vector() {
        // The canonical example from RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(PkcePair::derive_challenge(verifier), expected);
    }

    #[test]
    fn generated_verifier_is_valid_length() {
        let p = PkcePair::generate();
        assert!((43..=128).contains(&p.verifier.len()));
        assert_eq!(p.challenge, PkcePair::derive_challenge(&p.verifier));
    }

    #[test]
    fn auth_url_contains_pkce_and_offline() {
        let p = PkcePair::generate();
        let url = p.auth_url(
            "https://accounts.google.com/o/oauth2/v2/auth",
            "cid.apps.googleusercontent.com",
            "http://127.0.0.1:8721/callback",
            &["https://www.googleapis.com/auth/gmail.modify".into()],
            "xyzstate",
        );
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("state=xyzstate"));
        // redirect must be percent-encoded
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1"));
    }

    #[test]
    fn refresh_keeps_existing_refresh_token_when_omitted() {
        let mut ts = TokenSet {
            access_token: "old".into(),
            refresh_token: Some("keep-me".into()),
            expires_in_s: 3600,
            obtained_at_unix: 0,
            scopes: vec!["gmail.modify".into()],
        };
        ts.apply_refresh("new-access".into(), None, 3600, 100);
        assert_eq!(ts.access_token, "new-access");
        assert_eq!(ts.refresh_token.as_deref(), Some("keep-me")); // NOT dropped
        assert_eq!(ts.obtained_at_unix, 100);
    }

    #[test]
    fn refresh_rotates_refresh_token_when_present() {
        let mut ts = TokenSet {
            access_token: "old".into(),
            refresh_token: Some("old-rt".into()),
            expires_in_s: 3600,
            obtained_at_unix: 0,
            scopes: vec![],
        };
        ts.apply_refresh("a".into(), Some("new-rt".into()), 3600, 50);
        assert_eq!(ts.refresh_token.as_deref(), Some("new-rt"));
    }
}
