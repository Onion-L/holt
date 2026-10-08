//! The AWS SDK default credential chain pieces the TypeScript bedrock
//! adapter delegates to the SDK client: ECS container credentials
//! (`AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` /
//! `AWS_CONTAINER_CREDENTIALS_FULL_URI`) and web-identity (IRSA) credentials
//! (`AWS_WEB_IDENTITY_TOKEN_FILE` + `AWS_ROLE_ARN` via the STS
//! `AssumeRoleWithWebIdentity` call).
//!
//! Both providers cache their session credentials process-wide and refresh
//! once expired, matching the SDK providers' observable behavior. All HTTP
//! traffic goes through the crate [`HttpFetch`] transport, so tests inject
//! canned STS/container responses.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::ai::types::ProviderEnv;
use crate::ai::utils::http::{HttpBody, HttpFetch, HttpMethod, HttpRequest};
use crate::ai::utils::provider_env::get_provider_env_value;

/// Session credentials from a remote provider (ECS or STS).
#[derive(Clone, Debug, PartialEq)]
pub struct AwsSessionCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    /// Expiration as epoch milliseconds when the provider reported one.
    pub expires_at_ms: Option<i64>,
}

impl AwsSessionCredentials {
    fn sigv4_tuple(&self) -> (String, String, Option<String>) {
        (
            self.access_key_id.clone(),
            self.secret_access_key.clone(),
            self.session_token.clone(),
        )
    }

    fn is_expired(&self, now_ms: i64) -> bool {
        // The SDK providers stop serving credentials shortly before the
        // reported expiry; use the same margin-free check with a small
        // skew so a refresh never races the wall clock.
        self.expires_at_ms
            .is_some_and(|expires_at| now_ms + 5_000 >= expires_at)
    }
}

fn credential_cache() -> &'static Mutex<HashMap<String, AwsSessionCredentials>> {
    static CACHE: OnceLock<Mutex<HashMap<String, AwsSessionCredentials>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Test seam: drops the cached remote credentials.
pub fn clear_aws_credential_cache_for_tests() {
    credential_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

fn now_ms() -> i64 {
    crate::ai::auth::resolve::now_millis()
}

fn cached(key: &str) -> Option<(String, String, Option<String>)> {
    let cache = credential_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let credentials = cache.get(key)?;
    (!credentials.is_expired(now_ms())).then(|| credentials.sigv4_tuple())
}

fn store(key: &str, credentials: AwsSessionCredentials) {
    credential_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key.to_string(), credentials);
}

/// Parses an ISO-8601 / RFC 3339 timestamp (`2030-01-01T00:00:00Z`) into
/// epoch milliseconds; fractional seconds and offsets are accepted, other
/// calendars are not.
fn parse_iso8601_ms(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let year: i64 = value.get(0..4)?.parse().ok()?;
    if bytes[4] != b'-' || bytes[7] != b'-' || (bytes[10] != b'T' && bytes[10] != b' ') {
        return None;
    }
    let month: i64 = value.get(5..7)?.parse().ok()?;
    let day: i64 = value.get(8..10)?.parse().ok()?;
    let hour: i64 = value.get(11..13)?.parse().ok()?;
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let minute: i64 = value.get(14..16)?.parse().ok()?;
    let second: i64 = value.get(17..19)?.parse().ok()?;

    // Civil-time to days (Howard Hinnant's algorithm).
    let years = if month <= 2 { year - 1 } else { year };
    let era = years.div_euclid(400);
    let year_of_era = years.rem_euclid(400);
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let mut epoch_seconds = days * 86_400 + hour * 3_600 + minute * 60 + second;

    let mut millis = 0i64;
    let tail = &value[19..];
    let fraction_end = tail.find(['Z', 'z', '+', '-']).unwrap_or(tail.len());
    if let Some(digits) = tail[..fraction_end].strip_prefix('.') {
        let mut scaled = digits.to_string();
        while scaled.len() < 3 {
            scaled.push('0');
        }
        millis = scaled.get(0..3).and_then(|s| s.parse().ok()).unwrap_or(0);
    }
    let rest = &tail[fraction_end..];
    match rest {
        "" | "Z" | "z" => {}
        offset
            if (offset.starts_with('+') || offset.starts_with('-'))
                && (offset.len() == 5 || (offset.len() == 6 && offset.as_bytes()[3] == b':')) =>
        {
            let sign: i64 = if offset.starts_with('+') { 1 } else { -1 };
            let offset = offset.replace(':', "");
            let offset_hour: i64 = offset.get(1..3)?.parse().ok()?;
            let offset_minute: i64 = offset.get(3..5)?.parse().ok()?;
            epoch_seconds -= sign * (offset_hour * 3_600 + offset_minute * 60);
        }
        _ => return None,
    }
    Some(epoch_seconds * 1000 + millis)
}

