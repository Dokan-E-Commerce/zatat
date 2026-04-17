use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct SubscribeData {
    pub channel: String,
    #[serde(default)]
    pub auth: Option<String>,
    #[serde(default)]
    pub channel_data: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UnsubscribeData {
    pub channel: String,
}

#[derive(Debug, Deserialize)]
pub struct SigninData {
    pub auth: String,
    pub user_data: String,
    #[serde(default)]
    pub watchlist: Option<Vec<String>>,
}

/// Accepts `data` as either a JSON-encoded string or a native object;
/// SDK versions differ.
pub fn parse_data_loose<T>(data: &Value) -> Result<T, serde_json::Error>
where
    T: for<'de> Deserialize<'de>,
{
    if let Value::String(s) = data {
        serde_json::from_str(s)
    } else {
        T::deserialize(data.clone())
    }
}

pub fn data_as_value(data: &Value) -> Value {
    match data {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone())),
        _ => data.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_subscribe_from_stringified() {
        let raw = json!("{\"channel\":\"x\",\"auth\":\"k:abc\"}");
        let s: SubscribeData = parse_data_loose(&raw).unwrap();
        assert_eq!(s.channel, "x");
        assert_eq!(s.auth.unwrap(), "k:abc");
    }

    #[test]
    fn parse_subscribe_from_object() {
        let raw = json!({"channel": "x"});
        let s: SubscribeData = parse_data_loose(&raw).unwrap();
        assert_eq!(s.channel, "x");
        assert!(s.auth.is_none());
    }
}
