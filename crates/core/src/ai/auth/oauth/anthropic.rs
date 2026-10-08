//! Port of `pi-core/ai/src/auth/oauth/anthropic.ts`: the Anthropic (Claude
//! Pro/Max) OAuth flow.
//!
//! The TypeScript module uses `node:http` for the OAuth callback server on a
//! fixed loopback port; the Rust port serves the same pages over a `tokio`
//! TCP listener. The client id is stored base64-encoded upstream; it is
//! inlined decoded here.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::ai::auth::oauth::NowMs;
use crate::ai::auth::oauth::oauth_page::{oauth_error_html, oauth_success_html};
use crate::ai::auth::oauth::pkce::{PkcePair, generate_pkce};
use crate::ai::auth::resolve::now_millis;
use crate::ai::auth::types::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthPromptKind, AuthStorageError, ModelAuth, OAuthAuth,
    OAuthCredential,
};
use crate::ai::types::{FetchFunction, ProviderEnv};
use crate::ai::utils::http::{HttpBody, HttpMethod, HttpRequest};
use crate::ai::utils::provider_env::get_provider_env_value;

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CALLBACK_PORT: u16 = 53692;
const CALLBACK_PATH: &str = "/callback";
const REDIRECT_URI: &str = "http://localhost:53692/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const REQUEST_TIMEOUT_MS: u64 = 30_000;
/// Refresh slightly before the reported expiry.
const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;

/// The Anthropic OAuth flow. Clone-cheap; `Default` uses the reqwest
/// transport, the real clock, and process env for the callback host.
#[derive(Clone)]
pub struct AnthropicOAuth {
    fetch: FetchFunction,
    now_ms: NowMs,
    env: ProviderEnv,
}

impl Default for AnthropicOAuth {
    fn default() -> Self {
        Self {
            fetch: crate::ai::utils::reqwest_fetch::default_fetch(),
            now_ms: Arc::new(now_millis),
            env: ProviderEnv::new(),
        }
    }
}

impl AnthropicOAuth {
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

    /// Port of `postJson`: JSON POST with the combined timeout+abort signal,
    /// returning the raw response body.
    async fn post_json(
        &self,
        url: &str,
        body: Value,
        signal: &CancellationToken,
    ) -> Result<String, String> {
        let request = HttpRequest {
            signal: None,
            method: HttpMethod::Post,
            url: url.to_string(),
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("accept".to_string(), "application/json".to_string()),
            ],
            body: HttpBody::Json(body),
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
        let body_text = crate::ai::utils::http::collect_text(response).await;
        if !(200..300).contains(&status) {
            return Err(format!(
                "HTTP request failed. status={status}; url={url}; body={body_text}"
            ));
        }
        Ok(body_text)
    }

    async fn exchange_authorization_code(
        &self,
        code: &str,
        state: &str,
        verifier: &str,
        redirect_uri: &str,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        let response_body = self
            .post_json(
                TOKEN_URL,
                json!({
                    "grant_type": "authorization_code",
                    "client_id": CLIENT_ID,
                    "code": code,
                    "state": state,
                    "redirect_uri": redirect_uri,
                    "code_verifier": verifier,
                }),
                signal,
            )
            .await
            .map_err(|error| {
                format!(
                    "Token exchange request failed. url={TOKEN_URL}; redirect_uri={redirect_uri}; response_type=authorization_code; details={error}"
                )
            })?;

        let data: Value = serde_json::from_str(&response_body).map_err(|error| {
            format!(
                "Token exchange returned invalid JSON. url={TOKEN_URL}; body={response_body}; details={error}"
            )
        })?;

        Ok(credential_from_token_response(&data, self.now()))
    }

    async fn refresh_anthropic_token(
        &self,
        refresh_token: &str,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        let response_body = self
            .post_json(
                TOKEN_URL,
                json!({
                    "grant_type": "refresh_token",
                    "client_id": CLIENT_ID,
                    "refresh_token": refresh_token,
                }),
                signal,
            )
            .await
            .map_err(|error| {
                format!("Anthropic token refresh request failed. url={TOKEN_URL}; details={error}")
            })?;

        let data: Value = serde_json::from_str(&response_body).map_err(|error| {
            format!(
                "Anthropic token refresh returned invalid JSON. url={TOKEN_URL}; body={response_body}; details={error}"
            )
        })?;

        Ok(credential_from_token_response(&data, self.now()))
    }

