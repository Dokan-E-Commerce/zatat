use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use crypto_secretbox::aead::generic_array::GenericArray;
use crypto_secretbox::aead::{Aead, KeyInit, OsRng};
use crypto_secretbox::{Key, XSalsa20Poly1305};
#[allow(unused_imports)]
use rand::rngs::OsRng as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EncryptionError {
    #[error("master key is not valid base64")]
    InvalidMasterKey,
    #[error("master key must be 32 bytes")]
    WrongMasterKeyLength,
    #[error("encryption failed")]
    EncryptFailed,
    #[error("decryption failed")]
    DecryptFailed,
    #[error("bad encrypted envelope")]
    BadEnvelope,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EncryptedEnvelope {
    pub nonce: String,
    pub ciphertext: String,
}

pub fn looks_encrypted(data: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return false;
    };
    v.get("nonce").is_some() && v.get("ciphertext").is_some()
}

pub fn decode_master_key(b64: &str) -> Result<[u8; 32], EncryptionError> {
    let bytes = BASE64
        .decode(b64)
        .map_err(|_| EncryptionError::InvalidMasterKey)?;
    if bytes.len() != 32 {
        return Err(EncryptionError::WrongMasterKeyLength);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Per-channel key = SHA256(channel_name || master_key). Must match
/// `pusher-http-node`'s `generateSharedSecret` or the browser SDK can't
/// decrypt what we publish.
pub fn derive_shared_secret(channel: &str, master: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(channel.as_bytes());
    h.update(master);
    let out = h.finalize();
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&out);
    secret
}

pub fn encrypt_payload(plaintext: &[u8], secret: &[u8; 32]) -> Result<String, EncryptionError> {
    let key = Key::from_slice(secret);
    let cipher = XSalsa20Poly1305::new(key);
    let mut nonce_bytes = [0u8; 24];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = GenericArray::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| EncryptionError::EncryptFailed)?;
    let env = EncryptedEnvelope {
        nonce: BASE64.encode(nonce_bytes),
        ciphertext: BASE64.encode(ct),
    };
    serde_json::to_string(&env).map_err(|_| EncryptionError::EncryptFailed)
}

pub fn decrypt_payload(envelope_json: &str, secret: &[u8; 32]) -> Result<Vec<u8>, EncryptionError> {
    let env: EncryptedEnvelope =
        serde_json::from_str(envelope_json).map_err(|_| EncryptionError::BadEnvelope)?;
    let nonce_bytes = BASE64
        .decode(&env.nonce)
        .map_err(|_| EncryptionError::BadEnvelope)?;
    if nonce_bytes.len() != 24 {
        return Err(EncryptionError::BadEnvelope);
    }
    let ct_bytes = BASE64
        .decode(&env.ciphertext)
        .map_err(|_| EncryptionError::BadEnvelope)?;
    let key = Key::from_slice(secret);
    let cipher = XSalsa20Poly1305::new(key);
    let nonce = GenericArray::from_slice(&nonce_bytes);
    cipher
        .decrypt(nonce, ct_bytes.as_slice())
        .map_err(|_| EncryptionError::DecryptFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let master = [7u8; 32];
        let secret = derive_shared_secret("private-encrypted-x", &master);
        let ct = encrypt_payload(b"hello", &secret).unwrap();
        let pt = decrypt_payload(&ct, &secret).unwrap();
        assert_eq!(pt, b"hello");
    }

    #[test]
    fn looks_encrypted_detects_envelope() {
        assert!(looks_encrypted(r#"{"nonce":"AAAA","ciphertext":"BBBB"}"#));
        assert!(!looks_encrypted(r#"{"hello":"world"}"#));
    }

    #[test]
    fn derive_shared_secret_is_deterministic() {
        let master = [1u8; 32];
        let a = derive_shared_secret("ch", &master);
        let b = derive_shared_secret("ch", &master);
        assert_eq!(a, b);
    }

    #[test]
    fn decrypt_payload_never_panics_on_short_nonce() {
        // Regression: fuzz corpus discovered that an envelope with an
        // empty (or non-24-byte) `nonce` field panicked inside
        // `GenericArray::from_slice`. Must return BadEnvelope instead.
        let key = [0x42u8; 32];
        assert!(decrypt_payload(r#"{"nonce":"","ciphertext":""}"#, &key).is_err());
        assert!(decrypt_payload(r#"{"nonce":"AAAA","ciphertext":"AAAA"}"#, &key).is_err());
        assert!(decrypt_payload(r#"{"nonce":"","ciphertext":"ZmFrZQ=="}"#, &key).is_err());
    }
}
