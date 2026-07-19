use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::Uri;

use zatat_core::application::AppArc;
use zatat_core::error::PusherError;
use zatat_protocol::http_sign::{strip_path_prefix, verify_http};

const AUTH_TIMESTAMP_GRACE_SECONDS: i64 = 600;

#[derive(Debug)]
pub struct VerifyError {
    pub code: u16,
    pub message: String,
}

impl VerifyError {
    fn from_pusher(err: PusherError) -> Self {
        Self {
            code: 401,
            message: err.message().to_string(),
        }
    }
}

pub fn parse_query(uri: &Uri) -> Vec<(String, String)> {
    let Some(q) = uri.query() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.push((pct_decode(k), pct_decode(v)));
    }
    out
}

fn pct_decode(s: &str) -> String {
    let replaced: String = s.replace('+', " ");
    let bytes = replaced.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (
                char::from(bytes[i + 1]).to_digit(16),
                char::from(bytes[i + 2]).to_digit(16),
            ) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}

pub fn verify_request(
    app: &AppArc,
    method: &str,
    uri: &Uri,
    server_path_prefix: &str,
    body: &[u8],
) -> Result<(), VerifyError> {
    let pairs = parse_query(uri);
    let Some(given_sig) = pairs
        .iter()
        .find(|(k, _)| k == "auth_signature")
        .map(|(_, v)| v.clone())
    else {
        return Err(VerifyError::from_pusher(PusherError::InvalidAuthSignature));
    };
    let Some(ts) = pairs
        .iter()
        .find(|(k, _)| k == "auth_timestamp")
        .and_then(|(_, v)| v.parse::<i64>().ok())
    else {
        return Err(VerifyError {
            code: 401,
            message: "Timestamp required".into(),
        });
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if (now - ts).abs() > AUTH_TIMESTAMP_GRACE_SECONDS {
        return Err(VerifyError {
            code: 401,
            message: format!(
                "Timestamp expired: given timestamp {ts} is more than {AUTH_TIMESTAMP_GRACE_SECONDS} seconds old"
            ),
        });
    }
    let path = strip_path_prefix(uri.path(), server_path_prefix);
    if !verify_http(method, path, body, &pairs, &given_sig, &app.secret) {
        return Err(VerifyError {
            code: 401,
            message: "Authentication signature invalid.".into(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zatat_core::application::{AcceptClientEventsFrom, Application};
    use zatat_protocol::http_sign::sign_http;

    #[test]
    fn parse_query_decodes_percent_and_plus() {
        let uri: Uri = "/x?a=hello+world&b=c%2Fd&auth_signature=abc"
            .parse()
            .unwrap();
        let pairs = parse_query(&uri);
        assert_eq!(pairs[0], ("a".into(), "hello world".into()));
        assert_eq!(pairs[1], ("b".into(), "c/d".into()));
    }

    #[test]
    fn pct_decode_unicode() {
        assert_eq!(pct_decode("%E2%9C%93"), "✓");
    }

    const TEST_SECRET: &str = "dev-secret";
    const TEST_PATH: &str = "/apps/app-1/channels";

    fn mk_app() -> AppArc {
        std::sync::Arc::new(
            Application::new(
                "app-1".into(),
                "dev-key".into(),
                TEST_SECRET.into(),
                60,
                30,
                10_000,
                None,
                AcceptClientEventsFrom::Members,
                None,
                Vec::new(),
            )
            .expect("app builds"),
        )
    }

    fn now_secs() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn signed_uri(pairs: &[(String, String)]) -> Uri {
        let sig = sign_http("GET", TEST_PATH, &[], pairs, TEST_SECRET);
        let mut qs = pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        qs.push_str(&format!("&auth_signature={sig}"));
        format!("{TEST_PATH}?{qs}").parse().unwrap()
    }

    #[test]
    fn verify_request_passes_with_fresh_timestamp() {
        let app = mk_app();
        let pairs = vec![
            ("auth_key".to_string(), "dev-key".to_string()),
            ("auth_timestamp".to_string(), now_secs().to_string()),
            ("auth_version".to_string(), "1.0".to_string()),
        ];
        let uri = signed_uri(&pairs);
        assert!(verify_request(&app, "GET", &uri, "", &[]).is_ok());
    }

    #[test]
    fn verify_request_rejects_missing_timestamp() {
        let app = mk_app();
        let pairs = vec![
            ("auth_key".to_string(), "dev-key".to_string()),
            ("auth_version".to_string(), "1.0".to_string()),
        ];
        let uri = signed_uri(&pairs);
        let err = verify_request(&app, "GET", &uri, "", &[]).unwrap_err();
        assert_eq!(err.code, 401);
        assert_eq!(err.message, "Timestamp required");
    }

    #[test]
    fn verify_request_rejects_stale_timestamp() {
        let app = mk_app();
        let ts = now_secs() - 700;
        let pairs = vec![
            ("auth_key".to_string(), "dev-key".to_string()),
            ("auth_timestamp".to_string(), ts.to_string()),
            ("auth_version".to_string(), "1.0".to_string()),
        ];
        let uri = signed_uri(&pairs);
        let err = verify_request(&app, "GET", &uri, "", &[]).unwrap_err();
        assert_eq!(err.code, 401);
        assert!(err.message.contains("more than 600 seconds old"));
    }

    #[test]
    fn verify_request_rejects_future_timestamp_beyond_grace() {
        let app = mk_app();
        let ts = now_secs() + 700;
        let pairs = vec![
            ("auth_key".to_string(), "dev-key".to_string()),
            ("auth_timestamp".to_string(), ts.to_string()),
            ("auth_version".to_string(), "1.0".to_string()),
        ];
        let uri = signed_uri(&pairs);
        let err = verify_request(&app, "GET", &uri, "", &[]).unwrap_err();
        assert_eq!(err.code, 401);
    }
}
