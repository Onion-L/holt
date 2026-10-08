//! Port of `pi-core/ai/src/auth/oauth/xai.ts`: the xAI OAuth device-code
//! flow.
//!
//! The TypeScript flow uses global `fetch` (stubbed in tests); the Rust port
//! carries an injectable [`FetchFunction`] plus an injectable clock so the
//! token-expiry arithmetic stays deterministic under test.

use std::sync::Arc;

use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::ai::auth::oauth::NowMs;
use crate::ai::auth::oauth::device_code::{
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, poll_oauth_device_code_flow,
};
use crate::ai::auth::resolve::now_millis;
use crate::ai::auth::types::{
    AuthEvent, AuthInteraction, AuthStorageError, ModelAuth, OAuthAuth, OAuthCredential,
};
use crate::ai::types::FetchFunction;
use crate::ai::utils::http::{HttpBody, HttpMethod, HttpRequest, collect_text};

const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const XAI_DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const XAI_TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
/// Refresh slightly before the reported expiry to avoid using a token that
/// dies mid-request.
const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;
const DEFAULT_TOKEN_LIFETIME_SECONDS: f64 = 3600.0;

/// The xAI OAuth flow. Clone-cheap; `Default` uses the reqwest transport and
/// the real clock.
#[derive(Clone)]
pub struct XaiOAuth {
    fetch: FetchFunction,
    now_ms: NowMs,
}

impl Default for XaiOAuth {
    fn default() -> Self {
        Self {
            fetch: crate::ai::utils::reqwest_fetch::default_fetch(),
            now_ms: Arc::new(now_millis),
        }
    }
}

impl XaiOAuth {
    pub fn new(fetch: FetchFunction, now_ms: NowMs) -> Self {
        Self { fetch, now_ms }
    }

    fn now(&self) -> i64 {
        (self.now_ms)()
    }

    async fn post_form(
        &self,
        url: &str,
        fields: &[(&str, &str)],
        signal: &CancellationToken,
    ) -> Result<OAuthHttpResponse, String> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields.iter().map(|(key, value)| (*key, *value)))
            .finish();
        let response = self
            .fetch
            .fetch(HttpRequest {
                signal: None,
                method: HttpMethod::Post,
                url: url.to_string(),
                headers: vec![
                    ("accept".to_string(), "application/json".to_string()),
                    (
                        "content-type".to_string(),
                        "application/x-www-form-urlencoded".to_string(),
                    ),
                ],
                body: HttpBody::Text(body),
            })
            .await
            .map_err(|error| {
                if signal.is_cancelled() {
                    "Login cancelled".to_string()
                } else {
                    error.to_string()
                }
            })?;

        let status = response.status;
        let ok = (200..300).contains(&status);
        let text = collect_text(response).await;
        let body = match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            Ok(_) => Map::new(),
            Err(_) => {
                if signal.is_cancelled() {
                    return Err("Login cancelled".to_string());
                }
                return Err(format!("xAI OAuth returned invalid JSON (HTTP {status})"));
            }
        };
        Ok(OAuthHttpResponse { ok, status, body })
    }

    async fn request_device_code(
        &self,
        signal: &CancellationToken,
    ) -> Result<XaiDeviceCode, String> {
        let response = self
            .post_form(
                XAI_DEVICE_CODE_URL,
                &[
                    ("client_id", XAI_CLIENT_ID),
                    ("scope", XAI_SCOPE),
                    ("referrer", "pi"),
                ],
                signal,
            )
            .await?;
        if !response.ok {
            return Err(request_failure("device authorization", &response));
        }
        parse_device_code(&response.body)
    }

    async fn poll_for_tokens(
        &self,
        device: &XaiDeviceCode,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        let this = self.clone();
        let device = device.clone();
        let signal = signal.clone();
        poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
            interval_seconds: device.interval_seconds,
            expires_in_seconds: Some(device.expires_in_seconds),
            wait_before_first_poll: true,
            signal: signal.clone(),
            poll: move || {
                let this = this.clone();
                let device = device.clone();
                let signal = signal.clone();
                async move {
                    let response = this
                        .post_form(
                            XAI_TOKEN_URL,
                            &[
                                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                                ("client_id", XAI_CLIENT_ID),
                                ("device_code", device.device_code.as_str()),
                            ],
                            &signal,
                        )
                        .await;

                    let response = match response {
                        Ok(response) => response,
                        // A thrown poll error propagates out of the poller
                        // unchanged in TypeScript; `Failed` is the error
                        // channel here.
                        Err(message) => return OAuthDeviceCodePollResult::Failed { message },
                    };
                    if response.ok {
                        return match this.credentials_from_token_response(&response.body, None) {
                            Ok(value) => OAuthDeviceCodePollResult::Complete(value),
                            Err(message) => OAuthDeviceCodePollResult::Failed { message },
                        };
                    }

                    match response.body.get("error").and_then(Value::as_str) {
                        Some("authorization_pending") => OAuthDeviceCodePollResult::Pending,
                        Some("slow_down") => OAuthDeviceCodePollResult::SlowDown {
                            interval_seconds: response.body.get("interval").and_then(Value::as_f64),
                        },
                        Some("access_denied") | Some("authorization_denied") => {
                            OAuthDeviceCodePollResult::Failed {
                                message: "xAI device authorization was denied".to_string(),
                            }
                        }
                        Some("expired_token") => OAuthDeviceCodePollResult::Failed {
                            message: "xAI device code expired".to_string(),
                        },
                        _ => OAuthDeviceCodePollResult::Failed {
                            message: request_failure("device token polling", &response),
                        },
                    }
                }
            },
        })
        .await
    }

    async fn refresh_xai_token(
        &self,
        refresh_token: &str,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        let response = self
            .post_form(
                XAI_TOKEN_URL,
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", XAI_CLIENT_ID),
                    ("refresh_token", refresh_token),
                ],
                signal,
            )
            .await?;
        if !response.ok {
            return Err(request_failure("token refresh", &response));
        }
        self.credentials_from_token_response(&response.body, Some(refresh_token))
    }

    fn credentials_from_token_response(
        &self,
        body: &Map<String, Value>,
        previous_refresh_token: Option<&str>,
    ) -> Result<OAuthCredential, String> {
        let access = required_string(body, "access_token")?;
        // xAI may omit refresh_token on refresh when the token is not
        // rotated.
        let refresh = if !body.contains_key("refresh_token")
            && let Some(previous) = previous_refresh_token
        {
            previous.to_string()
        } else {
            required_string(body, "refresh_token")?
        };
        let expires_in_seconds = if body.contains_key("expires_in") {
            positive_number(body, "expires_in")?
        } else {
            DEFAULT_TOKEN_LIFETIME_SECONDS
        };
        Ok(OAuthCredential {
            access,
            refresh,
            expires: self.now() + (expires_in_seconds * 1000.0) as i64 - REFRESH_SKEW_MS,
            extra: Map::new(),
        })
    }
}

