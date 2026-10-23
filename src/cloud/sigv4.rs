//! AWS Signature Version 4 (`AWS4-HMAC-SHA256`) request signing.
//!
//! Services other than S3 sign the URI-encoded form of the path as sent, so a
//! Bedrock model ID like `anthropic.claude-…-v1:0` travels as `%3A` and is
//! signed as `%253A`.

use aws_lc_rs::hmac;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn sha256_hex(b: &[u8]) -> String {
    hex(&Sha256::digest(b))
}

fn hmac256(key: &[u8], data: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data)
        .as_ref()
        .to_vec()
}

/// RFC 3986 encoding of everything but unreserved characters (and `/` when `path`).
pub fn uri_encode(s: &str, path: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (path && b == b'/')
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The derived key for one day, region and service.
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k = hmac256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac256(&k, region.as_bytes());
    let k = hmac256(&k, service.as_bytes());
    hmac256(&k, b"aws4_request")
}

/// Sign a request. `headers` are extra headers to cover (e.g. `content-type`);
/// `host`, `x-amz-date` and `x-amz-security-token` are added here. Returns the
/// headers to send: `x-amz-date`, the session token if any, and `authorization`.
#[allow(clippy::too_many_arguments)] // eight orthogonal inputs to one signing step
pub fn sign(
    method: &str,
    url: &reqwest::Url,
    headers: &[(&str, &str)],
    body: &[u8],
    creds: &Credentials,
    region: &str,
    service: &str,
    time: chrono::DateTime<chrono::Utc>,
) -> Vec<(String, String)> {
    let amz_date = time.format("%Y%m%dT%H%M%SZ").to_string();
    let date = &amz_date[..8];
    let host = match url.port() {
        Some(p) => format!("{}:{p}", url.host_str().unwrap_or("")),
        None => url.host_str().unwrap_or("").to_string(),
    };
    let mut out: Vec<(String, String)> = vec![("x-amz-date".into(), amz_date.clone())];
    if let Some(t) = &creds.session_token {
        out.push(("x-amz-security-token".into(), t.clone()));
    }
    let mut canon: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| {
            (
                k.to_ascii_lowercase(),
                v.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .chain(std::iter::once(("host".to_string(), host)))
        .chain(out.iter().cloned())
        .collect();
    canon.sort();
    let canonical_headers: String = canon.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = canon
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };
    let mut query: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (uri_encode(&k, false), uri_encode(&v, false)))
        .collect();
    query.sort();
    let query = query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let request = format!(
        "{method}\n{}\n{query}\n{canonical_headers}\n{signed_headers}\n{}",
        uri_encode(path, true),
        sha256_hex(body)
    );
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(request.as_bytes())
    );
    let signature = hex(&hmac256(
        &signing_key(&creds.secret_access_key, date, region, service),
        to_sign.as_bytes(),
    ));
    out.push((
        "authorization".into(),
        format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}", creds.access_key_id),
    ));
    out
}
