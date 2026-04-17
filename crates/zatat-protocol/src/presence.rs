use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PresenceMember {
    pub user_id: String,
    #[serde(default)]
    pub user_info: Option<Value>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PresenceData {
    pub count: usize,
    pub ids: Vec<String>,
    pub hash: BTreeMap<String, Value>,
}

impl PresenceData {
    pub fn from_members<I>(members: I) -> Self
    where
        I: IntoIterator<Item = Option<PresenceMember>>,
    {
        let mut seen: BTreeMap<String, Value> = BTreeMap::new();
        for member in members {
            let Some(m) = member else {
                return Self::empty();
            };
            seen.entry(m.user_id)
                .or_insert_with(|| m.user_info.unwrap_or(Value::Null));
        }
        let ids: Vec<String> = seen.keys().cloned().collect();
        Self {
            count: ids.len(),
            ids,
            hash: seen,
        }
    }

    pub fn empty() -> Self {
        Self {
            count: 0,
            ids: Vec::new(),
            hash: BTreeMap::new(),
        }
    }

    /// Wraps this roster as `{"presence": {count, ids, hash}}` — the
    /// `data` field of `pusher_internal:subscription_succeeded`.
    pub fn to_subscription_data(&self) -> Value {
        let mut hash = Map::with_capacity(self.hash.len());
        for (k, v) in &self.hash {
            hash.insert(k.clone(), v.clone());
        }
        let mut presence = Map::with_capacity(3);
        presence.insert("count".into(), Value::from(self.count));
        presence.insert(
            "ids".into(),
            Value::Array(self.ids.iter().cloned().map(Value::String).collect()),
        );
        presence.insert("hash".into(), Value::Object(hash));
        let mut outer = Map::with_capacity(1);
        outer.insert("presence".into(), Value::Object(presence));
        Value::Object(outer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hash_is_sorted_deterministically() {
        let data = PresenceData::from_members(vec![
            Some(PresenceMember {
                user_id: "b".into(),
                user_info: None,
            }),
            Some(PresenceMember {
                user_id: "a".into(),
                user_info: None,
            }),
        ]);
        assert_eq!(data.ids, vec!["a", "b"]);
    }

    #[test]
    fn dedup_by_user_id() {
        let data = PresenceData::from_members(vec![
            Some(PresenceMember {
                user_id: "a".into(),
                user_info: Some(json!({"n": 1})),
            }),
            Some(PresenceMember {
                user_id: "a".into(),
                user_info: Some(json!({"n": 2})),
            }),
        ]);
        assert_eq!(data.count, 1);
        assert_eq!(data.hash["a"], json!({"n": 1}), "first insertion wins");
    }

    #[test]
    fn missing_user_id_anywhere_returns_empty_roster() {
        let data = PresenceData::from_members(vec![
            Some(PresenceMember {
                user_id: "a".into(),
                user_info: None,
            }),
            None,
        ]);
        assert_eq!(data.count, 0);
        assert!(data.ids.is_empty());
    }

    #[test]
    fn missing_user_info_becomes_null_in_hash() {
        let data = PresenceData::from_members(vec![Some(PresenceMember {
            user_id: "a".into(),
            user_info: None,
        })]);
        assert_eq!(data.hash["a"], Value::Null);
    }

    #[test]
    fn to_subscription_data_shape() {
        let data = PresenceData::from_members(vec![Some(PresenceMember {
            user_id: "a".into(),
            user_info: Some(json!({"name":"Alice"})),
        })]);
        let v = data.to_subscription_data();
        assert_eq!(v["presence"]["count"], 1);
        assert_eq!(v["presence"]["ids"], json!(["a"]));
        assert_eq!(v["presence"]["hash"]["a"]["name"], "Alice");
    }
}
