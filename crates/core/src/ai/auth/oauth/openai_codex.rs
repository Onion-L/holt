//! Port of `pi-core/ai/src/auth/oauth/openai-codex.ts`: the OpenAI Codex
//! (ChatGPT OAuth) flow.
//!
//! The TypeScript module uses Node crypto/http for state and the OAuth
//! callback; the Rust port uses `getrandom` and a `tokio` TCP listener. The
//! `accountId` credential extension rides the extra map.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::ai::auth::oauth::NowMs;
use crate::ai::auth::oauth::device_code::{
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, poll_oauth_device_code_flow,
};
use crate::ai::auth::oauth::oauth_page::{oauth_error_html, oauth_success_html};
use crate::ai::auth::oauth::pkce::{PkcePair, generate_pkce};
use crate::ai::auth::resolve::now_millis;
use crate::ai::auth::types::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthPromptKind, AuthPromptOption, AuthStorageError,
    ModelAuth, OAuthAuth, OAuthCredential,
};
use crate::ai::types::{FetchFunction, ProviderEnv};
use crate::ai::utils::http::{HttpBody, HttpMethod, HttpRequest, collect_text};
use crate::ai::utils::provider_env::get_provider_env_value;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEVICE_CODE_TIMEOUT_SECONDS: f64 = 15.0 * 60.0;
const OPENAI_CODEX_BROWSER_LOGIN_METHOD: &str = "browser";
const OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD: &str = "device_code";
const SCOPE: &str = "openid profile email offline_access";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";

/// The OpenAI Codex OAuth flow. Clone-cheap.
#[derive(Clone)]
pub struct OpenAICodexOAuth {
    fetch: FetchFunction,
    now_ms: NowMs,
    env: ProviderEnv,
}

impl Default for OpenAICodexOAuth {
    fn default() -> Self {
        Self {
            fetch: crate::ai::utils::reqwest_fetch::default_fetch(),
            now_ms: Arc::new(now_millis),
            env: ProviderEnv::new(),
        }
    }
}

impl OpenAICodexOAuth {
    pub fn new(fetch: FetchFunction, now_ms: NowMs, env: ProviderEnv) -> Self {
        Self { fetch, now_ms, env }
    }

    fn now(&self) -> i64 {
        (self.now_ms)()
    }

    fn callback_host(&self) -> String {
        get_provider_env_value("PI_OAUTH_CALLBACK_HOST", Some(&self.env))
            .unwrap_or_else(|| "127.0.0.1".to_string())
    }

    /// Port of `fetchWithLoginCancellation`.
    async fn fetch_response(
        &self,
        url: &str,
        method: HttpMethod,
        content_type: &str,
        body: HttpBody,
        signal: &CancellationToken,
    ) -> Result<(u16, String), String> {
        let request = HttpRequest {
            signal: None,
            method,
            url: url.to_string(),
            headers: vec![("content-type".to_string(), content_type.to_string())],
            body,
        };
        let response = Arc::clone(&self.fetch)
            .fetch(request)
            .await
            .map_err(|error| {
                if signal.is_cancelled() {
                    "Login cancelled".to_string()
                } else {
                    error.to_string()
                }
            })?;
        let status = response.status;
        Ok((status, collect_text(response).await))
    }

    /// Port of `readTokenResponse`.
    fn read_token_response(
        &self,
        status: u16,
        text: &str,
        operation: &str,
    ) -> Result<OAuthToken, String> {
        if !(200..300).contains(&status) {
            let detail = if text.is_empty() { "" } else { text };
            return Err(format!(
                "OpenAI Codex token {operation} failed ({status}): {detail}"
            ));
        }
        let parsed: Value = serde_json::from_str(text)
            .map_err(|_| format!("OpenAI Codex token {operation} response missing fields: null"))?;
        let access = parsed.get("access_token").and_then(Value::as_str);
        let refresh = parsed.get("refresh_token").and_then(Value::as_str);
        let expires_in = parsed.get("expires_in").and_then(Value::as_f64);
        let (Some(access), Some(refresh), Some(expires_in)) = (access, refresh, expires_in) else {
            return Err(format!(
                "OpenAI Codex token {operation} response missing fields: {parsed}"
            ));
        };
        Ok(OAuthToken {
            access: access.to_string(),
            refresh: refresh.to_string(),
            expires: self.now() + (expires_in * 1000.0) as i64,
        })
    }