    async fn login_with_interaction(
        &self,
        interaction: Arc<dyn AuthInteraction>,
    ) -> Result<OAuthCredential, String> {
        let signal = interaction.signal().unwrap_or_default();
        let PkcePair {
            verifier,
            challenge,
        } = generate_pkce();
        let server = start_callback_server(&verifier, &signal, &self.callback_host()).await?;

        let manual_abort = CancellationToken::new();
        let manual_outcome: Arc<Mutex<ManualOutcome>> = Arc::new(Mutex::new(ManualOutcome {
            input: None,
            error: None,
        }));

        let authorize_url = format!(
            "{AUTHORIZE_URL}?{}",
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("code", "true")
                .append_pair("client_id", CLIENT_ID)
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", REDIRECT_URI)
                .append_pair("scope", SCOPES)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("state", &verifier)
                .finish()
        );
        interaction.notify(AuthEvent::AuthUrl {
            url: authorize_url,
            instructions: Some(
                "Complete login in your browser. If the browser is on another machine, paste the final redirect URL here."
                    .to_string(),
            ),
        });

        // Manual entry races the callback; any outcome hands the login over
        // to manual parsing unless a callback already delivered a code.
        let manual_task = {
            let interaction = Arc::clone(&interaction);
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
            let mut state: Option<String> = None;
            if let Some(error) = manual_outcome.lock().unwrap().error.clone() {
                return Err(error);
            }
            if let Some((callback_code, callback_state)) = result
                && !callback_code.is_empty()
            {
                code = Some(callback_code);
                state = Some(callback_state);
            }

            if code.is_none() {
                let manual_input = manual_outcome.lock().unwrap().input.clone();
                if let Some(input) = manual_input {
                    let parsed = parse_authorization_input(&input);
                    if let Some(parsed_state) = &parsed.state
                        && parsed_state != &verifier
                    {
                        return Err("OAuth state mismatch".to_string());
                    }
                    code = parsed.code;
                    state = Some(parsed.state.unwrap_or_else(|| verifier.clone()));
                }
            }

            if code.is_none() {
                let _ = manual_task.await;
                let (manual_input, manual_error) = {
                    let manual = manual_outcome.lock().unwrap();
                    (manual.input.clone(), manual.error.clone())
                };
                if let Some(error) = manual_error {
                    return Err(error);
                }
                if let Some(input) = manual_input {
                    let parsed = parse_authorization_input(&input);
                    if let Some(parsed_state) = &parsed.state
                        && parsed_state != &verifier
                    {
                        return Err("OAuth state mismatch".to_string());
                    }
                    code = parsed.code;
                    state = Some(parsed.state.unwrap_or_else(|| verifier.clone()));
                }
            }

            let Some(code) = code else {
                return Err("Missing authorization code".to_string());
            };
            let Some(state) = state else {
                return Err("Missing OAuth state".to_string());
            };
            interaction.notify(AuthEvent::Progress {
                message: "Exchanging authorization code for tokens...".to_string(),
            });
            self.exchange_authorization_code(&code, &state, &verifier, REDIRECT_URI, &signal)
                .await
        }
        .await;

        manual_abort.cancel();
        server.close();
        outcome
    }
}

/// The shared `anthropicOAuth` value.
pub fn anthropic_oauth() -> Arc<dyn OAuthAuth> {
    Arc::new(AnthropicOAuth::default())
}

