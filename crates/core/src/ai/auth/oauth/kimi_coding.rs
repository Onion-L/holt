//! Port of `pi-core/ai/src/auth/oauth/kimi-coding.ts`: the Kimi Code
//! subscription OAuth flow.
//!
//! RFC 8628 device authorization grant against `https://auth.kimi.com` with
//! JSON responses. The access token authenticates requests to
//! `https://api.kimi.com/coding` as an `Authorization: Bearer` header.
//!
//! The TypeScript flow uses global `fetch` (stubbed in tests) with a 30s
//! per-request `AbortSignal.timeout`; the Rust port carries an injectable
//! [`FetchFunction`] (with `tokio::time::timeout` reproducing the request
//! timeout), an injectable clock, and provider-scoped env overrides so the
//! host override stays testable without mutating process env.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::ai::auth::oauth::NowMs;
use crate::ai::auth::oauth::device_code::{
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, poll_oauth_device_code_flow,
};
use crate::ai::auth::resolve::now_millis;
use crate::ai::auth::types::{
    AuthEvent, AuthInteraction, AuthStorageError, ModelAuth, OAuthAuth, OAuthCredential,
};
use crate::ai::types::{FetchFunction, ProviderEnv};
use crate::ai::utils::http::{HttpBody, HttpMethod, HttpRequest, collect_text};
use crate::ai::utils::provider_env::get_provider_env_value;

const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const DEFAULT_OAUTH_HOST: &str = "https://auth.kimi.com";
const DEVICE_CODE_TIMEOUT_SECONDS: f64 = 15.0 * 60.0;
const DEFAULT_POLL_INTERVAL_SECONDS: f64 = 5.0;
const REQUEST_TIMEOUT_MS: u64 = 30 * 1000;
const REFRESH_MAX_RETRIES: u32 = 3;

/// The Kimi Code OAuth flow. Clone-cheap; `Default` uses the reqwest
/// transport, the real clock, and process env for host overrides.
#[derive(Clone)]
pub struct KimiCodingOAuth {
    fetch: FetchFunction,
    now_ms: NowMs,
    env: ProviderEnv,
}

impl Default for KimiCodingOAuth {
    fn default() -> Self {
        Self {
            fetch: crate::ai::utils::reqwest_fetch::default_fetch(),
            now_ms: Arc::new(now_millis),
            env: ProviderEnv::new(),
        }
    }
}

impl KimiCodingOAuth {
    pub fn new(fetch: FetchFunction, now_ms: NowMs, env: ProviderEnv) -> Self {
        Self { fetch, now_ms, env }
    }

    fn now(&self) -> i64 {
        (self.now_ms)()
    }

    fn oauth_host(&self) -> String {
        let override_host = get_provider_env_value("KIMI_CODE_OAUTH_HOST", Some(&self.env))
            .or_else(|| get_provider_env_value("KIMI_OAUTH_HOST", Some(&self.env)));
        override_host
            .unwrap_or_else(|| DEFAULT_OAUTH_HOST.to_string())
            .trim_end_matches('/')
            .to_string()
    }

