//! Ports `src/api-gateway/crypto-helper.js` exactly — AES-256-GCM,
//! key = the raw bytes of base64-decoded `CREDENTIALS_ENCRYPTION_KEY` (no
//! KDF/hashing of the key itself), 12-byte random IV per encryption,
//! stored as `"<iv_hex>:<authTag_hex>:<ciphertext_hex>"`. Must stay
//! byte-format-compatible with every already-encrypted row Node wrote
//! (`service_credentials`, `sso_configs.encrypted_client_secret`,
//! `notification_webhooks.encrypted_target`, `custom_actions.extra_headers`
//! secret values, `inbound_webhooks.webhook_secret`, `dead_letter_queue.
//! encrypted_payload`) — this is the single scheme covering all of them.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("CREDENTIALS_ENCRYPTION_KEY is not valid base64: {0}")]
    InvalidBase64(#[from] base64::DecodeError),
    #[error("CREDENTIALS_ENCRYPTION_KEY must decode to exactly 32 bytes, got {0}")]
    WrongKeyLength(usize),
    #[error("stored value is not in the expected \"iv:tag:ciphertext\" hex format")]
    MalformedStoredValue,
    #[error("hex decoding failed: {0}")]
    HexDecode(#[from] hex::FromHexError),
    #[error("decryption failed (wrong key, or the value was tampered with)")]
    DecryptionFailed,
}

#[derive(Clone)]
pub struct CredentialCipher {
    key: Key<Aes256Gcm>,
}

impl CredentialCipher {
    /// Mirrors the module-load-time behavior of crypto-helper.js: decode
    /// the env var as base64 and hard-require exactly 32 bytes. Node
    /// `process.exit(1)`s on failure; the caller here should do the
    /// equivalent (fail server startup), since a misconfigured key isn't
    /// recoverable at runtime.
    pub fn from_base64_key(raw: &str) -> Result<Self, CryptoError> {
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD.decode(raw)?;
        if decoded.len() != 32 {
            return Err(CryptoError::WrongKeyLength(decoded.len()));
        }
        Ok(Self {
            key: *Key::<Aes256Gcm>::from_slice(&decoded),
        })
    }

    pub fn encrypt(&self, plaintext: &str) -> String {
        let cipher = Aes256Gcm::new(&self.key);
        let mut iv_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut iv_bytes);
        let nonce = Nonce::from_slice(&iv_bytes);

        // `encrypt` on this crate returns ciphertext with the 16-byte GCM
        // tag appended — split it back apart so the stored format matches
        // Node's separate iv/tag/ciphertext fields exactly.
        let sealed = cipher
            .encrypt(nonce, Payload { msg: plaintext.as_bytes(), aad: &[] })
            .expect("AES-256-GCM encryption of a well-formed UTF-8 string never fails");
        let tag_start = sealed.len() - 16;
        let (ciphertext, tag) = sealed.split_at(tag_start);

        format!("{}:{}:{}", hex::encode(iv_bytes), hex::encode(tag), hex::encode(ciphertext))
    }

    pub fn decrypt(&self, stored: &str) -> Result<String, CryptoError> {
        let mut parts = stored.splitn(3, ':');
        let (iv_hex, tag_hex, ciphertext_hex) = match (parts.next(), parts.next(), parts.next()) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => return Err(CryptoError::MalformedStoredValue),
        };

        let iv = hex::decode(iv_hex)?;
        let tag = hex::decode(tag_hex)?;
        let ciphertext = hex::decode(ciphertext_hex)?;

        let cipher = Aes256Gcm::new(&self.key);
        let nonce = Nonce::from_slice(&iv);
        // Re-append the tag (this crate's `decrypt` expects
        // ciphertext||tag, same layout `encrypt` produced above).
        let mut sealed = ciphertext;
        sealed.extend_from_slice(&tag);

        let plaintext = cipher
            .decrypt(nonce, Payload { msg: &sealed, aad: &[] })
            .map_err(|_| CryptoError::DecryptionFailed)?;
        String::from_utf8(plaintext).map_err(|_| CryptoError::DecryptionFailed)
    }
}
