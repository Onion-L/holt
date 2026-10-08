//! Port of `pi-core/ai/src/auth/oauth/radius.ts`: the Radius gateway OAuth
//! flow.
//!
//! Radius is a pi-messages gateway. OAuth client APIs live on the configured
//! gateway; only the interactive browser authorization endpoint is
//! discovered. Model catalog loading is owned by the Radius provider.
//!
//! The TypeScript module uses `node:http` for the callback server; the Rust
//! port serves the same pages over a `tokio` TCP listener.

use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
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
use crate::ai::providers::radius_config::normalize_radius_gateway_url;
use crate::ai::types::FetchFunction;
use crate::ai::utils::http::{HttpBody, HttpMethod, HttpRequest, collect_text};
use crate::ai::utils::uuid::uuidv7;

const CALLBACK_HOST: &str = "127.0.0.1";
const CALLBACK_PORT: u16 = 1456;
const CALLBACK_PATH: &str = "/oauth/callback";
const REDIRECT_URI: &str = "http://127.0.0.1:1456/oauth/callback";
const TOKEN_EXPIRY_SKEW_MS: i64 = 60_000;
const LOGIN_METHOD_BROWSER: &str = "browser";
const LOGIN_METHOD_DEVICE_CODE: &str = "device-code";
const OAUTH_CLIENT_ID: &str = "pi-gateway";
const OAUTH_SCOPE: &str = "gateway offline_access";
const OAUTH_DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Port of `RadiusOAuthOptions`.
pub struct RadiusOAuthOptions {
    pub name: String,
    pub gateway: String,
}

/// Port of `OAuthResponseError`, kept as data so the poll loop can branch on
/// the upstream `error` code.
struct OAuthResponseError {
    #[allow(dead_code)]
    status: u16,
    oauth_error: Option<String>,
    message: String,
}

enum TokenRequestError {
    Response(OAuthResponseError),
    Other(String),
}

impl TokenRequestError {
    fn message(&self) -> String {
        match self {
            TokenRequestError::Response(error) => error.message.clone(),
            TokenRequestError::Other(message) => message.clone(),
        }
    }
}

/// The Radius OAuth flow. Clone-cheap.
#[derive(Clone)]
pub struct RadiusOAuth {
    name: String,
    gateway: String,
    fetch: FetchFunction,
    now_ms: NowMs,
}

/// Port of `createRadiusOAuth`.
pub fn create_radius_oauth(options: RadiusOAuthOptions) -> Arc<dyn OAuthAuth> {
    Arc::new(RadiusOAuth {
        name: options.name,
        gateway: normalize_radius_gateway_url(&options.gateway),
        fetch: crate::ai::utils::reqwest_fetch::default_fetch(),
        now_ms: Arc::new(now_millis),
    })
}

impl RadiusOAuth {
    /// Test constructor with an injectable transport and clock.
    pub fn new(
        name: impl Into<String>,
        gateway: impl Into<String>,
        fetch: FetchFunction,
        now_ms: NowMs,
    ) -> Self {
        Self {
            name: name.into(),
            gateway: normalize_radius_gateway_url(&gateway.into()),
            fetch,
            now_ms,
        }
    }

    fn now(&self) -> i64 {
        (self.now_ms)()
    }

    async fn fetch_response(
        &self,
        url: &str,
        method: HttpMethod,
        content_type: Option<&str>,
        body: HttpBody,
        signal: &CancellationToken,
    ) -> Result<(u16, String), String> {
        let mut headers = vec![("accept".to_string(), "application/json".to_string())];
        if let Some(content_type) = content_type {
            headers.push(("content-type".to_string(), content_type.to_string()));
        }
        let request = HttpRequest {
            signal: None,
            method,
            url: url.to_string(),
            headers,
            body,
        };
        let response = self.fetch.fetch(request).await.map_err(|error| {
            if signal.is_cancelled() {
                "Login cancelled".to_string()
            } else {
                error.to_string()
            }
        })?;
        let status = response.status;
        Ok((status, collect_text(response).await))
    }