    /// Port of `postForm`-style fetch with the combined timeout+abort signal.
    async fn post_json(
        &self,
        url: &str,
        fields: &[(&str, &str)],
        signal: &CancellationToken,
    ) -> Result<JsonResponse, String> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields.iter().map(|(key, value)| (*key, *value)))
            .finish();
        let request = HttpRequest {
            signal: None,
            method: HttpMethod::Post,
            url: url.to_string(),
            headers: vec![
                (
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
                ("accept".to_string(), "application/json".to_string()),
            ],
            body: HttpBody::Text(body),
        };
        let fetch = Arc::clone(&self.fetch);
        let response = tokio::time::timeout(
            Duration::from_millis(REQUEST_TIMEOUT_MS),
            fetch.fetch(request),
        )
        .await
        .map_err(|_| {
            if signal.is_cancelled() {
                "Login cancelled".to_string()
            } else {
                "signal timed out".to_string()
            }
        })?
        .map_err(|error| {
            if signal.is_cancelled() {
                "Login cancelled".to_string()
            } else {
                error.to_string()
            }
        })?;

        let status = response.status;
        let text = collect_text(response).await;
        // `readJson` keeps objects (including arrays) and drops everything
        // falsy or non-object (parse failures included).
        let json = serde_json::from_str::<Value>(&text)
            .ok()
            .filter(|json| json.is_object() || json.is_array());
        Ok(JsonResponse { status, text, json })
    }

    async fn start_device_authorization(
        &self,
        oauth_host: &str,
        signal: &CancellationToken,
    ) -> Result<DeviceAuthorization, String> {
        let response = self
            .post_json(
                &format!("{oauth_host}/api/oauth/device_authorization"),
                &[("client_id", CLIENT_ID)],
                signal,
            )
            .await?;

        if !(200..300).contains(&response.status) {
            let suffix = if response.text.is_empty() {
                String::new()
            } else {
                format!(": {}", response.text)
            };
            return Err(format!(
                "Kimi Code device authorization failed with status {}{suffix}",
                response.status
            ));
        }

        let json = response.json.clone().unwrap_or(Value::Null);
        let string_field = |name: &str| json.get(name).and_then(Value::as_str);
        let device_code = string_field("device_code");
        let user_code = string_field("user_code");
        let verification_uri = string_field("verification_uri");
        let verification_uri_complete = string_field("verification_uri_complete");
        if device_code.is_none()
            || user_code.is_none()
            || verification_uri.is_none()
            || verification_uri_complete.is_none()
            || trusted_http_url(verification_uri_complete.unwrap_or_default()).is_none()
            || trusted_http_url(verification_uri.unwrap_or_default()).is_none()
        {
            return Err(format!(
                "Invalid Kimi Code device authorization response: {json}"
            ));
        }

        let interval = json.get("interval").and_then(Value::as_f64);
        let expires_in = json.get("expires_in").and_then(Value::as_f64);
        Ok(DeviceAuthorization {
            device_code: device_code.unwrap().to_string(),
            user_code: user_code.unwrap().to_string(),
            // The base `verification_uri` is validated but the flow only
            // surfaces the complete variant.
            verification_uri_complete: verification_uri_complete.unwrap().to_string(),
            interval_seconds: interval
                .filter(|interval| interval.is_finite() && *interval > 0.0)
                .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS),
            expires_in_seconds: expires_in
                .filter(|expires_in| expires_in.is_finite() && *expires_in > 0.0)
                .unwrap_or(DEVICE_CODE_TIMEOUT_SECONDS),
        })
    }

    async fn poll_for_token(
        &self,
        oauth_host: &str,
        device: &DeviceAuthorization,
        signal: &CancellationToken,
    ) -> Result<TokenResponse, String> {
        let this = self.clone();
        let oauth_host = oauth_host.to_string();
        let device = device.clone();
        let signal = signal.clone();
        poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
            interval_seconds: Some(device.interval_seconds),
            expires_in_seconds: Some(device.expires_in_seconds),
            wait_before_first_poll: true,
            signal: signal.clone(),
            poll: move || {
                let this = this.clone();
                let oauth_host = oauth_host.clone();
                let device = device.clone();
                let signal = signal.clone();
                async move {
                    let response = match this
                        .post_json(
                            &format!("{oauth_host}/api/oauth/token"),
                            &[
                                ("client_id", CLIENT_ID),
                                ("device_code", device.device_code.as_str()),
                                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                            ],
                            &signal,
                        )
                        .await
                    {
                        Ok(response) => response,
                        Err(message) => return OAuthDeviceCodePollResult::Failed { message },
                    };

                    if response.status >= 500 {
                        let suffix = if response.text.is_empty() {
                            String::new()
                        } else {
                            format!(": {}", response.text)
                        };
                        return OAuthDeviceCodePollResult::Failed {
                            message: format!(
                                "Kimi Code device token request failed with status {}{suffix}",
                                response.status
                            ),
                        };
                    }

                    let has_access_token = response
                        .json
                        .as_ref()
                        .and_then(|json| json.get("access_token"))
                        .is_some_and(Value::is_string);
                    if (200..300).contains(&response.status) && has_access_token {
                        return match parse_token_response(
                            response.json.as_ref(),
                            "poll",
                            this.now(),
                        ) {
                            Ok(value) => OAuthDeviceCodePollResult::Complete(value),
                            Err(message) => OAuthDeviceCodePollResult::Failed { message },
                        };
                    }

                    let json = response.json.clone().unwrap_or(Value::Null);
                    let error = json.get("error").cloned();
                    let description = match json.get("error_description").and_then(Value::as_str) {
                        Some(description) => format!(": {description}"),
                        None => String::new(),
                    };
                    match error.as_ref().and_then(Value::as_str) {
                        Some("authorization_pending") => OAuthDeviceCodePollResult::Pending,
                        Some("slow_down") => OAuthDeviceCodePollResult::SlowDown {
                            interval_seconds: json
                                .get("interval")
                                .and_then(Value::as_f64)
                                .filter(|interval| *interval > 0.0),
                        },
                        Some("expired_token") => OAuthDeviceCodePollResult::Failed {
                            message:
                                "Kimi Code device authorization expired. Please restart login."
                                    .to_string(),
                        },
                        Some("access_denied") => OAuthDeviceCodePollResult::Failed {
                            message: "Kimi Code login was denied.".to_string(),
                        },
                        other => OAuthDeviceCodePollResult::Failed {
                            message: format!(
                                "Kimi Code device token request failed (status {}){}",
                                response.status,
                                match other {
                                    Some(error) => format!(": {error}{description}"),
                                    None => String::new(),
                                }
                            ),
                        },
                    }
                }
            },
        })
        .await
    }

    async fn refresh_token(
        &self,
        refresh_token: &str,
        signal: &CancellationToken,
    ) -> Result<TokenResponse, String> {
        let oauth_host = self.oauth_host();
        let mut last_error: Option<String> = None;
        for attempt in 0..=REFRESH_MAX_RETRIES {
            if attempt > 0 {
                let delay_ms = 1000u64 * (1u64 << (attempt - 1));
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                    _ = signal.cancelled() => {
                        return Err("Kimi Code token refresh aborted".to_string());
                    }
                }
            }
            if signal.is_cancelled() {
                return Err("Kimi Code token refresh aborted".to_string());
            }

            let response = self
                .post_json(
                    &format!("{oauth_host}/api/oauth/token"),
                    &[
                        ("client_id", CLIENT_ID),
                        ("grant_type", "refresh_token"),
                        ("refresh_token", refresh_token),
                    ],
                    signal,
                )
                .await;
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    last_error = Some(error);
                    continue;
                }
            };

            if (200..300).contains(&response.status) {
                return parse_token_response(response.json.as_ref(), "refresh", self.now());
            }

            // Unauthorized: the stored credential is dead; Models clears it
            // and prompts re-login.
            let error = response
                .json
                .as_ref()
                .and_then(|json| json.get("error"))
                .and_then(Value::as_str);
            if response.status == 401 || response.status == 403 || error == Some("invalid_grant") {
                let description = match response
                    .json
                    .as_ref()
                    .and_then(|json| json.get("error_description"))
                    .and_then(Value::as_str)
                {
                    Some(description) => format!(": {description}"),
                    None => String::new(),
                };
                return Err(format!(
                    "Kimi Code token refresh unauthorized (status {}){description}",
                    response.status
                ));
            }

            if is_retryable_refresh_failure(response.status) && attempt < REFRESH_MAX_RETRIES {
                last_error = Some(format!(
                    "Kimi Code token refresh failed with status {}",
                    response.status
                ));
                continue;
            }

            let text = response
                .json
                .map(|json| json.to_string())
                .unwrap_or_else(|| "null".to_string());
            return Err(format!(
                "Kimi Code token refresh failed with status {}{}",
                response.status,
                if text.is_empty() {
                    String::new()
                } else {
                    format!(": {text}")
                }
            ));
        }

        Err(last_error.unwrap_or_else(|| "Kimi Code token refresh failed".to_string()))
    }
}

