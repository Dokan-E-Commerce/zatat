use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Error)]
pub enum ChannelAuthError {
    #[error("auth token missing `:` separator")]
    BadFormat,
    #[error("auth key mismatch")]
    KeyMismatch,
    #[error("signature mismatch")]
    Mismatch,
}

/// Signs `{socket_id}:{channel}[:{channel_data}]` and returns
/// `"{app_key}:{hex_signature}"` — the `auth` field a client sends in
/// `pusher:subscribe` for private and presence channels.
pub fn sign_channel_auth(
    app_key: &str,
    socket_id: &str,
    channel: &str,
    channel_data: Option<&str>,
    secret: &str,
) -> String {
    let canonical = canonical_channel_string(socket_id, channel, channel_data);
    let sig = hmac_hex(secret.as_bytes(), canonical.as_bytes());
    format!("{app_key}:{sig}")
}

/// Verifies that `auth = "{app_key}:{hex_sig}"` covers the expected
/// `{socket_id}:{channel}[:{channel_data}]`. Constant-time compare.
pub fn verify_channel_auth(
    socket_id: &str,
    channel: &str,
    channel_data: Option<&str>,
    auth: &str,
    secret: &str,
) -> Result<(), ChannelAuthError> {
    let (_, given_sig) = auth.split_once(':').ok_or(ChannelAuthError::BadFormat)?;
    let canonical = canonical_channel_string(socket_id, channel, channel_data);
    let expected = hmac_hex(secret.as_bytes(), canonical.as_bytes());
    if expected.as_bytes().ct_eq(given_sig.as_bytes()).into() {
        Ok(())
    } else {
        Err(ChannelAuthError::Mismatch)
    }
}

/// Signs the canonical form for `pusher:signin`:
/// `"{socket_id}::user::{user_data}"` (note the double-colon delimiters).
pub fn sign_user_auth(app_key: &str, socket_id: &str, user_data: &str, secret: &str) -> String {
    let canonical = format!("{socket_id}::user::{user_data}");
    let sig = hmac_hex(secret.as_bytes(), canonical.as_bytes());
    format!("{app_key}:{sig}")
}

pub fn verify_user_auth(
    socket_id: &str,
    user_data: &str,
    auth: &str,
    secret: &str,
) -> Result<(), ChannelAuthError> {
    let (_, given_sig) = auth.split_once(':').ok_or(ChannelAuthError::BadFormat)?;
    let canonical = format!("{socket_id}::user::{user_data}");
    let expected = hmac_hex(secret.as_bytes(), canonical.as_bytes());
    if expected.as_bytes().ct_eq(given_sig.as_bytes()).into() {
        Ok(())
    } else {
        Err(ChannelAuthError::Mismatch)
    }
}

fn canonical_channel_string(socket_id: &str, channel: &str, channel_data: Option<&str>) -> String {
    match channel_data {
        Some(data) => format!("{socket_id}:{channel}:{data}"),
        None => format!("{socket_id}:{channel}"),
    }
}

fn hmac_hex(key: &[u8], msg: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any length");
    mac.update(msg);
    hex::encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP_KEY: &str = "dev-key";
    const SECRET: &str = "dev-secret";
    const SOCKET_ID: &str = "12345.67890";

    #[test]
    fn private_channel_round_trip() {
        let auth = sign_channel_auth(APP_KEY, SOCKET_ID, "private-x", None, SECRET);
        verify_channel_auth(SOCKET_ID, "private-x", None, &auth, SECRET).unwrap();
    }

    #[test]
    fn presence_channel_round_trip() {
        let data = r#"{"user_id":"u-42"}"#;
        let auth = sign_channel_auth(APP_KEY, SOCKET_ID, "presence-lobby", Some(data), SECRET);
        verify_channel_auth(SOCKET_ID, "presence-lobby", Some(data), &auth, SECRET).unwrap();
    }

    #[test]
    fn channel_data_whitespace_matters() {
        let data1 = r#"{"user_id":"u-42"}"#;
        let data2 = r#"{ "user_id": "u-42" }"#;
        let auth = sign_channel_auth(APP_KEY, SOCKET_ID, "presence-lobby", Some(data1), SECRET);
        assert!(matches!(
            verify_channel_auth(SOCKET_ID, "presence-lobby", Some(data2), &auth, SECRET),
            Err(ChannelAuthError::Mismatch)
        ));
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let auth = sign_channel_auth(APP_KEY, SOCKET_ID, "private-x", None, SECRET);
        assert!(matches!(
            verify_channel_auth(SOCKET_ID, "private-x", None, &auth, "wrong"),
            Err(ChannelAuthError::Mismatch)
        ));
    }

    #[test]
    fn user_auth_round_trip() {
        let user_data = r#"{"id":"alice","user_info":{"name":"Alice"}}"#;
        let auth = sign_user_auth(APP_KEY, SOCKET_ID, user_data, SECRET);
        verify_user_auth(SOCKET_ID, user_data, &auth, SECRET).unwrap();
    }
}
