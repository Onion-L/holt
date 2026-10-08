//! Port of `pi-core/ai/src/auth/oauth/github-copilot.ts`: the GitHub Copilot
//! OAuth device flow.
//!
//! The client id is stored base64-encoded upstream; it is inlined decoded
//! here. Rate-limit retries reproduce the TypeScript budget arithmetic with
//! an injectable clock for determinism.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::ai::auth::oauth::NowMs;
use crate::ai::auth::oauth::device_code::{
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, poll_oauth_device_code_flow,
};
use crate::ai::auth::resolve::now_millis;
use crate::ai::auth::types::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthPromptKind, AuthStorageError, ModelAuth, OAuthAuth,
    OAuthCredential,
};
use crate::ai::types::FetchFunction;
use crate::ai::utils::http::{HttpBody, HttpMethod, HttpRequest, collect_text};

const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const COPILOT_API_VERSION: &str = "2026-06-01";
const PER_REQUEST_TIMEOUT_MS: u64 = 5_000;
/// Refresh slightly before the reported expiry.
const EXPIRY_SKEW_MS: i64 = 5 * 60 * 1000;
const INDIVIDUAL_BASE_URL: &str = "https://api.individual.githubcopilot.com";

fn copilot_headers() -> Vec<(String, String)> {
    vec![
        (
            "User-Agent".to_string(),
            "GitHubCopilotChat/0.35.0".to_string(),
        ),
        ("Editor-Version".to_string(), "vscode/1.107.0".to_string()),
        (
            "Editor-Plugin-Version".to_string(),
            "copilot-chat/0.35.0".to_string(),
        ),
        (
            "Copilot-Integration-Id".to_string(),
            "vscode-chat".to_string(),
        ),
    ]
}

/// Port of `normalizeDomain`.
pub fn normalize_domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    url::Url::parse(&with_scheme)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
}

fn get_urls(domain: &str) -> (String, String, String) {
    (
        format!("https://{domain}/login/device/code"),
        format!("https://{domain}/login/oauth/access_token"),
        format!("https://api.{domain}/copilot_internal/v2/token"),
    )
}

/// Parse the proxy-ep from a Copilot token and convert to an API base URL.
/// Token format: `tid=...;exp=...;proxy-ep=proxy.individual.githubcopilot.com;...`
fn get_base_url_from_token(token: &str) -> Option<String> {
    let proxy_host = token
        .split(";")
        .find_map(|part| part.trim_start().strip_prefix("proxy-ep="))?;
    // `proxy.xxx` converts to `api.xxx`.
    let api_host = match proxy_host.strip_prefix("proxy.") {
        Some(host) => format!("api.{host}"),
        None => proxy_host.to_string(),
    };
    Some(format!("https://{api_host}"))
}

/// Port of `getGitHubCopilotBaseUrl`.
pub fn get_github_copilot_base_url(token: Option<&str>, enterprise_domain: Option<&str>) -> String {
    if let Some(token) = token
        && let Some(url_from_token) = get_base_url_from_token(token)
    {
        return url_from_token;
    }
    if let Some(enterprise_domain) = enterprise_domain {
        return format!("https://copilot-api.{enterprise_domain}");
    }
    INDIVIDUAL_BASE_URL.to_string()
}