impl OAuthAuth for AnthropicOAuth {
    fn name(&self) -> &str {
        "Anthropic (Claude Pro/Max)"
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
            this.refresh_anthropic_token(&refresh_token, &signal)
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

#[derive(Default)]
struct ManualOutcome {
    input: Option<String>,
    error: Option<String>,
}

struct ParsedAuthorizationInput {
    code: Option<String>,
    state: Option<String>,
}

/// Port of `parseAuthorizationInput`: accepts a redirect URL, a
/// `code#state` pair, a query string, or a bare code.
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

    if value.contains('#') {
        let (code, state) = value.split_once('#').unwrap();
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

fn credential_from_token_response(data: &Value, now_ms: i64) -> OAuthCredential {
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
    OAuthCredential {
        refresh: string_field("refresh_token"),
        access: string_field("access_token"),
        expires: now_ms + (expires_in * 1000.0) as i64 - REFRESH_SKEW_MS,
        extra: Default::default(),
    }
}

/// One-shot wait shared between the callback server and the login flow.
struct CallbackWait {
    settled: bool,
    tx: Option<oneshot::Sender<Option<(String, String)>>>,
}

struct CallbackServerInner {
    expected_state: String,
    wait: Mutex<CallbackWait>,
    shutdown: CancellationToken,
}

impl CallbackServerInner {
    /// Port of `settleWait`: first settle wins.
    fn settle(&self, value: Option<(String, String)>) {
        let mut wait = self.wait.lock().unwrap();
        if wait.settled {
            return;
        }
        wait.settled = true;
        if let Some(tx) = wait.tx.take() {
            let _ = tx.send(value);
        }
    }
}

/// Handle for the running callback server.
pub struct CallbackServer {
    inner: Arc<CallbackServerInner>,
    rx: Mutex<Option<CodeRx>>,
}

/// The one-shot wait behind `wait_for_code`.
type CodeRx = oneshot::Receiver<Option<(String, String)>>;

impl CallbackServer {
    fn clone_handle(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            rx: Mutex::new(None),
        }
    }

    fn close(&self) {
        self.inner.shutdown.cancel();
    }

    fn cancel_wait(&self) {
        self.inner.settle(None);
    }

    async fn wait_for_code(&self) -> Option<(String, String)> {
        let rx = self.rx.lock().unwrap().take();
        match rx {
            Some(rx) => rx.await.unwrap_or(None),
            None => None,
        }
    }
}

async fn start_callback_server(
    expected_state: &str,
    signal: &CancellationToken,
    callback_host: &str,
) -> Result<CallbackServer, String> {
    let listener = TcpListener::bind((callback_host, CALLBACK_PORT))
        .await
        .map_err(|error| error.to_string())?;
    let (tx, rx) = oneshot::channel();
    let inner = Arc::new(CallbackServerInner {
        expected_state: expected_state.to_string(),
        wait: Mutex::new(CallbackWait {
            settled: false,
            tx: Some(tx),
        }),
        shutdown: CancellationToken::new(),
    });

    // Flow cancellation hands the login to manual entry (cancelWait).
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
                tokio::spawn(async move {
                    handle_callback_connection(inner, connection).await;
                });
            }
        });
    }

    Ok(CallbackServer {
        inner,
        rx: Mutex::new(Some(rx)),
    })
}

async fn handle_callback_connection(
    inner: Arc<CallbackServerInner>,
    mut stream: tokio::net::TcpStream,
) {
    // The server only checks the route, mirroring the TypeScript handler.
    let Some((_method, path_and_query)) = read_http_head(&mut stream).await else {
        return;
    };
    let parsed = url::Url::parse(&format!("http://loopback{path_and_query}")).ok();
    let Some(parsed) = parsed else {
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
    let code = param("code");
    let state = param("state");

    if let Some(error) = param("error") {
        send_html(
            &mut stream,
            400,
            &oauth_error_html(
                "Anthropic authentication did not complete.",
                Some(&format!("Error: {error}")),
            ),
        )
        .await;
        return;
    }

    if code.as_deref().is_none_or(str::is_empty) || state.is_none() {
        send_html(
            &mut stream,
            400,
            &oauth_error_html("Missing code or state parameter.", None),
        )
        .await;
        return;
    }

    let (code, state) = (code.unwrap(), state.unwrap());
    if state != inner.expected_state {
        send_html(&mut stream, 400, &oauth_error_html("State mismatch.", None)).await;
        return;
    }

    send_html(
        &mut stream,
        200,
        &oauth_success_html("Anthropic authentication completed. You can close this window."),
    )
    .await;
    inner.settle(Some((code, state)));
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