/// The shared `kimiCodingOAuth` value.
pub fn kimi_coding_oauth() -> Arc<dyn OAuthAuth> {
    Arc::new(KimiCodingOAuth::default())
}

impl OAuthAuth for KimiCodingOAuth {
    fn name(&self) -> &str {
        "Kimi Code (subscription)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with Kimi Code")
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
    ) -> crate::ai::auth::types::AuthFuture<Result<OAuthCredential, AuthStorageError>> {
        let this = self.clone();
        Box::pin(async move {
            let signal = interaction.signal().unwrap_or_default();
            let oauth_host = this.oauth_host();
            let device = this
                .start_device_authorization(&oauth_host, &signal)
                .await
                .map_err(AuthStorageError)?;
            interaction.notify(AuthEvent::DeviceCode {
                user_code: device.user_code.clone(),
                verification_uri: device.verification_uri_complete.clone(),
                interval_seconds: Some(device.interval_seconds as u64),
                expires_in_seconds: Some(device.expires_in_seconds as u64),
            });
            let token = this
                .poll_for_token(&oauth_host, &device, &signal)
                .await
                .map_err(AuthStorageError)?;
            Ok(OAuthCredential {
                access: token.access,
                refresh: token.refresh,
                expires: token.expires,
                extra: Default::default(),
            })
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
            let token = this
                .refresh_token(&refresh_token, &signal)
                .await
                .map_err(AuthStorageError)?;
            Ok(OAuthCredential {
                access: token.access,
                refresh: token.refresh,
                expires: token.expires,
                extra: Default::default(),
            })
        })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> crate::ai::auth::types::AuthFuture<Result<ModelAuth, AuthStorageError>> {
        let credential = credential.clone();
        Box::pin(async move {
            Ok(ModelAuth {
                headers: Some(
                    [(
                        "Authorization".to_string(),
                        Some(format!("Bearer {}", credential.access)),
                    )]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            })
        })
    }
}

struct JsonResponse {
    status: u16,
    text: String,
    json: Option<Value>,
}

#[derive(Clone)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    interval_seconds: f64,
    expires_in_seconds: f64,
}