    async fn exchange_authorization_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        signal: &CancellationToken,
    ) -> Result<OAuthToken, String> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("code", code)
            .append_pair("code_verifier", verifier)
            .append_pair("redirect_uri", redirect_uri)
            .finish();
        let (status, text) = self
            .fetch_response(
                TOKEN_URL,
                HttpMethod::Post,
                "application/x-www-form-urlencoded",
                HttpBody::Text(body),
                signal,
            )
            .await?;
        self.read_token_response(status, &text, "exchange")
    }

    async fn refresh_access_token(
        &self,
        refresh_token: &str,
        signal: &CancellationToken,
    ) -> Result<OAuthToken, String> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "refresh_token")
            .append_pair("refresh_token", refresh_token)
            .append_pair("client_id", CLIENT_ID)
            .finish();
        let (status, text) = self
            .fetch_response(
                TOKEN_URL,
                HttpMethod::Post,
                "application/x-www-form-urlencoded",
                HttpBody::Text(body),
                signal,
            )
            .await
            .map_err(|error| format!("OpenAI Codex token refresh error: {error}"))?;
        self.read_token_response(status, &text, "refresh")
    }

    async fn start_device_auth(
        &self,
        signal: &CancellationToken,
    ) -> Result<DeviceAuthInfo, String> {
        let (status, text) = self
            .fetch_response(
                DEVICE_USER_CODE_URL,
                HttpMethod::Post,
                "application/json",
                HttpBody::Json(json!({ "client_id": CLIENT_ID })),
                signal,
            )
            .await?;

        if !(200..300).contains(&status) {
            if status == 404 {
                return Err("OpenAI Codex device code login is not enabled for this server. Use browser login or verify the server URL.".to_string());
            }
            let suffix = if text.is_empty() {
                String::new()
            } else {
                format!(": {text}")
            };
            return Err(format!(
                "OpenAI Codex device code request failed with status {status}{suffix}"
            ));
        }

        let parsed: Value = serde_json::from_str(&text)
            .map_err(|_| "Invalid OpenAI Codex device code response: null".to_string())?;
        let interval = match parsed.get("interval") {
            Some(Value::String(interval)) => interval
                .trim()
                .parse::<f64>()
                .map(Some)
                .unwrap_or(Some(f64::NAN)),
            other => other.and_then(Value::as_f64),
        };
        let device_auth_id = parsed.get("device_auth_id").and_then(Value::as_str);
        let user_code = parsed.get("user_code").and_then(Value::as_str);
        match (device_auth_id, user_code, interval) {
            (Some(device_auth_id), Some(user_code), Some(interval))
                if interval.is_finite() && interval >= 0.0 =>
            {
                Ok(DeviceAuthInfo {
                    device_auth_id: device_auth_id.to_string(),
                    user_code: user_code.to_string(),
                    interval_seconds: interval,
                })
            }
            _ => Err(format!(
                "Invalid OpenAI Codex device code response: {parsed}"
            )),
        }
    }

    async fn poll_device_auth(
        &self,
        device: &DeviceAuthInfo,
        signal: &CancellationToken,
    ) -> Result<DeviceTokenSuccess, String> {
        let this = self.clone();
        let device = Arc::new(device.clone());
        let signal = signal.clone();
        poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
            interval_seconds: Some(device.interval_seconds),
            expires_in_seconds: Some(DEVICE_CODE_TIMEOUT_SECONDS),
            wait_before_first_poll: false,
            signal: signal.clone(),
            poll: move || {
                let this = this.clone();
                let device = Arc::clone(&device);
                let signal = signal.clone();
                async move {
                    let (status, text) = match this
                        .fetch_response(
                            DEVICE_TOKEN_URL,
                            HttpMethod::Post,
                            "application/json",
                            HttpBody::Json(json!({
                                "device_auth_id": device.device_auth_id,
                                "user_code": device.user_code,
                            })),
                            &signal,
                        )
                        .await
                    {
                        Ok(response) => response,
                        Err(message) => return OAuthDeviceCodePollResult::Failed { message },
                    };

                    if (200..300).contains(&status) {
                        let parsed: Value = match serde_json::from_str(&text) {
                            Ok(parsed) => parsed,
                            Err(_) => {
                                return OAuthDeviceCodePollResult::Failed {
                                    message:
                                        "Invalid OpenAI Codex device auth token response: null"
                                            .to_string(),
                                };
                            }
                        };
                        let authorization_code =
                            parsed.get("authorization_code").and_then(Value::as_str);
                        let code_verifier = parsed.get("code_verifier").and_then(Value::as_str);
                        return match (authorization_code, code_verifier) {
                            (Some(authorization_code), Some(code_verifier)) => {
                                OAuthDeviceCodePollResult::Complete(DeviceTokenSuccess {
                                    authorization_code: authorization_code.to_string(),
                                    code_verifier: code_verifier.to_string(),
                                })
                            }
                            _ => OAuthDeviceCodePollResult::Failed {
                                message: format!(
                                    "Invalid OpenAI Codex device auth token response: {parsed}"
                                ),
                            },
                        };
                    }

                    if status == 403 || status == 404 {
                        return OAuthDeviceCodePollResult::Pending;
                    }

                    let error_code = serde_json::from_str::<Value>(&text)
                        .ok()
                        .as_ref()
                        .and_then(|parsed| parsed.get("error"))
                        .and_then(|error| match error {
                            Value::String(code) => Some(code.clone()),
                            Value::Object(object) => object
                                .get("code")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            _ => None,
                        });

                    if error_code.as_deref() == Some("deviceauth_authorization_pending") {
                        return OAuthDeviceCodePollResult::Pending;
                    }
                    if error_code.as_deref() == Some("slow_down") {
                        return OAuthDeviceCodePollResult::SlowDown {
                            interval_seconds: None,
                        };
                    }

                    let suffix = if text.is_empty() {
                        String::new()
                    } else {
                        format!(": {text}")
                    };
                    OAuthDeviceCodePollResult::Failed {
                        message: format!(
                            "OpenAI Codex device auth failed with status {status}{suffix}"
                        ),
                    }
                }
            },
        })
        .await
    }

    async fn login_device_code(
        &self,
        interaction: &Arc<dyn AuthInteraction>,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        let device = self.start_device_auth(signal).await?;
        interaction.notify(AuthEvent::DeviceCode {
            user_code: device.user_code.clone(),
            verification_uri: DEVICE_VERIFICATION_URI.to_string(),
            interval_seconds: Some(device.interval_seconds as u64),
            expires_in_seconds: Some(DEVICE_CODE_TIMEOUT_SECONDS as u64),
        });
        let code = self.poll_device_auth(&device, signal).await?;
        self.exchange_authorization_code_for_credentials(
            &code.authorization_code,
            &code.code_verifier,
            DEVICE_REDIRECT_URI,
            signal,
        )
        .await
    }

    async fn exchange_authorization_code_for_credentials(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        let token = self
            .exchange_authorization_code(code, verifier, redirect_uri, signal)
            .await?;
        credentials_from_token(&token)
    }

    async fn login_browser(
        &self,
        interaction: &Arc<dyn AuthInteraction>,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        let PkcePair {
            verifier,
            challenge,
        } = generate_pkce();
        let state = create_state();
        let authorize_url = format!(
            "{AUTHORIZE_URL}?{}",
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("response_type", "code")
                .append_pair("client_id", CLIENT_ID)
                .append_pair("redirect_uri", REDIRECT_URI)
                .append_pair("scope", SCOPE)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("state", &state)
                .append_pair("id_token_add_organizations", "true")
                .append_pair("codex_cli_simplified_flow", "true")
                .append_pair("originator", "pi")
                .finish()
        );

        let server = start_local_oauth_server(&state, signal, &self.callback_host()).await;
        let manual_abort = CancellationToken::new();
        let manual_outcome: Arc<Mutex<ManualOutcome>> = Arc::new(Mutex::new(ManualOutcome {
            input: None,
            error: None,
        }));

        interaction.notify(AuthEvent::AuthUrl {
            url: authorize_url,
            instructions: Some(
                "A browser window should open. Complete login to finish.".to_string(),
            ),
        });

        let manual_task = {
            let interaction = Arc::clone(interaction);
            let manual_abort = manual_abort.clone();
            let server = server.clone_handle();
            let manual_outcome = Arc::clone(&manual_outcome);
            tokio::spawn(async move {
                let prompt = AuthPrompt {
                    signal: Some(manual_abort),
                    kind: AuthPromptKind::ManualCode {
                        message: "Complete login in your browser, or paste the authorization code / redirect URL here:"
                            .to_string(),
                        placeholder: Some(REDIRECT_URI.to_string()),
                    },
                };
                match interaction.prompt(prompt).await {
                    Ok(input) => {
                        manual_outcome.lock().unwrap().input = Some(input);
                        server.cancel_wait();
                    }
                    Err(error) => {
                        manual_outcome.lock().unwrap().error = Some(error.0);
                        server.cancel_wait();
                    }
                }
            })
        };

        let outcome = async {
            let result = server.wait_for_code().await;
            let mut code: Option<String> = None;
            if let Some(error) = manual_outcome.lock().unwrap().error.clone() {
                return Err(error);
            }
            if let Some(callback_code) = result {
                code = Some(callback_code);
            } else if let Some(manual_code) = manual_outcome.lock().unwrap().input.clone() {
                let parsed = parse_authorization_input(&manual_code);
                if let Some(parsed_state) = &parsed.state
                    && parsed_state != &state
                {
                    return Err("State mismatch".to_string());
                }
                code = parsed.code;
            }

            if code.is_none() {
                let _ = manual_task.await;
                let (manual_code, manual_error) = {
                    let manual = manual_outcome.lock().unwrap();
                    (manual.input.clone(), manual.error.clone())
                };
                if let Some(error) = manual_error {
                    return Err(error);
                }
                if let Some(manual_code) = manual_code {
                    let parsed = parse_authorization_input(&manual_code);
                    if let Some(parsed_state) = &parsed.state
                        && parsed_state != &state
                    {
                        return Err("State mismatch".to_string());
                    }
                    code = parsed.code;
                }
            }

            let Some(code) = code else {
                return Err("Missing authorization code".to_string());
            };
            self.exchange_authorization_code_for_credentials(&code, &verifier, REDIRECT_URI, signal)
                .await
        }
        .await;

        manual_abort.cancel();
        server.close();
        outcome
    }

    async fn login_with_interaction(
        &self,
        interaction: Arc<dyn AuthInteraction>,
    ) -> Result<OAuthCredential, String> {
        let signal = interaction.signal().unwrap_or_default();
        let method = interaction
            .prompt(AuthPrompt {
                signal: None,
                kind: AuthPromptKind::Select {
                    message: "Select OpenAI Codex login method:".to_string(),
                    options: vec![
                        AuthPromptOption {
                            id: OPENAI_CODEX_BROWSER_LOGIN_METHOD.to_string(),
                            label: "Browser login (default)".to_string(),
                            description: None,
                        },
                        AuthPromptOption {
                            id: OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD.to_string(),
                            label: "Device code login (headless)".to_string(),
                            description: None,
                        },
                    ],
                },
            })
            .await
            .map_err(|error| error.0)?;

        match method.as_str() {
            OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD => {
                self.login_device_code(&interaction, &signal).await
            }
            OPENAI_CODEX_BROWSER_LOGIN_METHOD => self.login_browser(&interaction, &signal).await,
            other => Err(format!("Unknown OpenAI Codex login method: {other}")),
        }
    }
}

