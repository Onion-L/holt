//! AWS Signature Version 4 request signing, used by the Bedrock wire
//! transport (the TypeScript implementation delegates to the AWS SDK).
//!
//! Implements the signing process from the AWS "Create a signed AWS API
//! request" documentation; the known-answer test pins the documented
//! Glacier signature example.

use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

fn sha256_hex(data: &[u8]) -> String {
    hex_lower(&Sha256::digest(data))
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// UriEncode per the SigV4 rules: every byte except `A-Z a-z 0-9 - . _ ~`,
/// with `/` preserved (path component).
pub fn uri_encode(value: &str, encode_slash: bool) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char)
            }
            b'/' if !encode_slash => encoded.push('/'),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// One canonical header entry: lowercase name, trimmed value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigV4Header {
    pub name: String,
    pub value: String,
}

/// The inputs to a SigV4 signature.
pub struct SigV4Request<'a> {
    pub method: &'a str,
    /// Absolute path (already the raw, unencoded form).
    pub path: &'a str,
    /// Sorted query pairs, unencoded.
    pub query: &'a [(&'a str, &'a str)],
    pub headers: Vec<SigV4Header>,
    pub body: &'a [u8],
}

/// Credentials for signing.
pub struct SigV4Credentials<'a> {
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    pub session_token: Option<&'a str>,
}

/// Signs a request and returns the `Authorization` header value, mutating
/// `headers` with the `x-amz-date` (and `x-amz-security-token`) entries it
/// signed.
pub fn sign_request(
    request: &SigV4Request<'_>,
    credentials: &SigV4Credentials<'_>,
    region: &str,
    service: &str,
    amz_date: &str,
) -> String {
    let date = &amz_date[..8];
    if !request.headers.iter().any(|header| header.name == "host") {
        panic!("host header required");
    }
    let mut headers = request.headers.clone();
    headers.push(SigV4Header {
        name: "x-amz-date".to_string(),
        value: amz_date.to_string(),
    });
    if let Some(session_token) = credentials.session_token {
        headers.push(SigV4Header {
            name: "x-amz-security-token".to_string(),
            value: session_token.to_string(),
        });
    }
    headers.sort_by(|a, b| a.name.cmp(&b.name));
    headers.dedup_by(|a, b| a.name == b.name);

    let canonical_headers: String = headers
        .iter()
        .map(|header| format!("{}:{}\n", header.name, header.value.trim()))
        .collect();
    let signed_headers: String = headers
        .iter()
        .map(|header| header.name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_query = request
        .query
        .iter()
        .map(|(name, value)| format!("{}={}", uri_encode(name, true), uri_encode(value, true)))
        .collect::<Vec<_>>()
        .join("&");
    let canonical_uri = uri_encode(request.path, false);
    let payload_hash = sha256_hex(request.body);
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        request.method,
        canonical_uri,
        canonical_query,
        canonical_headers,
        signed_headers,
        payload_hash
    );

    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac(
        format!("AWS4{}", credentials.secret_access_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex_lower(&hmac(&k_signing, string_to_sign.as_bytes()));

    format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        credentials.access_key_id, scope, signed_headers, signature
    )
}

/// Extracts the host header value for a URL (used by callers that only have
/// the endpoint).
pub fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    rest.split('/')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_documented_glacier_signature_example() {
        // https://docs.aws.amazon.com/amazonglacier/latest/dev/amazon-glacier-signing-requests.html
        let request = SigV4Request {
            method: "PUT",
            path: "/-/vaults/examplevault",
            query: &[],
            headers: vec![
                SigV4Header {
                    name: "host".to_string(),
                    value: "glacier.us-east-1.amazonaws.com".to_string(),
                },
                SigV4Header {
                    name: "x-amz-glacier-version".to_string(),
                    value: "2012-06-01".to_string(),
                },
            ],
            body: b"",
        };
        let credentials = SigV4Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE",
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            session_token: None,
        };
        let authorization = sign_request(
            &request,
            &credentials,
            "us-east-1",
            "glacier",
            "20120525T002453Z",
        );
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 \
             Credential=AKIAIOSFODNN7EXAMPLE/20120525/us-east-1/glacier/aws4_request, \
             SignedHeaders=host;x-amz-date;x-amz-glacier-version, \
             Signature=3ce5b2f2fffac9262b4da9256f8d086b4aaf42eba5f111c21681a65a127b7c2a"
        );
    }

    #[test]
    fn uri_encodes_per_the_sigv4_rules() {
        assert_eq!(uri_encode("a b/c~d", false), "a%20b/c~d");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(uri_encode("ünïcode", false), "%C3%BCn%C3%AFcode");
    }

    #[test]
    fn extracts_hosts() {
        assert_eq!(
            host_of("https://bedrock-runtime.us-east-1.amazonaws.com/x"),
            "bedrock-runtime.us-east-1.amazonaws.com"
        );
        assert_eq!(host_of("http://127.0.0.1:8080"), "127.0.0.1");
    }
}