    fn oauth_response_error(status: u16, text: String, message: &str) -> OAuthResponseError {
        let mut oauth_error: Option<String> = None;
        let mut description: Option<String> = None;
        if !text.is_empty() {
            match serde_json::from_str::<Value>(&text) {
                Ok(data) => {
                    oauth_error = data
                        .get("error")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    description = data
                        .get("error_description")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                Err(_) => {
                    description = Some(text);
                }
            }
        }
        let detail = match (&oauth_error, &description) {
            (Some(error), Some(description)) => format!("{error}: {description}"),
            (Some(error), None) => error.clone(),
            (None, Some(description)) => description.clone(),
            (None, None) => status.to_string(),
        };
        OAuthResponseError {
            status,
            oauth_error,
            message: format!("{message}: {detail}"),
        }
    }

    /// Port of `requestOAuthToken`.
    async fn request_oauth_token(
        &self,
        fields: &[(&str, &str)],
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, TokenRequestError> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields.iter().map(|(key, value)| (*key, *value)))
            .finish();
        let url = format!("{}/v1/oauth/token", self.gateway);
        let (status, text) = self
            .fetch_response(
                &url,
                HttpMethod::Post,
                Some("application/x-www-form-urlencoded"),
                HttpBody::Text(body),
                signal,
            )
            .await
            .map_err(TokenRequestError::Other)?;

        if !(200..300).contains(&status) {
            return Err(TokenRequestError::Response(Self::oauth_response_error(
                status,
                text,
                "Radius OAuth token request failed",
            )));
        }

        let data: Value = serde_json::from_str(&text)
            .map_err(|error| TokenRequestError::Other(error.to_string()))?;
        let string_field = |name: &str| {
            data.get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let expires_in = data
            .get("expires_in")
            .and_then(Value::as_f64)
            .unwrap_or(f64::NAN);
        let mut extra = Map::new();
        if let Some(scope) = data.get("scope").and_then(Value::as_str) {
            extra.insert("scope".to_string(), Value::String(scope.to_string()));
        }
        Ok(OAuthCredential {
            access: string_field("access_token"),
            refresh: string_field("refresh_token"),
            expires: self.now() + (expires_in * 1000.0) as i64 - TOKEN_EXPIRY_SKEW_MS,
            extra,
        })
    }

    /// Port of `loadRadiusOAuthDiscovery`.
    async fn load_discovery(&self, signal: &CancellationToken) -> Result<String, String> {
        let url = format!("{}/v1/oauth", self.gateway);
        let (status, text) = self
            .fetch_response(&url, HttpMethod::Get, None, HttpBody::Empty, signal)
            .await?;
        if !(200..300).contains(&status) {
            return Err(format!(
                "Could not load Radius OAuth config from {}: {status} {text}",
                self.gateway
            ));
        }
        let discovery: Value = serde_json::from_str(&text)
            .map_err(|_| format!("Invalid Radius OAuth config from {}", self.gateway))?;
        let Some(Value::String(authorization_endpoint)) = discovery.get("authorizationEndpoint")
        else {
            return Err(format!("Invalid Radius OAuth config from {}", self.gateway));
        };
        Ok(authorization_endpoint.clone())
    }

    async fn login_with_browser(
        &self,
        authorization_endpoint: &str,
        interaction: &Arc<dyn AuthInteraction>,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        let PkcePair {
            verifier,
            challenge,
        } = generate_pkce();
        // `crypto.randomUUID()` in TypeScript; any unique token serves the
        // state check.
        let state = uuidv7();
        let authorize_url = format!(
            "{authorization_endpoint}?{}",
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("response_type", "code")
                .append_pair("client_id", OAUTH_CLIENT_ID)
                .append_pair("redirect_uri", REDIRECT_URI)
                .append_pair("scope", OAUTH_SCOPE)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("handoff", "url")
                .append_pair("state", &state)
                .finish()
        );

        let callback = start_oauth_callback_server(&state, signal).await;
        interaction.notify(AuthEvent::Progress {
            message: format!("Listening for OAuth callback on {REDIRECT_URI}"),
        });
        interaction.notify(AuthEvent::AuthUrl {
            url: authorize_url,
            instructions: Some("Continue in your browser.".to_string()),
        });

        let outcome = async {
            let code = callback.wait_for_code().await;
            let Some(code) = code else {
                if signal.is_cancelled() {
                    return Err("Login cancelled".to_string());
                }
                return Err("OAuth callback did not complete.".to_string());
            };
            self.request_oauth_token(
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", OAUTH_CLIENT_ID),
                    ("redirect_uri", REDIRECT_URI),
                    ("code", code.as_str()),
                    ("code_verifier", verifier.as_str()),
                ],
                signal,
            )
            .await
            .map_err(|error| error.message())
        }
        .await;
        callback.close();
        outcome
    }