/// The parsed `availableModelIds` extension of a stored credential.
pub fn available_model_ids(credential: &OAuthCredential) -> Vec<String> {
    credential
        .extra
        .get("availableModelIds")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Port of `copilotEnterpriseDomain`.
pub fn copilot_enterprise_domain(credential: &OAuthCredential) -> Option<String> {
    let domain = credential
        .extra
        .get("enterpriseUrl")
        .and_then(Value::as_str)?;
    if domain.is_empty() {
        return None;
    }
    normalize_domain(domain)
}

/// Port of `parseGitHubCopilotModelCatalog`.
pub fn parse_github_copilot_model_catalog(
    raw: &Value,
    allow_policy_fallback: bool,
) -> Result<CopilotModelCatalog, String> {
    let Some(data) = raw.get("data").and_then(Value::as_array) else {
        return Err("Invalid Copilot models response".to_string());
    };

    struct AccountModel {
        id: String,
        picker_enabled: bool,
        policy_state: Option<String>,
    }
    let account_models: Vec<AccountModel> = data
        .iter()
        .filter_map(|raw_item| {
            let id = raw_item.get("id")?.as_str()?.to_string();
            // Models without tool support are unusable by pi.
            if raw_item
                .pointer("/capabilities/supports/tool_calls")
                .is_some_and(|tool_calls| tool_calls == &Value::Bool(false))
            {
                return None;
            }
            Some(AccountModel {
                id,
                picker_enabled: raw_item.get("model_picker_enabled") == Some(&Value::Bool(true)),
                policy_state: raw_item
                    .pointer("/policy/state")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect();

    let picker_model_ids: Vec<String> = account_models
        .iter()
        .filter(|model| model.picker_enabled && model.policy_state.as_deref() != Some("disabled"))
        .map(|model| model.id.clone())
        .collect();
    let use_policy_fallback = allow_policy_fallback && picker_model_ids.is_empty();
    let available_model_ids = if !picker_model_ids.is_empty() || !allow_policy_fallback {
        picker_model_ids
    } else {
        account_models
            .iter()
            .filter(|model| model.policy_state.as_deref() == Some("enabled"))
            .map(|model| model.id.clone())
            .collect()
    };
    let catalog = crate::ai::models_generated::models()
        .get("github-copilot")
        .map(|models| models.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let policy_model_ids: Vec<String> = account_models
        .iter()
        .filter(|model| {
            model.policy_state.as_deref() == Some("unconfigured")
                && catalog.contains(&model.id)
                && (model.picker_enabled || use_policy_fallback)
        })
        .map(|model| model.id.clone())
        .collect();
    Ok(CopilotModelCatalog {
        available_model_ids,
        policy_model_ids,
    })
}

/// Port of the `parseGitHubCopilotModelCatalog` result.
pub struct CopilotModelCatalog {
    pub available_model_ids: Vec<String>,
    pub policy_model_ids: Vec<String>,
}

/// Port of the retry policy shape used by the rate-limited helpers.
#[derive(Clone, Copy)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub max_elapsed_ms: i64,
}

struct FetchedResponse {
    status: u16,
    headers: Vec<(String, String)>,
    text: String,
}

impl FetchedResponse {
    fn is_ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// TypeScript formats failures as `${status} ${statusText}: ${text}`; the
    /// Rust transport carries no reason phrase, matching the empty default.
    fn failure_message(&self) -> String {
        format!("{} : {}", self.status, self.text)
    }
}

/// The GitHub Copilot OAuth flow. Clone-cheap.
#[derive(Clone)]
pub struct GitHubCopilotOAuth {
    fetch: FetchFunction,
    now_ms: NowMs,
}

impl Default for GitHubCopilotOAuth {
    fn default() -> Self {
        Self {
            fetch: crate::ai::utils::reqwest_fetch::default_fetch(),
            now_ms: Arc::new(now_millis),
        }
    }
}

impl GitHubCopilotOAuth {
    pub fn new(fetch: FetchFunction, now_ms: NowMs) -> Self {
        Self { fetch, now_ms }
    }

    fn now(&self) -> i64 {
        (self.now_ms)()
    }

    async fn raw_fetch(
        &self,
        url: &str,
        method: HttpMethod,
        headers: Vec<(String, String)>,
        body: HttpBody,
        timeout_ms: u64,
        signal: &CancellationToken,
    ) -> Result<FetchedResponse, String> {
        let request = HttpRequest {
            signal: None,
            method,
            url: url.to_string(),
            headers,
            body,
        };
        let fetch = Arc::clone(&self.fetch);
        let mut response =
            tokio::time::timeout(Duration::from_millis(timeout_ms), fetch.fetch(request))
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
        let headers = std::mem::take(&mut response.headers);
        let text = collect_text(response).await;
        Ok(FetchedResponse {
            status,
            headers,
            text,
        })
    }

    /// Port of `fetchWithRateLimitRetry`.
    async fn fetch_with_rate_limit_retry(
        &self,
        url: &str,
        method: HttpMethod,
        headers: Vec<(String, String)>,
        body: HttpBody,
        signal: &CancellationToken,
        retry_policy: RetryPolicy,
    ) -> Result<FetchedResponse, String> {
        let budget_active = retry_policy.max_retries > 0 && retry_policy.max_elapsed_ms > 0;
        let retry_deadline = budget_active.then(|| self.now() + retry_policy.max_elapsed_ms);
        let mut retry: u32 = 0;
        loop {
            let timeout_ms = match retry_deadline {
                Some(deadline) => {
                    let remaining = (deadline - self.now()).max(0) as u64;
                    remaining.min(PER_REQUEST_TIMEOUT_MS)
                }
                None => PER_REQUEST_TIMEOUT_MS,
            };
            let response = self
                .raw_fetch(
                    url,
                    method,
                    headers.clone(),
                    body.clone(),
                    timeout_ms,
                    signal,
                )
                .await?;
            if response.status != 429 || retry == retry_policy.max_retries {
                return Ok(response);
            }

            let retry_after = response
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
                .map(|(_, value)| value.clone());
            let mut delay_ms: f64 = 500.0 * 2f64.powi(retry as i32);
            if let Some(retry_after) = retry_after {
                delay_ms = match retry_after.parse::<f64>() {
                    Ok(seconds) => seconds * 1000.0,
                    Err(_) => match httpdate::parse_http_date(&retry_after) {
                        Ok(date) => date
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|duration| duration.as_millis() as f64 - self.now() as f64)
                            .unwrap_or(f64::NAN),
                        Err(_) => f64::NAN,
                    },
                };
                if !delay_ms.is_finite() {
                    return Ok(response);
                }
            }
            let delay_ms = delay_ms.max(0.0) as u64;
            if let Some(deadline) = retry_deadline
                && delay_ms as i64 >= deadline - self.now()
            {
                return Ok(response);
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                _ = signal.cancelled() => return Err("Login cancelled".to_string()),
            }
            retry += 1;
        }
    }

    /// Port of `fetchJson` for the GitHub endpoints.
    async fn fetch_json(
        &self,
        url: &str,
        method: HttpMethod,
        headers: Vec<(String, String)>,
        body: HttpBody,
        signal: &CancellationToken,
    ) -> Result<Value, String> {
        let response = self
            .raw_fetch(url, method, headers, body, PER_REQUEST_TIMEOUT_MS, signal)
            .await?;
        if !response.is_ok() {
            return Err(response.failure_message());
        }
        serde_json::from_str(&response.text).map_err(|error| error.to_string())
    }

    async fn start_device_flow(
        &self,
        domain: &str,
        signal: &CancellationToken,
    ) -> Result<DeviceCodeResponse, String> {
        let (device_code_url, _, _) = get_urls(domain);
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", CLIENT_ID)
            .append_pair("scope", "read:user")
            .finish();
        let headers = vec![
            ("accept".to_string(), "application/json".to_string()),
            (
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            ),
            (
                "User-Agent".to_string(),
                "GitHubCopilotChat/0.35.0".to_string(),
            ),
        ];
        let data = self
            .fetch_json(
                &device_code_url,
                HttpMethod::Post,
                headers,
                HttpBody::Text(body),
                signal,
            )
            .await?;

        if !data.is_object() {
            return Err("Invalid device code response".to_string());
        }
        let string_field = |name: &str| data.get(name).and_then(Value::as_str).map(str::to_string);
        let number_field = |name: &str| data.get(name).and_then(Value::as_f64);
        let device_code = string_field("device_code");
        let user_code = string_field("user_code");
        let verification_uri = string_field("verification_uri");
        let interval = number_field("interval");
        let expires_in = number_field("expires_in");
        let valid_fields = matches!(data.get("interval"), None | Some(Value::Number(_)));
        if device_code.is_none()
            || user_code.is_none()
            || verification_uri.is_none()
            || !valid_fields
            || expires_in.is_none()
        {
            return Err("Invalid device code response fields".to_string());
        }

        // The verification URI is opened in the user's browser; force it to
        // be an http(s) URL and normalize it before it reaches the UI.
        let verification_uri = match url::Url::parse(&verification_uri.unwrap()) {
            Ok(parsed) if matches!(parsed.scheme(), "https" | "http") => parsed.to_string(),
            _ => return Err("Untrusted verification_uri in device code response".to_string()),
        };

        Ok(DeviceCodeResponse {
            device_code: device_code.unwrap(),
            user_code: user_code.unwrap(),
            verification_uri,
            interval,
            expires_in: expires_in.unwrap(),
        })
    }

    async fn poll_for_github_access_token(
        &self,
        domain: &str,
        device: &DeviceCodeResponse,
        signal: &CancellationToken,
    ) -> Result<String, String> {
        let (_, access_token_url, _) = get_urls(domain);
        let this = self.clone();
        let access_token_url = Arc::new(access_token_url);
        let device = Arc::new(device.clone());
        let signal = signal.clone();
        poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
            interval_seconds: device.interval,
            expires_in_seconds: Some(device.expires_in),
            wait_before_first_poll: true,
            signal: signal.clone(),
            poll: move || {
                let this = this.clone();
                let access_token_url = Arc::clone(&access_token_url);
                let device = Arc::clone(&device);
                let signal = signal.clone();
                async move {
                    let body = url::form_urlencoded::Serializer::new(String::new())
                        .append_pair("client_id", CLIENT_ID)
                        .append_pair("device_code", device.device_code.as_str())
                        .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:device_code")
                        .finish();
                    let headers = vec![
                        ("accept".to_string(), "application/json".to_string()),
                        (
                            "content-type".to_string(),
                            "application/x-www-form-urlencoded".to_string(),
                        ),
                        (
                            "User-Agent".to_string(),
                            "GitHubCopilotChat/0.35.0".to_string(),
                        ),
                    ];
                    let raw = match this
                        .fetch_json(
                            &access_token_url,
                            HttpMethod::Post,
                            headers,
                            HttpBody::Text(body),
                            &signal,
                        )
                        .await
                    {
                        Ok(raw) => raw,
                        Err(message) => return OAuthDeviceCodePollResult::Failed { message },
                    };

                    if let Some(access_token) = raw.get("access_token").and_then(Value::as_str) {
                        return OAuthDeviceCodePollResult::Complete(access_token.to_string());
                    }

                    if let Some(error) = raw.get("error").and_then(Value::as_str) {
                        let description = raw
                            .get("error_description")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        return match error {
                            "authorization_pending" => OAuthDeviceCodePollResult::Pending,
                            "slow_down" => OAuthDeviceCodePollResult::SlowDown {
                                interval_seconds: raw
                                    .get("interval")
                                    .and_then(Value::as_f64)
                                    .filter(|interval| interval.is_finite()),
                            },
                            error => {
                                let description_suffix = description
                                    .as_deref()
                                    .map(|description| format!(": {description}"))
                                    .unwrap_or_default();
                                OAuthDeviceCodePollResult::Failed {
                                    message: format!(
                                        "Device flow failed: {error}{description_suffix}"
                                    ),
                                }
                            }
                        };
                    }

                    OAuthDeviceCodePollResult::Failed {
                        message: "Invalid device token response".to_string(),
                    }
                }
            },
        })
        .await
    }

    /// Port of `refreshGitHubCopilotAccessToken`: exchange the GitHub access
    /// token for a Copilot token.
    async fn refresh_github_copilot_access_token(
        &self,
        refresh_token: &str,
        enterprise_domain: Option<&str>,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        let domain = enterprise_domain.unwrap_or("github.com");
        let (_, _, copilot_token_url) = get_urls(domain);
        let mut headers = vec![
            ("accept".to_string(), "application/json".to_string()),
            (
                "authorization".to_string(),
                format!("Bearer {refresh_token}"),
            ),
        ];
        headers.extend(copilot_headers());

        let raw = self
            .fetch_json(
                &copilot_token_url,
                HttpMethod::Get,
                headers,
                HttpBody::Empty,
                signal,
            )
            .await?;
        if !raw.is_object() {
            return Err("Invalid Copilot token response".to_string());
        }
        let Some(Value::String(token)) = raw.get("token") else {
            return Err("Invalid Copilot token response fields".to_string());
        };
        let Some(expires_at) = raw.get("expires_at").and_then(Value::as_f64) else {
            return Err("Invalid Copilot token response fields".to_string());
        };

        let mut extra = Map::new();
        if let Some(enterprise_domain) = enterprise_domain {
            extra.insert(
                "enterpriseUrl".to_string(),
                Value::String(enterprise_domain.to_string()),
            );
        }
        Ok(OAuthCredential {
            refresh: refresh_token.to_string(),
            access: token.clone(),
            expires: (expires_at * 1000.0) as i64 - EXPIRY_SKEW_MS,
            extra,
        })
    }

    /// Port of `fetchGitHubCopilotModels`.
    async fn fetch_github_copilot_models(
        &self,
        copilot_token: &str,
        enterprise_domain: Option<&str>,
        signal: &CancellationToken,
        retry_policy: RetryPolicy,
    ) -> Result<CopilotModelCatalog, String> {
        let base_url = get_github_copilot_base_url(Some(copilot_token), enterprise_domain);
        // Some Individual accounts return false for every picker flag despite
        // explicit enabled policies. Limit the fallback to that endpoint so
        // other account types keep strict picker semantics.
        let allow_policy_fallback = base_url == INDIVIDUAL_BASE_URL;
        let mut headers = vec![
            ("accept".to_string(), "application/json".to_string()),
            (
                "authorization".to_string(),
                format!("Bearer {copilot_token}"),
            ),
        ];
        headers.extend(copilot_headers());
        headers.push((
            "X-GitHub-Api-Version".to_string(),
            COPILOT_API_VERSION.to_string(),
        ));

        let response = self
            .fetch_with_rate_limit_retry(
                &format!("{base_url}/models"),
                HttpMethod::Get,
                headers,
                HttpBody::Empty,
                signal,
                retry_policy,
            )
            .await?;
        if !response.is_ok() {
            return Err(response.failure_message());
        }
        let raw: Value = serde_json::from_str(&response.text).map_err(|error| error.to_string())?;
        parse_github_copilot_model_catalog(&raw, allow_policy_fallback)
    }

    /// Port of `enableGitHubCopilotModel`: policy updates are best effort.
    async fn enable_github_copilot_model(
        &self,
        token: &str,
        model_id: &str,
        enterprise_domain: Option<&str>,
        signal: &CancellationToken,
    ) -> Result<bool, String> {
        let base_url = get_github_copilot_base_url(Some(token), enterprise_domain);
        let url = format!("{base_url}/models/{model_id}/policy");
        let mut headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("authorization".to_string(), format!("Bearer {token}")),
        ];
        headers.extend(copilot_headers());
        headers.push(("openai-intent".to_string(), "chat-policy".to_string()));
        headers.push(("x-interaction-type".to_string(), "chat-policy".to_string()));

        let response = match self
            .fetch_with_rate_limit_retry(
                &url,
                HttpMethod::Post,
                headers,
                HttpBody::Json(json!({"state": "enabled"})),
                signal,
                RetryPolicy {
                    max_retries: 2,
                    max_elapsed_ms: 5_000,
                },
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                if signal.is_cancelled() {
                    return Err(error);
                }
                return Ok(false);
            }
        };
        if response.status == 429 {
            return Err(response.failure_message());
        }
        Ok(response.is_ok())
    }

    /// Port of `enableGitHubCopilotModels`: exhausted rate limiting stops the
    /// batch.
    async fn enable_github_copilot_models(
        &self,
        token: &str,
        model_ids: &[String],
        enterprise_domain: Option<&str>,
        signal: &CancellationToken,
    ) -> Result<Vec<String>, String> {
        let mut enabled_model_ids = Vec::new();
        for model_id in model_ids {
            match self
                .enable_github_copilot_model(token, model_id, enterprise_domain, signal)
                .await
            {
                Ok(true) => enabled_model_ids.push(model_id.clone()),
                Ok(false) => {}
                Err(error) => {
                    if signal.is_cancelled() {
                        return Err(error);
                    }
                    break;
                }
            }
        }
        Ok(enabled_model_ids)
    }

    async fn login_with_interaction(
        &self,
        interaction: Arc<dyn AuthInteraction>,
    ) -> Result<OAuthCredential, String> {
        let signal = interaction.signal().unwrap_or_default();
        let input = interaction
            .prompt(AuthPrompt {
                signal: None,
                kind: AuthPromptKind::Text {
                    message: "GitHub Enterprise URL/domain (blank for github.com)".to_string(),
                    placeholder: Some("company.ghe.com".to_string()),
                },
            })
            .await
            .map_err(|error| error.0)?;
        if signal.is_cancelled() {
            return Err("Login cancelled".to_string());
        }

        let trimmed = input.trim().to_string();
        let enterprise_domain = normalize_domain(&input);
        if !trimmed.is_empty() && enterprise_domain.is_none() {
            return Err("Invalid GitHub Enterprise URL/domain".to_string());
        }
        let domain = enterprise_domain
            .clone()
            .unwrap_or_else(|| "github.com".to_string());

        let device = self.start_device_flow(&domain, &signal).await?;
        interaction.notify(AuthEvent::DeviceCode {
            user_code: device.user_code.clone(),
            verification_uri: device.verification_uri.clone(),
            interval_seconds: device.interval.map(|interval| interval as u64),
            expires_in_seconds: Some(device.expires_in as u64),
        });

        let github_access_token = self
            .poll_for_github_access_token(&domain, &device, &signal)
            .await?;
        let mut credentials = self
            .refresh_github_copilot_access_token(
                &github_access_token,
                enterprise_domain.as_deref(),
                &signal,
            )
            .await?;
        let models = self
            .fetch_github_copilot_models(
                &credentials.access,
                enterprise_domain.as_deref(),
                &signal,
                RetryPolicy {
                    max_retries: 2,
                    max_elapsed_ms: 5_000,
                },
            )
            .await?;
        let mut enabled_model_ids = Vec::new();
        if !models.policy_model_ids.is_empty() {
            interaction.notify(AuthEvent::Progress {
                message: "Enabling models...".to_string(),
            });
            enabled_model_ids = self
                .enable_github_copilot_models(
                    &credentials.access,
                    &models.policy_model_ids,
                    enterprise_domain.as_deref(),
                    &signal,
                )
                .await?;
        }

        // Union preserving first occurrence, like `[...new Set([...])]`.
        let mut available: Vec<String> = models.available_model_ids;
        for id in enabled_model_ids {
            if !available.contains(&id) {
                available.push(id);
            }
        }
        credentials.extra.insert(
            "availableModelIds".to_string(),
            Value::Array(available.into_iter().map(Value::String).collect()),
        );
        Ok(credentials)
    }

    /// Port of `refreshGitHubCopilotToken`.
    async fn refresh_github_copilot_token(
        &self,
        refresh_token: &str,
        enterprise_domain: Option<&str>,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        let mut credentials = self
            .refresh_github_copilot_access_token(refresh_token, enterprise_domain, signal)
            .await?;
        let catalog = self
            .fetch_github_copilot_models(
                &credentials.access,
                enterprise_domain,
                signal,
                RetryPolicy {
                    max_retries: 0,
                    max_elapsed_ms: 0,
                },
            )
            .await?;
        credentials.extra.insert(
            "availableModelIds".to_string(),
            Value::Array(
                catalog
                    .available_model_ids
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            ),
        );
        Ok(credentials)
    }
}