/// The shared `openaiCodexOAuth` value.
pub fn openai_codex_oauth() -> Arc<dyn OAuthAuth> {
    Arc::new(OpenAICodexOAuth::default())
}

impl OAuthAuth for OpenAICodexOAuth {
    fn name(&self) -> &str {
        "OpenAI (ChatGPT Plus/Pro)"
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
        Box::pin(async move {
            let token = this
                .refresh_access_token(&refresh_token, &signal)
                .await
                .map_err(AuthStorageError)?;
            credentials_from_token(&token).map_err(AuthStorageError)
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

struct OAuthToken {
    access: String,
    refresh: String,
    expires: i64,
}

#[derive(Clone)]
struct DeviceAuthInfo {
    device_auth_id: String,
    user_code: String,
    interval_seconds: f64,
}

struct DeviceTokenSuccess {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Default)]
struct ManualOutcome {
    input: Option<String>,
    error: Option<String>,
}

struct ParsedAuthorizationInput {
    code: Option<String>,
    state: Option<String>,
}

fn create_state() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("system RNG is always available");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_authorization_input(input: &str) -> ParsedAuthorizationInput {
    let value = input.trim();
    if value.is_empty() {
        return ParsedAuthorizationInput {
            code: None,
            state: None,
        };
    }

    if let Ok(url) = url::Url::parse(value) {
        let param = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string())
        };
        return ParsedAuthorizationInput {
            code: param("code"),
            state: param("state"),
        };
    }

    if let Some((code, state)) = value.split_once('#') {
        return ParsedAuthorizationInput {
            code: Some(code.to_string()),
            state: Some(state.to_string()),
        };
    }

    if value.contains("code=") {
        let param = |name: &str| {
            url::form_urlencoded::parse(value.as_bytes())
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string())
        };
        return ParsedAuthorizationInput {
            code: param("code"),
            state: param("state"),
        };
    }

