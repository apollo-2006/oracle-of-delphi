//! AES-256-GCM token vault (architecture §4.1).
//!
//! Refresh/access tokens are sealed with AES-256-GCM. Each record uses a fresh
//! random 96-bit nonce, and the AAD binds the ciphertext to
//! `provider|account|scope-hash` so a sealed blob can never be replayed under a
//! different scope or account. The master key comes from the OS keyring in
//! production (Secret Service / DPAPI); here it is injected so the vault is
//! testable, and the key never touches disk in plaintext.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("seal failed")]
    Seal,
    #[error("open failed (wrong key, tampered ciphertext, or AAD mismatch)")]
    Open,
}

/// A sealed record: nonce prefixed to ciphertext. Self-describing so it can be
/// persisted as one blob per (provider, account).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedToken {
    pub nonce: [u8; 12],
    pub ciphertext: Vec<u8>,
}

impl SealedToken {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(12 + self.ciphertext.len());
        v.extend_from_slice(&self.nonce);
        v.extend_from_slice(&self.ciphertext);
        v
    }
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < 12 {
            return None;
        }
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&b[..12]);
        Some(SealedToken {
            nonce,
            ciphertext: b[12..].to_vec(),
        })
    }
}

pub struct TokenVault {
    cipher: Aes256Gcm,
}

impl TokenVault {
    /// Construct from a 32-byte master key (from the OS keyring in production).
    pub fn new(master_key: &[u8; 32]) -> Self {
        let key = Key::<Aes256Gcm>::from_slice(master_key);
        TokenVault {
            cipher: Aes256Gcm::new(key),
        }
    }

    /// AAD binding the ciphertext to its context — replay across scope/account
    /// is rejected at open time because the AAD won't match.
    fn aad(provider: &str, account: &str, scopes: &[String]) -> Vec<u8> {
        use sha2::{Digest, Sha256};
        let mut sorted = scopes.to_vec();
        sorted.sort();
        let mut h = Sha256::new();
        h.update(sorted.join(" ").as_bytes());
        let scope_hash = h.finalize();
        format!("{provider}|{account}|{scope_hash:x}").into_bytes()
    }

    pub fn seal(
        &self,
        provider: &str,
        account: &str,
        scopes: &[String],
        plaintext: &[u8],
    ) -> Result<SealedToken, VaultError> {
        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let aad = Self::aad(provider, account, scopes);
        let ciphertext = self
            .cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| VaultError::Seal)?;
        Ok(SealedToken {
            nonce: nonce_bytes,
            ciphertext,
        })
    }

    pub fn open(
        &self,
        provider: &str,
        account: &str,
        scopes: &[String],
        sealed: &SealedToken,
    ) -> Result<Vec<u8>, VaultError> {
        let nonce = Nonce::from_slice(&sealed.nonce);
        let aad = Self::aad(provider, account, scopes);
        self.cipher
            .decrypt(
                nonce,
                Payload {
                    msg: &sealed.ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| VaultError::Open)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> TokenVault {
        TokenVault::new(&[7u8; 32])
    }
    fn scopes() -> Vec<String> {
        vec!["gmail.modify".into(), "calendar.events".into()]
    }

    #[test]
    fn seal_open_roundtrip() {
        let v = vault();
        let sealed = v
            .seal("google", "abir", &scopes(), b"refresh-token-xyz")
            .unwrap();
        let opened = v.open("google", "abir", &scopes(), &sealed).unwrap();
        assert_eq!(opened, b"refresh-token-xyz");
    }

    #[test]
    fn nonce_is_unique_per_seal() {
        let v = vault();
        let a = v.seal("google", "abir", &scopes(), b"t").unwrap();
        let b = v.seal("google", "abir", &scopes(), b"t").unwrap();
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.ciphertext, b.ciphertext); // GCM randomized by nonce
    }

    #[test]
    fn wrong_account_fails_to_open() {
        let v = vault();
        let sealed = v.seal("google", "abir", &scopes(), b"t").unwrap();
        assert!(v
            .open("google", "someone_else", &scopes(), &sealed)
            .is_err());
    }

    #[test]
    fn scope_order_does_not_matter_but_scope_set_does() {
        let v = vault();
        let sealed = v.seal("google", "abir", &scopes(), b"t").unwrap();
        // reordered scopes: same set → opens
        let reordered = vec!["calendar.events".into(), "gmail.modify".into()];
        assert!(v.open("google", "abir", &reordered, &sealed).is_ok());
        // different scope set → fails (replay protection)
        let escalated = vec!["gmail.modify".into(), "gmail.send".into()];
        assert!(v.open("google", "abir", &escalated, &sealed).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let v = vault();
        let mut sealed = v.seal("google", "abir", &scopes(), b"t").unwrap();
        sealed.ciphertext[0] ^= 0xff;
        assert!(v.open("google", "abir", &scopes(), &sealed).is_err());
    }

    #[test]
    fn sealed_token_byte_roundtrip() {
        let v = vault();
        let sealed = v.seal("google", "abir", &scopes(), b"hello").unwrap();
        let bytes = sealed.to_bytes();
        let back = SealedToken::from_bytes(&bytes).unwrap();
        assert_eq!(sealed, back);
    }
}
