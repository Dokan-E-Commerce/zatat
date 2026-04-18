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

/// Constant-time compare that avoids leaking lengths via an early-exit
/// memcmp. `subtle::ConstantTimeEq` returns false on length mismatch.
fn ct_eq_bytes(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
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
/// `{socket_id}:{channel}[:{channel_data}]`. Constant-time compare on
/// both the app-key prefix and the signature. Rejecting wrong prefixes
/// catches client misconfigurations (wrong app_key env var paired with a
/// valid sig for a different app) and matches Pusher's stricter behavior.
pub fn verify_channel_auth(
    expected_app_key: &str,
    socket_id: &str,
    channel: &str,
    channel_data: Option<&str>,
    auth: &str,
    secret: &str,
) -> Result<(), ChannelAuthError> {
    let (given_key, given_sig) = auth.split_once(':').ok_or(ChannelAuthError::BadFormat)?;
    // Always compute expected sig so we take the same amount of time on
    // prefix-mismatch vs. sig-mismatch (no early-exit side channel).
    let canonical = canonical_channel_string(socket_id, channel, channel_data);
    let expected = hmac_hex(secret.as_bytes(), canonical.as_bytes());
    let key_ok = ct_eq_bytes(given_key.as_bytes(), expected_app_key.as_bytes());
    let sig_ok = ct_eq_bytes(given_sig.as_bytes(), expected.as_bytes());
    match (key_ok, sig_ok) {
        (true, true) => Ok(()),
        (false, _) => Err(ChannelAuthError::KeyMismatch),
        (true, false) => Err(ChannelAuthError::Mismatch),
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
    expected_app_key: &str,
    socket_id: &str,
    user_data: &str,
    auth: &str,
    secret: &str,
) -> Result<(), ChannelAuthError> {
    let (given_key, given_sig) = auth.split_once(':').ok_or(ChannelAuthError::BadFormat)?;
    let canonical = format!("{socket_id}::user::{user_data}");
    let expected = hmac_hex(secret.as_bytes(), canonical.as_bytes());
    let key_ok = ct_eq_bytes(given_key.as_bytes(), expected_app_key.as_bytes());
    let sig_ok = ct_eq_bytes(given_sig.as_bytes(), expected.as_bytes());
    match (key_ok, sig_ok) {
        (true, true) => Ok(()),
        (false, _) => Err(ChannelAuthError::KeyMismatch),
        (true, false) => Err(ChannelAuthError::Mismatch),
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
        verify_channel_auth(APP_KEY, SOCKET_ID, "private-x", None, &auth, SECRET).unwrap();
    }

    #[test]
    fn presence_channel_round_trip() {
        let data = r#"{"user_id":"u-42"}"#;
        let auth = sign_channel_auth(APP_KEY, SOCKET_ID, "presence-lobby", Some(data), SECRET);
        verify_channel_auth(
            APP_KEY,
            SOCKET_ID,
            "presence-lobby",
            Some(data),
            &auth,
            SECRET,
        )
        .unwrap();
    }

    #[test]
    fn channel_data_whitespace_matters() {
        let data1 = r#"{"user_id":"u-42"}"#;
        let data2 = r#"{ "user_id": "u-42" }"#;
        let auth = sign_channel_auth(APP_KEY, SOCKET_ID, "presence-lobby", Some(data1), SECRET);
        assert!(matches!(
            verify_channel_auth(
                APP_KEY,
                SOCKET_ID,
                "presence-lobby",
                Some(data2),
                &auth,
                SECRET
            ),
            Err(ChannelAuthError::Mismatch)
        ));
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let auth = sign_channel_auth(APP_KEY, SOCKET_ID, "private-x", None, SECRET);
        assert!(matches!(
            verify_channel_auth(APP_KEY, SOCKET_ID, "private-x", None, &auth, "wrong"),
            Err(ChannelAuthError::Mismatch)
        ));
    }

    #[test]
    fn wrong_app_key_prefix_is_rejected() {
        // Regression: server used to accept any prefix. A client that
        // signs with the correct secret but presents a different app_key
        // (usually a misconfiguration) must now be rejected outright.
        let auth = sign_channel_auth(APP_KEY, SOCKET_ID, "private-x", None, SECRET);
        // Tamper only the key half, keep the valid sig.
        let bad_key_auth = {
            let (_, sig) = auth.split_once(':').unwrap();
            format!("wrong-key:{sig}")
        };
        assert!(matches!(
            verify_channel_auth(APP_KEY, SOCKET_ID, "private-x", None, &bad_key_auth, SECRET),
            Err(ChannelAuthError::KeyMismatch)
        ));
    }

    #[test]
    fn empty_key_prefix_is_rejected() {
        let auth = sign_channel_auth(APP_KEY, SOCKET_ID, "private-x", None, SECRET);
        let (_, sig) = auth.split_once(':').unwrap();
        let bad = format!(":{sig}");
        assert!(matches!(
            verify_channel_auth(APP_KEY, SOCKET_ID, "private-x", None, &bad, SECRET),
            Err(ChannelAuthError::KeyMismatch)
        ));
    }

    #[test]
    fn user_auth_round_trip() {
        let user_data = r#"{"id":"alice","user_info":{"name":"Alice"}}"#;
        let auth = sign_user_auth(APP_KEY, SOCKET_ID, user_data, SECRET);
        verify_user_auth(APP_KEY, SOCKET_ID, user_data, &auth, SECRET).unwrap();
    }

    #[test]
    fn user_auth_wrong_app_key_is_rejected() {
        let user_data = r#"{"id":"alice"}"#;
        let auth = sign_user_auth(APP_KEY, SOCKET_ID, user_data, SECRET);
        let (_, sig) = auth.split_once(':').unwrap();
        let bad = format!("wrong-key:{sig}");
        assert!(matches!(
            verify_user_auth(APP_KEY, SOCKET_ID, user_data, &bad, SECRET),
            Err(ChannelAuthError::KeyMismatch)
        ));
    }
}