    ParsedAuthorizationInput {
        code: Some(value.to_string()),
        state: None,
    }
}

/// Port of `decodeJwt`: standard base64 payload, unverified.
fn decode_jwt(token: &str) -> Option<Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let decoded = decode_base64(parts[1])?;
    serde_json::from_str(&decoded).ok()
}

fn decode_base64(input: &str) -> Option<String> {
    let mut buffer = Vec::new();
    let mut word = [0u8; 4];
    let mut count = 0;
    for character in input.bytes() {
        let value = match character {
            b'A'..=b'Z' => character - b'A',
            b'a'..=b'z' => character - b'a' + 26,
            b'0'..=b'9' => character - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        word[count] = value;
        count += 1;
        if count == 4 {
            buffer.push((word[0] << 2) | (word[1] >> 4));
            buffer.push((word[1] << 4) | (word[2] >> 2));
            buffer.push((word[2] << 6) | word[3]);
            count = 0;
        }
    }
    match count {
        2 => {
            buffer.push((word[0] << 2) | (word[1] >> 4));
        }
        3 => {
            buffer.push((word[0] << 2) | (word[1] >> 4));
            buffer.push((word[1] << 4) | (word[2] >> 2));
        }
        _ => {}
    }
    String::from_utf8(buffer).ok()
}

fn get_account_id(access_token: &str) -> Option<String> {
    let payload = decode_jwt(access_token)?;
    let account_id = payload
        .get(JWT_CLAIM_PATH)?
        .get("chatgpt_account_id")?
        .as_str()?;
    (!account_id.is_empty()).then(|| account_id.to_string())
}

fn credentials_from_token(token: &OAuthToken) -> Result<OAuthCredential, String> {
    let Some(account_id) = get_account_id(&token.access) else {
        return Err("Failed to extract accountId from token".to_string());
    };
    Ok(OAuthCredential {
        access: token.access.clone(),
        refresh: token.refresh.clone(),
        expires: token.expires,
        extra: [("accountId".to_string(), Value::String(account_id))]
            .into_iter()
            .collect(),
    })
}

struct CallbackServerInner {
    settled: Mutex<bool>,
    tx: Mutex<Option<oneshot::Sender<Option<String>>>>,
    shutdown: CancellationToken,
}

impl CallbackServerInner {
    fn settle(&self, code: Option<String>) {
        let mut settled = self.settled.lock().unwrap();
        if *settled {
            return;
        }
        *settled = true;
        self.shutdown.cancel();
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let _ = tx.send(code);
        }
    }
}

