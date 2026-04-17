use serde::Deserialize;
use serde_json::Value;

/// Serializes a Pusher envelope with `data` embedded as a JSON string.
/// Omits the `channel` / `data` keys when absent.
pub fn encode_envelope(event: &str, data: Option<&Value>, channel: Option<&str>) -> String {
    let mut buf = String::with_capacity(64);
    buf.push('{');
    buf.push_str("\"event\":");
    write_json_string(&mut buf, event);

    if let Some(value) = data {
        let inner = serde_json::to_string(value)
            .expect("serde_json::Value always serializes to a JSON string");
        buf.push_str(",\"data\":");
        write_json_string(&mut buf, &inner);
    }

    if let Some(channel) = channel {
        buf.push_str(",\"channel\":");
        write_json_string(&mut buf, channel);
    }

    buf.push('}');
    buf
}

/// Like `encode_envelope` but `data_raw` is already a JSON-encoded string.
pub fn encode_envelope_raw_data(
    event: &str,
    data_raw: Option<&str>,
    channel: Option<&str>,
) -> String {
    let mut buf = String::with_capacity(64);
    buf.push('{');
    buf.push_str("\"event\":");
    write_json_string(&mut buf, event);

    if let Some(raw) = data_raw {
        buf.push_str(",\"data\":");
        write_json_string(&mut buf, raw);
    }
    if let Some(channel) = channel {
        buf.push_str(",\"channel\":");
        write_json_string(&mut buf, channel);
    }
    buf.push('}');
    buf
}

fn write_json_string(buf: &mut String, s: &str) {
    buf.push('"');
    for c in s.chars() {
        match c {
            '"' => buf.push_str("\\\""),
            '\\' => buf.push_str("\\\\"),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            '\x08' => buf.push_str("\\b"),
            '\x0c' => buf.push_str("\\f"),
            c if (c as u32) < 0x20 => buf.push_str(&format!("\\u{:04x}", c as u32)),
            c => buf.push(c),
        }
    }
    buf.push('"');
}

#[derive(Debug, Deserialize)]
pub struct InboundFrame {
    pub event: String,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub channel: Option<String>,
}

pub fn parse_inbound(text: &str) -> Result<InboundFrame, serde_json::Error> {
    serde_json::from_str(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_data_is_stringified() {
        let s = encode_envelope(
            "pusher:connection_established",
            Some(&json!({"socket_id":"1.2"})),
            None,
        );
        assert!(s.contains(r#""data":"{\"socket_id\":\"1.2\"}""#));
    }

    #[test]
    fn envelope_omits_empty_channel_and_data() {
        let s = encode_envelope("pusher:cache_miss", None, Some("cache-x"));
        assert!(!s.contains("data"));
        assert!(s.contains(r#""channel":"cache-x""#));
    }

    #[test]
    fn raw_data_passed_through() {
        let s = encode_envelope_raw_data("m", Some("{\"a\":1}"), Some("ch"));
        assert!(s.contains(r#""data":"{\"a\":1}""#));
        assert!(s.contains(r#""channel":"ch""#));
    }
}