#[derive(Clone)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<f64>,
    expires_in: f64,
}

/// The shared `githubCopilotOAuth` value.
pub fn github_copilot_oauth() -> Arc<dyn OAuthAuth> {
    Arc::new(GitHubCopilotOAuth::default())
}

impl OAuthAuth for GitHubCopilotOAuth {
    fn name(&self) -> &str {
        "GitHub Copilot"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
    ) -> crate::ai::auth::types::AuthFuture<Result<OAuthCredential, AuthStorageError>> {
        let this = self.clone();
        Box::pin(async move {
            this.login_with_interaction(interaction)
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
        let enterprise_domain = copilot_enterprise_domain(credential);
        Box::pin(async move {
            this.refresh_github_copilot_token(&refresh_token, enterprise_domain.as_deref(), &signal)
                .await
                .map_err(AuthStorageError)
        })
    }

    /// Derive the credential-specific proxy endpoint for each request.
    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> crate::ai::auth::types::AuthFuture<Result<ModelAuth, AuthStorageError>> {
        let credential = credential.clone();
        Box::pin(async move {
            Ok(ModelAuth {
                api_key: Some(credential.access.clone()),
                base_url: Some(get_github_copilot_base_url(
                    Some(&credential.access),
                    copilot_enterprise_domain(&credential).as_deref(),
                )),
                ..Default::default()
            })
        })
    }
}