/// Handle for the running callback server. A bind failure degrades to a wait
/// that resolves `None`, mirroring the TypeScript `on("error")` fallback.
struct CallbackServer {
    inner: Option<Arc<CallbackServerInner>>,
    rx: Mutex<Option<oneshot::Receiver<Option<String>>>>,
}

impl CallbackServer {
    fn clone_handle(&self) -> Self {
        Self {
            inner: self.inner.as_ref().map(Arc::clone),
            rx: Mutex::new(None),
        }
    }

    fn close(&self) {
        if let Some(inner) = &self.inner {
            inner.shutdown.cancel();
        }
    }

    fn cancel_wait(&self) {
        if let Some(inner) = &self.inner {
            inner.settle(None);
        }
    }

    async fn wait_for_code(&self) -> Option<String> {
        let rx = self.rx.lock().unwrap().take();
        match rx {
            Some(rx) => rx.await.unwrap_or(None),
            None => None,
        }
    }
}

async fn start_local_oauth_server(
    state: &str,
    signal: &CancellationToken,
    callback_host: &str,
) -> CallbackServer {
    let listener = match TcpListener::bind((callback_host, CALLBACK_PORT)).await {
        Ok(listener) => listener,
        Err(_) => {
            return CallbackServer {
                inner: None,
                rx: Mutex::new(None),
            };
        }
    };
    let (tx, rx) = oneshot::channel();
    let inner = Arc::new(CallbackServerInner {
        settled: Mutex::new(false),
        tx: Mutex::new(Some(tx)),
        shutdown: CancellationToken::new(),
    });

    {
        let inner = Arc::clone(&inner);
        let shutdown = inner.shutdown.clone();
        let signal = signal.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = shutdown.cancelled() => {}
                _ = signal.cancelled() => inner.settle(None),
            }
        });
    }

    {
        let inner = Arc::clone(&inner);
        let state = state.to_string();
        let shutdown = inner.shutdown.clone();
        tokio::spawn(async move {
            loop {
                let connection = tokio::select! {
                    _ = shutdown.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => stream,
                        Err(_) => break,
                    },
                };
                let inner = Arc::clone(&inner);
                let state = state.clone();
                tokio::spawn(async move {
                    handle_callback_connection(inner, &state, connection).await;
                });
            }
        });
    }

    CallbackServer {
        inner: Some(inner),
        rx: Mutex::new(Some(rx)),
    }
}