/// Extracts the trimmed text of `tag` from a flat XML document (the STS
/// `AssumeRoleWithWebIdentity` response shape).
fn xml_element<'a>(document: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = document.find(&open)? + open.len();
    let end = start + document[start..].find(&close)?;
    Some(document[start..end].trim())
}

fn uri_encode_component(value: &str) -> String {
    // Form encoding (application/x-www-form-urlencoded): everything except
    // unreserved characters is percent-encoded, spaces become `%20` (the
    // SigV4 form convention the STS endpoint expects).
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

/// Port of the SDK container credential provider: fetches credentials from
/// the ECS/EKS metadata endpoints. `None` when no container env is set.
pub(crate) async fn ecs_container_credentials(
    fetch: &Arc<dyn HttpFetch>,
    env: Option<&ProviderEnv>,
) -> Option<Result<(String, String, Option<String>), String>> {
    let relative = get_provider_env_value("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", env);
    let full = get_provider_env_value("AWS_CONTAINER_CREDENTIALS_FULL_URI", env);
    let url = match (&relative, &full) {
        (Some(relative), _) => format!("http://169.254.170.2{}", relative.trim_end_matches('/')),
        (None, Some(full)) => full.clone(),
        (None, None) => return None,
    };
    let cache_key = format!("ecs:{url}");

    if let Some(credentials) = cached(&cache_key) {
        return Some(Ok(credentials));
    }

    let mut headers = Vec::new();
    if let Some(token) = get_provider_env_value("AWS_CONTAINER_AUTHORIZATION_TOKEN", env) {
        headers.push(("authorization".to_string(), token));
    }
    if full.is_some() {
        headers.push(("accept".to_string(), "application/json".to_string()));
    }

    let response = fetch
        .fetch(HttpRequest {
            signal: None,
            method: HttpMethod::Get,
            url,
            headers,
            body: HttpBody::Empty,
        })
        .await
        .map_err(|error| format!("failed to fetch container credentials: {error}"));
    let response = match response {
        Ok(response) => response,
        Err(error) => return Some(Err(error)),
    };
    if !(200..300).contains(&response.status) {
        return Some(Err(format!(
            "container credentials endpoint returned {}",
            response.status
        )));
    }
    let body = match crate::ai::utils::http::collect_text(response)
        .await
        .parse::<serde_json::Value>()
    {
        Ok(body) => body,
        Err(error) => return Some(Err(format!("invalid container credentials JSON: {error}"))),
    };
    let (Some(access_key_id), Some(secret_access_key)) = (
        body.get("AccessKeyId").and_then(|value| value.as_str()),
        body.get("SecretAccessKey").and_then(|value| value.as_str()),
    ) else {
        return Some(Err(
            "container credentials response missing AccessKeyId/SecretAccessKey".to_string(),
        ));
    };
    let credentials = AwsSessionCredentials {
        access_key_id: access_key_id.to_string(),
        secret_access_key: secret_access_key.to_string(),
        session_token: body
            .get("Token")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        expires_at_ms: body
            .get("Expiration")
            .and_then(|value| value.as_str())
            .and_then(parse_iso8601_ms),
    };
    store(&cache_key, credentials.clone());
    Some(Ok(credentials.sigv4_tuple()))
}

/// Port of the SDK web-identity (IRSA) credential provider: exchanges the
/// projected service-account token for session credentials through the STS
/// `AssumeRoleWithWebIdentity` action. `None` when no web-identity env is
/// set.
pub(crate) async fn web_identity_credentials(
    fetch: &Arc<dyn HttpFetch>,
    env: Option<&ProviderEnv>,
    region: &str,
) -> Option<Result<(String, String, Option<String>), String>> {
    let token_file = get_provider_env_value("AWS_WEB_IDENTITY_TOKEN_FILE", env)?;
    let role_arn = get_provider_env_value("AWS_ROLE_ARN", env)?;
    let session_name = get_provider_env_value("AWS_ROLE_SESSION_NAME", env)
        .unwrap_or_else(crate::ai::utils::uuid::uuidv7);
    let cache_key = format!("web-identity:{token_file}:{role_arn}");

    if let Some(credentials) = cached(&cache_key) {
        return Some(Ok(credentials));
    }

    let token = std::fs::read_to_string(&token_file)
        .map_err(|error| format!("failed to read web identity token: {error}"));
    let token = match token {
        Ok(token) => token,
        Err(error) => return Some(Err(error)),
    };

    // The SDK signs the STS call with SigV4 and the region's STS endpoint;
    // the unsigned form POST over TLS is the wire-equivalent call.
    let url = if region == "aws-global" {
        "https://sts.amazonaws.com/".to_string()
    } else {
        format!("https://sts.{region}.amazonaws.com/")
    };
    let form = [
        ("Action", "AssumeRoleWithWebIdentity"),
        ("Version", "2011-06-15"),
        ("RoleSessionName", session_name.as_str()),
        ("RoleArn", role_arn.as_str()),
        ("WebIdentityToken", token.trim()),
    ];
    let body: Vec<u8> = form
        .iter()
        .map(|(key, value)| format!("{}={}", key, uri_encode_component(value)))
        .collect::<Vec<_>>()
        .join("&")
        .into_bytes();

    let response = fetch
        .fetch(HttpRequest {
            signal: None,
            method: HttpMethod::Post,
            url,
            headers: vec![
                (
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
                ("accept".to_string(), "text/xml".to_string()),
            ],
            body: HttpBody::Bytes(bytes::Bytes::from(body)),
        })
        .await
        .map_err(|error| format!("failed to call AssumeRoleWithWebIdentity: {error}"));
    let response = match response {
        Ok(response) => response,
        Err(error) => return Some(Err(error)),
    };
    if !(200..300).contains(&response.status) {
        return Some(Err(format!(
            "AssumeRoleWithWebIdentity returned {}",
            response.status
        )));
    }
    let body = crate::ai::utils::http::collect_text(response).await;
    let (Some(access_key_id), Some(secret_access_key)) = (
        xml_element(&body, "AccessKeyId"),
        xml_element(&body, "SecretAccessKey"),
    ) else {
        return Some(Err(
            "AssumeRoleWithWebIdentity response missing credentials".to_string(),
        ));
    };
    let credentials = AwsSessionCredentials {
        access_key_id: access_key_id.to_string(),
        secret_access_key: secret_access_key.to_string(),
        session_token: xml_element(&body, "SessionToken").map(str::to_string),
        expires_at_ms: xml_element(&body, "Expiration").and_then(parse_iso8601_ms),
    };
    store(&cache_key, credentials.clone());
    Some(Ok(credentials.sigv4_tuple()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::utils::http::{HttpFetchError, HttpResponse};
    use futures::future::BoxFuture;

    /// The credential cache is process-global; the provider tests serialize
    /// on it (each one clears the cache on entry).
    async fn cache_lock() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    fn coerce(fetch: &Arc<CannedFetch>) -> Arc<dyn HttpFetch> {
        Arc::clone(fetch) as Arc<dyn HttpFetch>
    }

    fn scoped_env(pairs: &[(&str, &str)]) -> ProviderEnv {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    /// Records requests and serves canned bodies in order.
    struct CannedFetch {
        bodies: Mutex<Vec<String>>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl CannedFetch {
        fn new(bodies: Vec<String>) -> Arc<Self> {
            Arc::new(Self {
                bodies: Mutex::new(bodies),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<HttpRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl HttpFetch for CannedFetch {
        fn fetch<'a>(
            &'a self,
            request: HttpRequest,
        ) -> BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
            self.requests.lock().unwrap().push(request.clone());
            let body = self
                .bodies
                .lock()
                .unwrap()
                .pop()
                .expect("no canned response left");
            Box::pin(async move {
                Ok(HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
                })
            })
        }
    }

    fn ecs_body() -> String {
        serde_json::json!({
            "AccessKeyId": "ASIAECS",
            "SecretAccessKey": "ecs-secret",
            "Token": "ecs-session",
            "Expiration": "2030-01-01T00:00:00Z",
        })
        .to_string()
    }

    fn sts_body() -> String {
        "<AssumeRoleWithWebIdentityResponse><AssumeRoleWithWebIdentityResult>\
         <Credentials>\
         <AccessKeyId>ASIAIRSA</AccessKeyId>\
         <SecretAccessKey>irsa-secret</SecretAccessKey>\
         <SessionToken>irsa-session</SessionToken>\
         <Expiration>2030-01-01T00:00:00Z</Expiration>\
         </Credentials>\
         <AssumedRoleUser></AssumedRoleUser>\
         </AssumeRoleWithWebIdentityResult></AssumeRoleWithWebIdentityResponse>"
            .to_string()
    }

    fn write_token_file(contents: &str) -> String {
        let path = std::env::temp_dir().join(format!("pi-aws-web-identity-{}", std::process::id()));
        let mut unique = path.into_os_string();
        unique.push(format!("-{:?}", std::time::SystemTime::now()));
        let path = std::path::PathBuf::from(unique);
        std::fs::write(&path, contents).expect("write token file");
        path.to_string_lossy().to_string()
    }

    #[test]
    fn parses_iso8601_timestamps() {
        assert_eq!(
            parse_iso8601_ms("1970-01-01T00:00:00Z"),
            Some(0),
            "unix epoch"
        );
        assert_eq!(
            parse_iso8601_ms("2030-01-01T00:00:00Z"),
            Some(1_893_456_000_000)
        );
        assert_eq!(
            parse_iso8601_ms("2030-01-01T01:00:00+01:00"),
            Some(1_893_456_000_000),
            "offset shifts the instant"
        );
        assert_eq!(
            parse_iso8601_ms("2030-01-01T00:00:00.123Z").unwrap() % 1000,
            123,
            "fractional seconds accepted"
        );
        assert_eq!(parse_iso8601_ms("not-a-date"), None);
    }

    #[test]
    fn extracts_xml_elements() {
        let document = "<r><AccessKeyId>key</AccessKeyId><Empty></Empty></r>";
        assert_eq!(xml_element(document, "AccessKeyId"), Some("key"));
        assert_eq!(xml_element(document, "Missing"), None);
    }

    #[tokio::test]
    async fn ecs_relative_uri_fetches_the_container_metadata_endpoint() {
        let _guard = cache_lock().await;
        clear_aws_credential_cache_for_tests();
        let env = scoped_env(&[("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "/v2/creds/abc")]);
        let fetch = CannedFetch::new(vec![ecs_body()]);
        let credentials = ecs_container_credentials(&coerce(&fetch), Some(&env))
            .await
            .expect("provider configured")
            .expect("fetch ok");
        assert_eq!(
            credentials,
            (
                "ASIAECS".to_string(),
                "ecs-secret".to_string(),
                Some("ecs-session".to_string())
            )
        );

        let requests = fetch.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(requests[0].url, "http://169.254.170.2/v2/creds/abc");
        assert!(requests[0].headers.is_empty(), "no token configured");
    }

    #[tokio::test]
    async fn ecs_full_uri_sends_the_authorization_token() {
        let _guard = cache_lock().await;
        clear_aws_credential_cache_for_tests();
        let env = scoped_env(&[
            (
                "AWS_CONTAINER_CREDENTIALS_FULL_URI",
                "http://credentials.example/creds",
            ),
            ("AWS_CONTAINER_AUTHORIZATION_TOKEN", "Basic xyz"),
        ]);
        let fetch = CannedFetch::new(vec![ecs_body()]);
        let credentials = ecs_container_credentials(&coerce(&fetch), Some(&env))
            .await
            .expect("provider configured")
            .expect("fetch ok");
        assert_eq!(
            credentials.0, "ASIAECS",
            "credentials parsed from the full-uri response"
        );

        let requests = fetch.requests();
        assert_eq!(requests[0].url, "http://credentials.example/creds");
        assert!(
            requests[0]
                .headers
                .iter()
                .any(|(name, value)| { name == "authorization" && value == "Basic xyz" })
        );
    }

    #[tokio::test]
    async fn ecs_credentials_are_cached_until_expired() {
        let _guard = cache_lock().await;
        clear_aws_credential_cache_for_tests();
        let env = scoped_env(&[("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "/v2/creds/cache")]);
        let fetch = CannedFetch::new(vec![ecs_body()]);
        for _ in 0..2 {
            ecs_container_credentials(&coerce(&fetch), Some(&env))
                .await
                .expect("provider configured")
                .expect("fetch ok");
        }
        assert_eq!(
            fetch.requests().len(),
            1,
            "the second resolution reuses the cached credentials"
        );
    }

    #[tokio::test]
    async fn web_identity_exchanges_the_token_through_sts() {
        let _guard = cache_lock().await;
        clear_aws_credential_cache_for_tests();
        let token_file = write_token_file("projected-service-account-token\n");
        let env = scoped_env(&[
            ("AWS_WEB_IDENTITY_TOKEN_FILE", &token_file),
            ("AWS_ROLE_ARN", "arn:aws:iam::123:role/bedrock"),
            ("AWS_ROLE_SESSION_NAME", "pi-session"),
        ]);
        let fetch = CannedFetch::new(vec![sts_body()]);
        let credentials = web_identity_credentials(&coerce(&fetch), Some(&env), "us-west-2")
            .await
            .expect("provider configured")
            .expect("fetch ok");
        assert_eq!(
            credentials,
            (
                "ASIAIRSA".to_string(),
                "irsa-secret".to_string(),
                Some("irsa-session".to_string())
            )
        );

        let requests = fetch.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, "https://sts.us-west-2.amazonaws.com/");
        let form = match &requests[0].body {
            HttpBody::Bytes(bytes) => String::from_utf8(bytes.to_vec()).unwrap(),
            other => panic!("expected form body, got {other:?}"),
        };
        for expected in [
            "Action=AssumeRoleWithWebIdentity",
            "Version=2011-06-15",
            "RoleSessionName=pi-session",
            "RoleArn=arn%3Aaws%3Aiam%3A%3A123%3Arole%2Fbedrock",
            "WebIdentityToken=projected-service-account-token",
        ] {
            assert!(
                form.contains(expected),
                "form body {form:?} missing {expected:?}"
            );
        }
        let _ = std::fs::remove_file(&token_file);
    }

    #[tokio::test]
    async fn web_identity_stays_inactive_without_a_role_arn() {
        let token_file = write_token_file("token");
        let env = scoped_env(&[("AWS_WEB_IDENTITY_TOKEN_FILE", &token_file)]);
        let fetch = CannedFetch::new(Vec::new());
        assert!(
            web_identity_credentials(&coerce(&fetch), Some(&env), "us-east-1")
                .await
                .is_none()
        );
        assert_eq!(fetch.requests().len(), 0);
        let _ = std::fs::remove_file(&token_file);
    }

    #[tokio::test]
    async fn container_credentials_stay_inactive_without_env() {
        let fetch = CannedFetch::new(Vec::new());
        assert!(
            ecs_container_credentials(&coerce(&fetch), None)
                .await
                .is_none()
        );
        assert_eq!(fetch.requests().len(), 0);
    }
}
