use serde_json::{json, Value};

use crate::envelope::{encode_envelope, encode_envelope_raw_data};
use crate::presence::PresenceData;
use zatat_core::error::PusherError;

pub fn connection_established(socket_id: &str, activity_timeout: u32) -> String {
    encode_envelope(
        "pusher:connection_established",
        Some(&json!({
            "socket_id": socket_id,
            "activity_timeout": activity_timeout,
        })),
        None,
    )
}

pub fn error(err: &PusherError) -> String {
    let data = json!({
        "code": err.code(),
        "message": err.message(),
    });
    encode_envelope("pusher:error", Some(&data), None)
}

pub fn error_with_message(code: u16, message: &str) -> String {
    encode_envelope(
        "pusher:error",
        Some(&json!({ "code": code, "message": message })),
        None,
    )
}

pub fn ping() -> String {
    encode_envelope("pusher:ping", None, None)
}

pub fn pong() -> String {
    encode_envelope("pusher:pong", None, None)
}

pub fn subscription_succeeded(channel: &str, data: Option<&Value>) -> String {
    match data {
        Some(v) => encode_envelope(
            "pusher_internal:subscription_succeeded",
            Some(v),
            Some(channel),
        ),
        None => encode_envelope_raw_data(
            "pusher_internal:subscription_succeeded",
            Some("{}"),
            Some(channel),
        ),
    }
}

pub fn subscription_succeeded_with_presence(channel: &str, presence: &PresenceData) -> String {
    encode_envelope(
        "pusher_internal:subscription_succeeded",
        Some(&presence.to_subscription_data()),
        Some(channel),
    )
}

pub fn member_added(channel: &str, user_id: &str, user_info: Option<&Value>) -> String {
    let data = match user_info {
        Some(info) => json!({ "user_id": user_id, "user_info": info }),
        None => json!({ "user_id": user_id }),
    };
    encode_envelope("pusher_internal:member_added", Some(&data), Some(channel))
}

pub fn member_removed(channel: &str, user_id: &str) -> String {
    encode_envelope(
        "pusher_internal:member_removed",
        Some(&json!({ "user_id": user_id })),
        Some(channel),
    )
}

pub fn cache_miss(channel: &str) -> String {
    encode_envelope("pusher:cache_miss", None, Some(channel))
}

pub fn subscription_count(channel: &str, count: usize) -> String {
    encode_envelope(
        "pusher_internal:subscription_count",
        Some(&json!({ "subscription_count": count })),
        Some(channel),
    )
}

pub fn signin_success(user_data: &str) -> String {
    let data = json!({ "user_data": user_data });
    encode_envelope("pusher:signin_success", Some(&data), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presence::PresenceMember;

    #[test]
    fn connection_established_envelope() {
        let s = connection_established("1.2", 30);
        assert!(s.contains(r#""event":"pusher:connection_established""#));
        assert!(s.contains(r#""socket_id\":\"1.2\""#));
        assert!(s.contains(r#"\"activity_timeout\":30"#));
    }

    #[test]
    fn subscription_succeeded_with_presence_roster() {
        let p = PresenceData::from_members(vec![Some(PresenceMember {
            user_id: "u".into(),
            user_info: None,
        })]);
        let s = subscription_succeeded_with_presence("presence-x", &p);
        assert!(s.contains(r#""channel":"presence-x""#));
        assert!(s.contains(r#"presence"#));
    }
}