    async fn login_with_device_code(
        &self,
        interaction: &Arc<dyn AuthInteraction>,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        let device = self.request_device_authorization(signal).await?;
        interaction.notify(AuthEvent::DeviceCode {
            user_code: device.user_code.clone(),
            verification_uri: device.verification_uri.clone(),
            interval_seconds: device.interval.map(|interval| interval as u64),
            expires_in_seconds: Some(device.expires_in as u64),
        });

        let this = self.clone();
        let device = Arc::new(device);
        let signal = signal.clone();
        poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
            interval_seconds: device.interval,
            expires_in_seconds: Some(device.expires_in),
            wait_before_first_poll: false,
            signal: signal.clone(),
            poll: move || {
                let this = this.clone();
                let device = Arc::clone(&device);
                let signal = signal.clone();
                async move {
                    match this
                        .request_oauth_token(
                            &[
                                ("grant_type", OAUTH_DEVICE_CODE_GRANT_TYPE),
                                ("client_id", OAUTH_CLIENT_ID),
                                ("device_code", device.device_code.as_str()),
                            ],
                            &signal,
                        )
                        .await
                    {
                        Ok(credentials) => OAuthDeviceCodePollResult::Complete(credentials),
                        Err(TokenRequestError::Other(message)) => {
                            // Non-OAuth errors propagate out of the poller
                            // unchanged in TypeScript.
                            OAuthDeviceCodePollResult::Failed { message }
                        }
                        Err(TokenRequestError::Response(error)) => {
                            match error.oauth_error.as_deref() {
                                Some("authorization_pending") => OAuthDeviceCodePollResult::Pending,
                                Some("slow_down") => OAuthDeviceCodePollResult::SlowDown {
                                    interval_seconds: None,
                                },
                                Some("expired_token") => OAuthDeviceCodePollResult::Failed {
                                    message: "Device authorization expired.".to_string(),
                                },
                                Some("access_denied") => OAuthDeviceCodePollResult::Failed {
                                    message: "Device authorization was denied.".to_string(),
                                },
                                _ => OAuthDeviceCodePollResult::Failed {
                                    message: error.message,
                                },
                            }
                        }
                    }
                }
            },
        })
        .await
    }

    async fn request_device_authorization(
        &self,
        signal: &CancellationToken,
    ) -> Result<DeviceAuthorizationResponse, String> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", OAUTH_CLIENT_ID)
            .append_pair("scope", OAUTH_SCOPE)
            .finish();
        let url = format!("{}/v1/oauth/device", self.gateway);
        let (status, text) = self
            .fetch_response(
                &url,
                HttpMethod::Post,
                Some("application/x-www-form-urlencoded"),
                HttpBody::Text(body),
                signal,
            )
            .await?;
        if !(200..300).contains(&status) {
            return Err(Self::oauth_response_error(
                status,
                text,
                "Radius OAuth device authorization failed",
            )
            .message);
        }

        let data: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        let string_field = |name: &str| data.get(name).and_then(Value::as_str);
        let number_field = |name: &str| data.get(name).and_then(Value::as_f64);
        let (Some(device_code), Some(user_code), Some(verification_uri), Some(expires_in)) = (
            string_field("device_code"),
            string_field("user_code"),
            string_field("verification_uri"),
            number_field("expires_in"),
        ) else {
            return Err(
                "Radius OAuth device authorization response is missing required fields".to_string(),
            );
        };
        Ok(DeviceAuthorizationResponse {
            device_code: device_code.to_string(),
            user_code: user_code.to_string(),
            verification_uri: verification_uri.to_string(),
            expires_in,
            interval: number_field("interval"),
        })
    }

    async fn login_with_interaction(
        &self,
        interaction: Arc<dyn AuthInteraction>,
    ) -> Result<OAuthCredential, String> {
        let signal = interaction.signal().unwrap_or_default();
        let login_method = interaction
            .prompt(AuthPrompt {
                signal: None,
                kind: AuthPromptKind::Select {
                    message: format!("Sign in to {}:", self.name),
                    options: vec![
                        AuthPromptOption {
                            id: LOGIN_METHOD_BROWSER.to_string(),
                            label: "Sign in with browser (recommended)".to_string(),
                            description: None,
                        },
                        AuthPromptOption {
                            id: LOGIN_METHOD_DEVICE_CODE.to_string(),
                            label: "Sign in with device code (when signing in from another device)"
                                .to_string(),
                            description: None,
                        },
                    ],
                },
            })
            .await
            .map_err(|error| error.0)?;

        match login_method.as_str() {
            LOGIN_METHOD_DEVICE_CODE => self.login_with_device_code(&interaction, &signal).await,
            LOGIN_METHOD_BROWSER => {
                let authorization_endpoint = self.load_discovery(&signal).await?;
                self.login_with_browser(&authorization_endpoint, &interaction, &signal)
                    .await
            }
            other => Err(format!("Unknown {} sign-in method: {other}", self.name)),
        }
    }
}