struct TokenResponse {
    access: String,
    refresh: String,
    expires: i64,
}

/// The verification URI is opened in the user's browser; only http(s) URLs
/// are trusted.
fn trusted_http_url(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    match url.scheme() {
        "https" | "http" => Some(url.to_string()),
        _ => None,
    }
}

fn parse_token_response(
    json: Option<&Value>,
    operation: &str,
    now_ms: i64,
) -> Result<TokenResponse, String> {
    let json = json.unwrap_or(&Value::Null);
    let access_token = json.get("access_token").and_then(Value::as_str);
    let refresh_token = json.get("refresh_token").and_then(Value::as_str);
    let expires_in = json.get("expires_in").and_then(Value::as_f64);
    let valid = matches!(access_token, Some(access) if !access.is_empty())
        && matches!(refresh_token, Some(refresh) if !refresh.is_empty())
        && matches!(expires_in, Some(expires) if expires.is_finite() && expires > 0.0);
    if !valid {
        return Err(format!(
            "Kimi Code token {operation} response missing fields: {json}"
        ));
    }
    Ok(TokenResponse {
        access: access_token.unwrap().to_string(),
        refresh: refresh_token.unwrap().to_string(),
        expires: now_ms + (expires_in.unwrap() * 1000.0) as i64,
    })
}

fn is_retryable_refresh_failure(status: u16) -> bool {
    status == 429 || status >= 500
}
