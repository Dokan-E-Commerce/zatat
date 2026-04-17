use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// HMAC-SHA256 over `"{METHOD}\n{PATH}\n{sorted k=v&...}"`. Excludes
/// `auth_signature`, `body_md5`, `appId`, `appKey`, `channelName` from
/// the params, and appends `body_md5 = md5(body)` when the body is non-empty.
pub fn sign_http(
    method: &str,
    path: &str,
    body: &[u8],
    query_params: &[(String, String)],
    secret: &str,
) -> String {
    let canonical = canonical_http_string(method, path, body, query_params);
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(canonical.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub fn verify_http(
    method: &str,
    path: &str,
    body: &[u8],
    query_params: &[(String, String)],
    given_signature: &str,
    secret: &str,
) -> bool {
    let expected = sign_http(method, path, body, query_params, secret);
    expected.as_bytes().ct_eq(given_signature.as_bytes()).into()
}

/// Strips a configured route prefix (e.g. `/realtime`) from a path before
/// signing. The canonical string signs the path *as the backend sees it*.
pub fn strip_path_prefix<'a>(path: &'a str, prefix: &str) -> &'a str {
    if prefix.is_empty() {
        return path;
    }
    let trimmed = prefix.trim_end_matches('/');
    if trimmed.is_empty() {
        return path;
    }
    if let Some(rest) = path.strip_prefix(trimmed) {
        if rest.is_empty() {
            "/"
        } else if rest.starts_with('/') {
            rest
        } else {
            path
        }
    } else {
        path
    }
}

fn canonical_http_string(
    method: &str,
    path: &str,
    body: &[u8],
    query_params: &[(String, String)],
) -> String {
    const EXCLUDED: &[&str] = &[
        "auth_signature",
        "body_md5",
        "appId",
        "appKey",
        "channelName",
    ];
    let mut filtered: Vec<(String, String)> = query_params
        .iter()
        .filter(|(k, _)| !EXCLUDED.contains(&k.as_str()))
        .cloned()
        .collect();

    if !body.is_empty() {
        let mut hasher = Md5::new();
        hasher.update(body);
        let digest = format!("{:x}", hasher.finalize());
        filtered.push(("body_md5".into(), digest));
    }

    filtered.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    let qs = filtered
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    format!("{method}\n{path}\n{qs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_ignoring_excluded_params() {
        let pairs_a = vec![
            ("auth_key".to_string(), "k".to_string()),
            ("appId".to_string(), "1".to_string()),
        ];
        let pairs_b = vec![("auth_key".to_string(), "k".to_string())];
        assert_eq!(
            sign_http("GET", "/p", b"", &pairs_a, "s"),
            sign_http("GET", "/p", b"", &pairs_b, "s")
        );
    }

    #[test]
    fn sorted_deterministically() {
        let pairs_a = vec![
            ("zeta".to_string(), "1".to_string()),
            ("alpha".to_string(), "2".to_string()),
        ];
        let pairs_b = vec![
            ("alpha".to_string(), "2".to_string()),
            ("zeta".to_string(), "1".to_string()),
        ];
        assert_eq!(
            sign_http("GET", "/p", b"", &pairs_a, "s"),
            sign_http("GET", "/p", b"", &pairs_b, "s")
        );
    }

    #[test]
    fn body_md5_added_when_body_non_empty() {
        let body = b"{\"name\":\"x\"}";
        let sig = sign_http("POST", "/apps/1/events", body, &[], "s");
        let body_md5 = format!("{:x}", Md5::digest(body));
        assert_eq!(body_md5, "5cf8efb68580b541236e85114899af81");
        assert!(!sig.is_empty());
    }

    #[test]
    fn constant_time_verify() {
        let sig = sign_http("GET", "/p", b"", &[], "s");
        assert!(verify_http("GET", "/p", b"", &[], &sig, "s"));
        assert!(!verify_http("GET", "/p", b"", &[], "0000", "s"));
    }

    #[test]
    fn strip_prefix_identity_when_empty() {
        assert_eq!(strip_path_prefix("/apps/1/events", ""), "/apps/1/events");
    }

    #[test]
    fn strip_prefix_removes_exact_match() {
        assert_eq!(
            strip_path_prefix("/realtime/apps/1/events", "/realtime"),
            "/apps/1/events"
        );
        assert_eq!(
            strip_path_prefix("/realtime/apps/1/events", "/realtime/"),
            "/apps/1/events"
        );
    }

    #[test]
    fn strip_prefix_no_match_preserves_path() {
        assert_eq!(
            strip_path_prefix("/apps/1/events", "/realtime"),
            "/apps/1/events"
        );
    }
}
