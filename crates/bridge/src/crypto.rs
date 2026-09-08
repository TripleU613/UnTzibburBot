//! Key management for data at rest.
//!
//! One master key (`BRIDGE_MASTER_KEY`) never encrypts anything directly. Every secret gets its
//! own key derived with HKDF-SHA256 from the master key, a purpose label and the account's
//! Tzibbur user id, so a leaked blob or cache file is useless without both the master key and
//! the account it belongs to. `BRIDGE_MASTER_KEY_PREVIOUS` lets the master key rotate: old
//! ciphertext is still readable and is re-wrapped to the current key when next used.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use hkdf::Hkdf;
use sha2::Sha256;
use std::sync::Arc;
use tzibbur_api::session::{AesGcmCipher, SecretCipher};

const SALT: &[u8] = b"untzibburbot/v1";
const V2_PREFIX: &str = "v2:";

#[derive(Clone)]
pub struct KeyRing {
    current: [u8; 32],
    previous: Option<[u8; 32]>,
}

impl KeyRing {
    pub fn new(current: [u8; 32], previous: Option<[u8; 32]>) -> Self {
        Self { current, previous }
    }

    fn derive(master: &[u8; 32], purpose: &str, context: &str) -> [u8; 32] {
        let hk = Hkdf::<Sha256>::new(Some(SALT), master);
        let mut info = Vec::with_capacity(purpose.len() + 1 + context.len());
        info.extend_from_slice(purpose.as_bytes());
        info.push(0);
        info.extend_from_slice(context.as_bytes());
        let mut out = [0u8; 32];
        hk.expand(&info, &mut out)
            .expect("32 bytes is a valid HKDF output length");
        out
    }

    /// Key for the account's local SQLCipher cache.
    pub fn cache_key(&self, account_context: &str) -> [u8; 32] {
        Self::derive(&self.current, "cache", account_context)
    }

    /// Cache keys derived from the previous master key (for rotation), if any.
    pub fn previous_cache_keys(&self, account_context: &str) -> Vec<[u8; 32]> {
        self.previous
            .iter()
            .map(|k| Self::derive(k, "cache", account_context))
            .collect()
    }

    fn session_ciphers(&self, context: &str) -> Vec<(AesGcmCipher, bool)> {
        let mut v = vec![(
            AesGcmCipher::new(Self::derive(&self.current, "session", context)),
            false,
        )];
        if let Some(prev) = &self.previous {
            v.push((
                AesGcmCipher::new(Self::derive(prev, "session", context)),
                true,
            ));
        }
        v
    }

    fn legacy_ciphers(&self) -> Vec<AesGcmCipher> {
        let mut v = vec![AesGcmCipher::new(self.current)];
        if let Some(prev) = &self.previous {
            v.push(AesGcmCipher::new(*prev));
        }
        v
    }
}

/// Encrypts Tzibbur session tokens for storage in Directus.
#[derive(Clone)]
pub struct SessionCipher {
    ring: Arc<KeyRing>,
}

/// A decrypted token plus whether the stored blob should be re-encrypted (it used a legacy
/// format or the previous master key).
pub struct Opened {
    pub token: String,
    pub rewrap: bool,
}

impl SessionCipher {
    pub fn new(ring: KeyRing) -> Self {
        Self {
            ring: Arc::new(ring),
        }
    }

    pub fn ring(&self) -> &KeyRing {
        &self.ring
    }

    /// Encrypt with a key bound to this account (`context` = Tzibbur user id).
    pub fn encrypt(&self, token: &str, context: &str) -> Result<String> {
        let (cipher, _) = self
            .ring
            .session_ciphers(context)
            .into_iter()
            .next()
            .expect("current key");
        let wire = cipher
            .encrypt(token.as_bytes())
            .context("encrypt session")?;
        Ok(format!(
            "{V2_PREFIX}{}",
            base64::engine::general_purpose::STANDARD.encode(wire)
        ))
    }

    pub fn decrypt(&self, blob: &str, context: &str) -> Result<Opened> {
        if let Some(b64) = blob.strip_prefix(V2_PREFIX) {
            let wire = decode(b64)?;
            for (cipher, is_previous) in self.ring.session_ciphers(context) {
                if let Ok(plain) = cipher.decrypt(&wire) {
                    return Ok(Opened {
                        token: String::from_utf8(plain).context("session utf8")?,
                        rewrap: is_previous,
                    });
                }
            }
            return Err(anyhow!(
                "cannot decrypt session (wrong BRIDGE_MASTER_KEY? set BRIDGE_MASTER_KEY_PREVIOUS when rotating)"
            ));
        }
        // Legacy blobs: AES-GCM directly under the master key, no account binding.
        let wire = decode(blob)?;
        for cipher in self.ring.legacy_ciphers() {
            if let Ok(plain) = cipher.decrypt(&wire) {
                return Ok(Opened {
                    token: String::from_utf8(plain).context("session utf8")?,
                    rewrap: true,
                });
            }
        }
        Err(anyhow!(
            "cannot decrypt legacy session (wrong BRIDGE_MASTER_KEY?)"
        ))
    }
}

fn decode(b64: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .context("session base64")
}

impl std::fmt::Debug for SessionCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionCipher")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_is_bound_to_account() {
        let c = SessionCipher::new(KeyRing::new([7u8; 32], None));
        let blob = c.encrypt("tok", "acct-a").unwrap();
        assert!(blob.starts_with("v2:"));
        let o = c.decrypt(&blob, "acct-a").unwrap();
        assert_eq!(o.token, "tok");
        assert!(!o.rewrap);
        // Same master key, different account: useless.
        assert!(c.decrypt(&blob, "acct-b").is_err());
        // Different master key: useless.
        assert!(SessionCipher::new(KeyRing::new([8u8; 32], None))
            .decrypt(&blob, "acct-a")
            .is_err());
    }

    #[test]
    fn rotation_and_legacy_rewrap() {
        let old = SessionCipher::new(KeyRing::new([1u8; 32], None));
        let blob = old.encrypt("tok", "a").unwrap();
        let rotated = SessionCipher::new(KeyRing::new([2u8; 32], Some([1u8; 32])));
        let o = rotated.decrypt(&blob, "a").unwrap();
        assert_eq!(o.token, "tok");
        assert!(o.rewrap);
        let fresh = rotated.encrypt(&o.token, "a").unwrap();
        assert!(!rotated.decrypt(&fresh, "a").unwrap().rewrap);
        assert!(SessionCipher::new(KeyRing::new([2u8; 32], None))
            .decrypt(&blob, "a")
            .is_err());

        // Legacy: raw AES-GCM under the master key without a prefix.
        let legacy = AesGcmCipher::new([1u8; 32]).encrypt(b"tok").unwrap();
        let legacy_blob = base64::engine::general_purpose::STANDARD.encode(legacy);
        let o = rotated.decrypt(&legacy_blob, "whatever").unwrap();
        assert_eq!(o.token, "tok");
        assert!(o.rewrap);
    }

    #[test]
    fn cache_keys_differ_per_account_and_rotate() {
        let r = KeyRing::new([1u8; 32], Some([0u8; 32]));
        assert_ne!(r.cache_key("a"), r.cache_key("b"));
        assert_eq!(r.previous_cache_keys("a").len(), 1);
        assert_eq!(
            r.previous_cache_keys("a")[0],
            KeyRing::new([0u8; 32], None).cache_key("a")
        );
    }
}
