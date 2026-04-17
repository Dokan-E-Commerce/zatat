#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use zatat_protocol::presence::{PresenceData, PresenceMember};

// `serde_json::Value` doesn't derive Arbitrary — fuzz a JSON string
// instead and fall back to None on parse failure (same as the WS path).
#[derive(Debug, Arbitrary)]
struct FuzzMember {
    user_id: String,
    user_info_json: Option<String>,
}

impl FuzzMember {
    fn into_presence(self) -> PresenceMember {
        let user_info = self
            .user_info_json
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
        PresenceMember {
            user_id: self.user_id,
            user_info,
        }
    }
}

fuzz_target!(|members: Vec<Option<FuzzMember>>| {
    let converted: Vec<Option<PresenceMember>> = members
        .into_iter()
        .map(|m| m.map(|x| x.into_presence()))
        .collect();

    let had_none = converted.iter().any(|m| m.is_none());
    let data = PresenceData::from_members(converted);

    // Pusher spec: one missing user_id invalidates the whole roster.
    if had_none {
        assert_eq!(data.count, 0);
        assert!(data.ids.is_empty());
        assert!(data.hash.is_empty());
    }
    assert_eq!(data.count, data.ids.len());
    assert_eq!(data.count, data.hash.len());
    let mut sorted = data.ids.clone();
    sorted.sort();
    assert_eq!(sorted, data.ids);

    let _ = data.to_subscription_data();
});