impl OAuthAuth for RadiusOAuth {
    fn name(&self) -> &str {
        &self.name
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
            this.request_oauth_token(
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", OAUTH_CLIENT_ID),
                    ("refresh_token", refresh_token.as_str()),
                ],
                &signal,
            )
            .await
            .map_err(|error| AuthStorageError(error.message()))
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

struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: f64,
    interval: Option<f64>,
}

struct CallbackServerInner {
    settled: Mutex<bool>,
    tx: Mutex<Option<oneshot::Sender<Option<String>>>>,
    shutdown: CancellationToken,
}

impl CallbackServerInner {
    fn finish(&self, code: Option<String>) {
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

/// Handle for one running callback server. A bind failure degrades to a
/// `wait` that resolves `None`, mirroring the TypeScript `once("error")`
/// fallback.
pub struct CallbackServer {
    inner: Option<Arc<CallbackServerInner>>,
    rx: Mutex<Option<oneshot::Receiver<Option<String>>>>,
}

impl CallbackServer {
    async fn wait_for_code(&self) -> Option<String> {
        let rx = self.rx.lock().unwrap().take();
        match rx {
            Some(rx) => rx.await.unwrap_or(None),
            None => None,
        }
    }

    fn close(&self) {
        if let Some(inner) = &self.inner {
            inner.finish(None);
        }
    }
}

async fn start_oauth_callback_server(
    expected_state: &str,
    signal: &CancellationToken,
) -> CallbackServer {
    let listener = match TcpListener::bind((CALLBACK_HOST, CALLBACK_PORT)).await {
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

    // Flow cancellation settles the wait with no code.
    {
        let inner = Arc::clone(&inner);
        let shutdown = inner.shutdown.clone();
        let signal = signal.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = shutdown.cancelled() => {}
                _ = signal.cancelled() => inner.finish(None),
            }
        });
    }

    {
        let inner = Arc::clone(&inner);
        let expected_state = expected_state.to_string();
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
                let expected_state = expected_state.clone();
                tokio::spawn(async move {
                    handle_callback_connection(inner, &expected_state, connection).await;
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
    expected_state: &str,
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

    if param("state").as_deref() != Some(expected_state) {
        send_html(
            &mut stream,
            400,
            &oauth_error_html("OAuth state mismatch.", None),
        )
        .await;
        return;
    }

    if let Some(error) = param("error") {
        let page_message = param("error_description").unwrap_or_else(|| error.clone());
        send_html(&mut stream, 400, &oauth_error_html(&page_message, None)).await;
        inner.finish(None);
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
        &oauth_success_html("Signed in to Radius. You may now close this page."),
    )
    .await;
    inner.finish(Some(code));
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
    // Give the page a moment to flush before the connection closes.
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
