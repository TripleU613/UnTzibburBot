//! Encryption of Tzibbur session tokens at rest. Wraps the API crate's
//! AES-256-GCM cipher (same framing as the Android client) with the bridge
//! master key, and stores ciphertext as base64.

use anyhow::{Context, Result};
use base64::Engine;
use std::sync::Arc;
use tzibbur_api::session::{AesGcmCipher, SecretCipher};

#[derive(Clone)]
pub struct SessionCipher {
    inner: Arc<AesGcmCipher>,
}

impl SessionCipher {
    pub fn new(master_key: [u8; 32]) -> Self {
        Self { inner: Arc::new(AesGcmCipher::new(master_key)) }
    }

    pub fn encrypt(&self, token: &str) -> Result<String> {
        let wire = self.inner.encrypt(token.as_bytes()).context("encrypt session")?;
        Ok(base64::engine::general_purpose::STANDARD.encode(wire))
    }

    pub fn decrypt(&self, blob: &str) -> Result<String> {
        let wire = base64::engine::general_purpose::STANDARD.decode(blob.as_bytes()).context("session base64")?;
        let plain = self.inner.decrypt(&wire).context("decrypt session (wrong BRIDGE_MASTER_KEY?)")?;
        String::from_utf8(plain).context("session utf8")
    }
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
    fn roundtrip() {
        let c = SessionCipher::new([7u8; 32]);
        let blob = c.encrypt("tok").unwrap();
        assert_ne!(blob, "tok");
        assert_eq!(c.decrypt(&blob).unwrap(), "tok");
        assert!(SessionCipher::new([8u8; 32]).decrypt(&blob).is_err());
    }
}