/// The shared `xaiOAuth` value.
pub fn xai_oauth() -> Arc<dyn OAuthAuth> {
    Arc::new(XaiOAuth::default())
}

impl OAuthAuth for XaiOAuth {
    fn name(&self) -> &str {
        "xAI (Grok/X subscription)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with SuperGrok or X Premium")
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
    ) -> crate::ai::auth::types::AuthFuture<Result<OAuthCredential, AuthStorageError>> {
        let this = self.clone();
        Box::pin(async move {
            let signal = interaction.signal().unwrap_or_default();
            let device = this
                .request_device_code(&signal)
                .await
                .map_err(AuthStorageError)?;
            interaction.notify(AuthEvent::DeviceCode {
                user_code: device.user_code.clone(),
                verification_uri: device
                    .verification_uri_complete
                    .clone()
                    .unwrap_or_else(|| device.verification_uri.clone()),
                interval_seconds: device.interval_seconds.map(|seconds| seconds as u64),
                expires_in_seconds: Some(device.expires_in_seconds as u64),
            });
            this.poll_for_tokens(&device, &signal)
                .await
                .map_err(AuthStorageError)
        })
    }

    fn refresh(
        &self,
        credential: &OAuthCredential,
        signal: CancellationToken,
    ) -> crate::ai::auth::types::AuthFuture<Result<OAuthCredential, AuthStorageError>> {
        let this = self.clone();
        let refresh_token = credential.refresh.clone();
        Box::pin(async move {
            this.refresh_xai_token(&refresh_token, &signal)
                .await
                .map_err(AuthStorageError)
        })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> crate::ai::auth::types::AuthFuture<Result<ModelAuth, AuthStorageError>> {
        let credential = credential.clone();
        Box::pin(async move {
            Ok(ModelAuth {
                api_key: Some(credential.access),
                ..Default::default()
            })
        })
    }
}

struct OAuthHttpResponse {
    ok: bool,
    status: u16,
    body: Map<String, Value>,
}

#[derive(Clone)]
struct XaiDeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    interval_seconds: Option<f64>,
    expires_in_seconds: f64,
}

fn required_string(body: &Map<String, Value>, field: &str) -> Result<String, String> {
    match body.get(field) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(format!("Invalid xAI OAuth response field: {field}")),
    }
}

fn positive_number(body: &Map<String, Value>, field: &str) -> Result<f64, String> {
    match body.get(field).and_then(Value::as_f64) {
        Some(value) if value.is_finite() && value > 0.0 => Ok(value),
        _ => Err(format!("Invalid xAI OAuth response field: {field}")),
    }
}

// The verification URI is opened in the user's browser; force it to be an
// https URL so a malicious response cannot make `open` launch something else.
fn validate_verification_uri(raw: &str) -> Result<String, String> {
    match url::Url::parse(raw) {
        Ok(url) if url.scheme() == "https" => Ok(url.to_string()),
        _ => Err("Untrusted verification URI in xAI OAuth response".to_string()),
    }
}

fn request_failure(action: &str, response: &OAuthHttpResponse) -> String {
    let error = response.body.get("error").and_then(Value::as_str);
    let description = response
        .body
        .get("error_description")
        .and_then(Value::as_str);
    let detail = [error, description]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(": ");
    let prefix = format!("xAI OAuth {action} failed (HTTP {})", response.status);
    if detail.is_empty() {
        prefix
    } else {
        format!("{prefix}: {detail}")
    }
}

fn parse_device_code(body: &Map<String, Value>) -> Result<XaiDeviceCode, String> {
    // RFC 8628 allows interval 0 (no minimum wait); fall back to the
    // poller's default instead of failing on non-positive or malformed
    // values.
    let interval_seconds = body
        .get("interval")
        .and_then(Value::as_f64)
        .filter(|interval| interval.is_finite() && *interval > 0.0);
    let device_code = required_string(body, "device_code")?;
    let user_code = required_string(body, "user_code")?;
    let verification_uri = validate_verification_uri(&required_string(body, "verification_uri")?)?;
    let verification_uri_complete = match body.get("verification_uri_complete") {
        Some(Value::String(value)) if !value.is_empty() => Some(validate_verification_uri(value)?),
        _ => None,
    };
    Ok(XaiDeviceCode {
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete,
        interval_seconds,
        expires_in_seconds: positive_number(body, "expires_in")?,
    })
}
