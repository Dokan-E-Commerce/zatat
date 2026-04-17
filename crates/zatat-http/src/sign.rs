use axum::http::Uri;

use zatat_core::application::AppArc;
use zatat_core::error::PusherError;
use zatat_protocol::http_sign::{strip_path_prefix, verify_http};

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
}