async fn handle_callback_connection(
    inner: Arc<CallbackServerInner>,
    state: &str,
    mut stream: tokio::net::TcpStream,
) {
    let Some((_method, path_and_query)) = read_http_head(&mut stream).await else {
        return;
    };
    let Some(parsed) = url::Url::parse(&format!("http://loopback{path_and_query}")).ok() else {
        send_html(
            &mut stream,
            404,
            &oauth_error_html("Callback route not found.", None),
        )
        .await;
        return;
    };
    if parsed.path() != CALLBACK_PATH {
        send_html(
            &mut stream,
            404,
            &oauth_error_html("Callback route not found.", None),
        )
        .await;
        return;
    }
    let param = |name: &str| {
        parsed
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.to_string())
    };
    if param("state").as_deref() != Some(state) {
        send_html(&mut stream, 400, &oauth_error_html("State mismatch.", None)).await;
        return;
    }
    let Some(code) = param("code") else {
        send_html(
            &mut stream,
            400,
            &oauth_error_html("Missing authorization code.", None),
        )
        .await;
        return;
    };
    send_html(
        &mut stream,
        200,
        &oauth_success_html("OpenAI authentication completed. You can close this window."),
    )
    .await;
    inner.settle(Some(code));
}

async fn send_html(stream: &mut tokio::net::TcpStream, status: u16, html: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// Reads the request head and returns `(method, path?query)`.
async fn read_http_head(stream: &mut tokio::net::TcpStream) -> Option<(String, String)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") || buffer.len() > 64 * 1024 {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buffer);
    let request_line = head.lines().next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path_and_query = parts.next()?.to_string();
    Some((method, path_and_query))
}
