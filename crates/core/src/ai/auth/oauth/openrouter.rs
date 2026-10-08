//! Port of `pi-core/ai/src/auth/oauth/openrouter.ts`: the OpenRouter PKCE
//! flow.
//!
//! OpenRouter exchanges an authorization code for a permanent, user-controlled
//! API key rather than an expiring access/refresh token pair. The callback is
//! handled by a one-shot loopback server on an ephemeral port, raced against a
//! manual prompt so remote/headless sessions can paste the redirect URL when
//! the browser cannot reach the loopback server.
//!
//! The TypeScript module uses `node:http` for the callback server; the Rust
//! port serves the same pages over a `tokio` TCP listener. The callback path
//! uses a UUIDv7 (the TypeScript `crypto.randomUUID()` v4 is a browser/Node
//! API; uniqueness is what the flow relies on).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::ai::auth::oauth::oauth_page::{oauth_error_html, oauth_success_html};
use crate::ai::auth::oauth::pkce::generate_pkce;
use crate::ai::auth::types::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthPromptKind, AuthStorageError, ModelAuth, OAuthAuth,
    OAuthCredential,
};
use crate::ai::types::{FetchFunction, ProviderEnv};
use crate::ai::utils::http::{HttpBody, HttpMethod, HttpRequest, collect_text};
use crate::ai::utils::provider_env::get_provider_env_value;
use crate::ai::utils::uuid::uuidv7;

const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
const TOKEN_URL: &str = "https://openrouter.ai/api/v1/auth/keys";
const LOGIN_TIMEOUT_MS: u64 = 5 * 60 * 1000;
const TOKEN_EXCHANGE_TIMEOUT_MS: u64 = 30_000;
/// `Number.MAX_SAFE_INTEGER`.
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// The OpenRouter OAuth flow. Clone-cheap; `Default` uses the reqwest
/// transport and process env for the callback host.
#[derive(Clone)]
pub struct OpenRouterOAuth {
    fetch: FetchFunction,
    env: ProviderEnv,
}

impl Default for OpenRouterOAuth {
    fn default() -> Self {
        Self {
            fetch: crate::ai::utils::reqwest_fetch::default_fetch(),
            env: ProviderEnv::new(),
        }
    }
}

impl OpenRouterOAuth {
    pub fn new(fetch: FetchFunction, env: ProviderEnv) -> Self {
        Self { fetch, env }
    }

    fn callback_host(&self) -> String {
        get_provider_env_value("PI_OAUTH_CALLBACK_HOST", Some(&self.env))
            .unwrap_or_else(|| "127.0.0.1".to_string())
    }

    async fn exchange_authorization_code(
        &self,
        code: &str,
        verifier: &str,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        if signal.is_cancelled() {
            return Err("Login cancelled".to_string());
        }
        let request = HttpRequest {
            signal: None,
            method: HttpMethod::Post,
            url: TOKEN_URL.to_string(),
            headers: vec![
                ("accept".to_string(), "application/json".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: HttpBody::Json(json!({
                "code": code,
                "code_verifier": verifier,
                "code_challenge_method": "S256",
            })),
        };
        let fetch = Arc::clone(&self.fetch);
        let response = tokio::time::timeout(
            Duration::from_millis(TOKEN_EXCHANGE_TIMEOUT_MS),
            fetch.fetch(request),
        )
        .await
        .map_err(|_| {
            if signal.is_cancelled() {
                "Login cancelled".to_string()
            } else {
                "OpenRouter OAuth token exchange timed out".to_string()
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
        let ok = (200..300).contains(&status);
        let text = collect_text(response).await;
        let body = match serde_json::from_str::<Value>(&text) {
            Ok(value) if value.is_object() => value,
            Ok(_) => Value::Object(Default::default()),
            Err(_) => {
                if ok {
                    return Err("OpenRouter OAuth returned invalid JSON".to_string());
                }
                Value::Object(Default::default())
            }
        };

        if !ok {
            let detail = error_detail(&body);
            return Err(format!(
                "OpenRouter OAuth key exchange failed (HTTP {status}){}",
                detail
                    .map(|detail| format!(": {detail}"))
                    .unwrap_or_default()
            ));
        }

        let Some(Value::String(key)) = body.get("key") else {
            return Err("OpenRouter OAuth response carries no \"key\"".to_string());
        };
        if key.is_empty() {
            return Err("OpenRouter OAuth response carries no \"key\"".to_string());
        }
        Ok(OAuthCredential {
            access: key.clone(),
            refresh: String::new(),
            expires: MAX_SAFE_INTEGER,
            extra: Default::default(),
        })
    }

    async fn start_callback_server(
        self: &Arc<Self>,
        callback_path: &str,
        verifier: &str,
        signal: &CancellationToken,
    ) -> Result<CallbackServer, String> {
        if signal.is_cancelled() {
            return Err("Login cancelled".to_string());
        }
        let callback_host = self.callback_host();
        let listener = TcpListener::bind((callback_host.as_str(), 0))
            .await
            .map_err(|error| error.to_string())?;
        let port = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();

        let (tx, rx) = oneshot::channel();
        let inner = Arc::new(CallbackServerInner {
            callback_path: callback_path.to_string(),
            verifier: verifier.to_string(),
            signal: signal.clone(),
            state: Mutex::new(CallbackServerState {
                claimed: false,
                settled: false,
                tx: Some(tx),
            }),
            shutdown: CancellationToken::new(),
        });

        // Login timeout: settles with an error after five minutes.
        {
            let inner = Arc::clone(&inner);
            let shutdown = inner.shutdown.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = shutdown.cancelled() => {}
                    _ = tokio::time::sleep(Duration::from_millis(LOGIN_TIMEOUT_MS)) => {
                        inner.finish(Err("OpenRouter OAuth login timed out".to_string()));
                    }
                }
            });
        }
        // Flow cancellation settles the wait with the cancel error.
        {
            let inner = Arc::clone(&inner);
            let shutdown = inner.shutdown.clone();
            let signal = signal.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = shutdown.cancelled() => {}
                    _ = signal.cancelled() => {
                        inner.finish(Err("Login cancelled".to_string()));
                    }
                }
            });
        }

        // Accept loop: each connection is handled concurrently so a 409/404
        // probe can be served while a claimed callback is still exchanging.
        {
            let inner = Arc::clone(&inner);
            let shutdown = inner.shutdown.clone();
            let flow = Arc::clone(self);
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
                    let flow = Arc::clone(&flow);
                    tokio::spawn(async move {
                        flow.handle_callback_connection(inner, connection).await;
                    });
                }
            });
        }

        if signal.is_cancelled() {
            inner.shutdown.cancel();
            return Err("Login cancelled".to_string());
        }

        Ok(CallbackServer {
            inner,
            callback_url: format!("http://{callback_host}:{port}{callback_path}"),
            manual_abort: Mutex::new(CancellationToken::new()),
            credential_rx: Mutex::new(Some(rx)),
        })
    }

    async fn handle_callback_connection(
        &self,
        inner: Arc<CallbackServerInner>,
        mut stream: tokio::net::TcpStream,
    ) {
        let Some(request) = read_http_head(&mut stream).await else {
            return;
        };
        let (method, path_and_query) = request;
        // The connection target itself is irrelevant; only the request line's
        // path and query matter, so parse against a dummy loopback base.
        let parsed = url::Url::parse(&format!("http://loopback{path_and_query}")).ok();
        let Some(parsed) = parsed else {
            send_html(
                &mut stream,
                404,
                &oauth_error_html("OAuth callback route not found.", None),
            )
            .await;
            return;
        };
        let query: std::collections::BTreeMap<String, String> = parsed
            .query_pairs()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();

        if method != "GET" || parsed.path() != inner.callback_path {
            send_html(
                &mut stream,
                404,
                &oauth_error_html("OAuth callback route not found.", None),
            )
            .await;
            return;
        }
        if inner.claimed() || inner.settled() {
            send_html(
                &mut stream,
                409,
                &oauth_error_html("This OAuth callback has already been used.", None),
            )
            .await;
            return;
        }

        if let Some(oauth_error) = query.get("error") {
            let description = query
                .get("error_description")
                .unwrap_or(oauth_error)
                .clone();
            send_html(
                &mut stream,
                400,
                &oauth_error_html("OpenRouter authorization was denied.", Some(&description)),
            )
            .await;
            inner.finish(Err(format!(
                "OpenRouter authorization failed: {description}"
            )));
            return;
        }

        let Some(code) = query.get("code").cloned() else {
            send_html(
                &mut stream,
                400,
                &oauth_error_html("OpenRouter returned no authorization code.", None),
            )
            .await;
            return;
        };
        inner.set_claimed();

        match self
            .exchange_authorization_code(&code, &inner.verifier, &inner.signal)
            .await
        {
            Ok(credential) => {
                send_html(
                    &mut stream,
                    200,
                    &oauth_success_html("Signed in to OpenRouter. You may now close this page."),
                )
                .await;
                inner.finish(Ok(Some(credential)));
            }
            Err(message) => {
                send_html(
                    &mut stream,
                    502,
                    &oauth_error_html("OpenRouter key exchange failed.", Some(&message)),
                )
                .await;
                inner.finish(Err(message));
            }
        }
    }
}

/// The shared `openRouterOAuth` value.
pub fn open_router_oauth() -> Arc<dyn OAuthAuth> {
    Arc::new(OpenRouterOAuth::default())
}

impl OAuthAuth for OpenRouterOAuth {
    fn name(&self) -> &str {
        "OpenRouter OAuth"
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with OpenRouter")
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
    ) -> crate::ai::auth::types::AuthFuture<Result<OAuthCredential, AuthStorageError>> {
        let this = Arc::new(self.clone());
        Box::pin(async move {
            this.login_with_interaction(interaction)
                .await
                .map_err(AuthStorageError)
        })
    }

    fn refresh(
        &self,
        credential: &OAuthCredential,
        _signal: CancellationToken,
    ) -> crate::ai::auth::types::AuthFuture<Result<OAuthCredential, AuthStorageError>> {
        // The OpenRouter credential is a permanent, user-controlled API key.
        let credential = credential.clone();
        Box::pin(async move { Ok(credential) })
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

impl OpenRouterOAuth {
    async fn login_with_interaction(
        self: Arc<Self>,
        interaction: Arc<dyn AuthInteraction>,
    ) -> Result<OAuthCredential, String> {
        let signal = interaction.signal().unwrap_or_default();
        let pkce = generate_pkce();
        // `crypto.randomUUID()` in TypeScript; any unique hex-and-dash token
        // serves the flow.
        let callback_path = format!("/oauth/callback/{}", uuidv7());
        let callback = self
            .start_callback_server(&callback_path, &pkce.verifier, &signal)
            .await?;

        // The TypeScript flow wraps everything in try/finally so the manual
        // prompt is aborted and the server closed on every exit.
        let outcome = self
            .run_login(&interaction, &signal, &pkce, &callback)
            .await;
        callback.manual_abort.lock().unwrap().cancel();
        callback.close();
        outcome
    }

    async fn run_login(
        self: &Arc<Self>,
        interaction: &Arc<dyn AuthInteraction>,
        signal: &CancellationToken,
        pkce: &crate::ai::auth::oauth::pkce::PkcePair,
        callback: &CallbackServer,
    ) -> Result<OAuthCredential, String> {
        let manual_abort = callback.manual_abort.lock().unwrap().clone();
        let manual_outcome: Arc<Mutex<ManualOutcome>> = Arc::new(Mutex::new(ManualOutcome {
            input: None,
            error: None,
        }));

        let authorize_url = {
            let mut url = url::Url::parse(AUTHORIZE_URL).expect("static authorize URL parses");
            url.query_pairs_mut()
                .append_pair("callback_url", &callback.callback_url)
                .append_pair("code_challenge", &pkce.challenge)
                .append_pair("code_challenge_method", "S256");
            url.to_string()
        };

        interaction.notify(AuthEvent::Progress {
            message: format!(
                "Listening for OpenRouter OAuth callback on {}",
                callback.callback_url
            ),
        });
        interaction.notify(AuthEvent::AuthUrl {
            url: authorize_url,
            instructions: Some(
                "Complete sign-in in your browser. If the browser is on another machine, paste the final redirect URL here."
                    .to_string(),
            ),
        });

        // Manual entry races the callback: any outcome hands the login over
        // unless a callback already claimed the exchange.
        let manual_task = {
            let interaction = Arc::clone(interaction);
            let manual_abort = manual_abort.clone();
            let callback = callback.clone_handle();
            let manual_outcome = Arc::clone(&manual_outcome);
            let placeholder = callback.callback_url.clone();
            tokio::spawn(async move {
                let prompt = AuthPrompt {
                    signal: Some(manual_abort),
                    kind: AuthPromptKind::ManualCode {
                        message: "Complete sign-in in your browser, or paste the authorization code / redirect URL here:"
                            .to_string(),
                        placeholder: Some(placeholder),
                    },
                };
                match interaction.prompt(prompt).await {
                    Ok(input) => {
                        manual_outcome.lock().unwrap().input = Some(input);
                        callback.cancel_wait();
                    }
                    Err(error) => {
                        manual_outcome.lock().unwrap().error = Some(error.0);
                        callback.cancel_wait();
                    }
                }
            })
        };

        let result = match callback.wait_for_credential().await {
            Ok(result) => result,
            Err(message) => return Err(message),
        };
        if let Some(error) = manual_outcome.lock().unwrap().error.clone() {
            return Err(error);
        }
        if let Some(credential) = result {
            return Ok(credential);
        }

        // No callback claimed the exchange: fall through to manual entry.
        let _ = manual_task.await;
        let (manual_input, manual_error) = {
            let manual = manual_outcome.lock().unwrap();
            (manual.input.clone(), manual.error.clone())
        };
        if let Some(error) = manual_error {
            return Err(error);
        }
        let Some(code) = manual_input.as_deref().and_then(parse_authorization_input) else {
            return Err("Missing authorization code".to_string());
        };
        interaction.notify(AuthEvent::Progress {
            message: "Exchanging authorization code for an API key...".to_string(),
        });
        self.exchange_authorization_code(&code, &pkce.verifier, signal)
            .await
    }
}

#[derive(Default)]
struct ManualOutcome {
    input: Option<String>,
    error: Option<String>,
}

struct CallbackServerState {
    claimed: bool,
    settled: bool,
    tx: Option<oneshot::Sender<Result<Option<OAuthCredential>, String>>>,
}

struct CallbackServerInner {
    callback_path: String,
    verifier: String,
    signal: CancellationToken,
    state: Mutex<CallbackServerState>,
    shutdown: CancellationToken,
}

impl CallbackServerInner {
    fn claimed(&self) -> bool {
        self.state.lock().unwrap().claimed
    }

    fn settled(&self) -> bool {
        self.state.lock().unwrap().settled
    }

    fn set_claimed(&self) {
        self.state.lock().unwrap().claimed = true;
    }

    /// Port of `finish`: settles the wait exactly once and stops the server.
    fn finish(&self, result: Result<Option<OAuthCredential>, String>) {
        let mut state = self.state.lock().unwrap();
        if state.settled {
            return;
        }
        state.settled = true;
        self.shutdown.cancel();
        if let Some(tx) = state.tx.take() {
            let _ = tx.send(result);
        }
    }
}

/// Handle for one running callback server.
pub struct CallbackServer {
    inner: Arc<CallbackServerInner>,
    callback_url: String,
    manual_abort: Mutex<CancellationToken>,
    credential_rx: Mutex<Option<CredentialRx>>,
}

/// The one-shot wait behind `wait_for_credential`.
type CredentialRx = oneshot::Receiver<Result<Option<OAuthCredential>, String>>;

impl CallbackServer {
    fn clone_handle(&self) -> Self {
        let manual_abort = self.manual_abort.lock().unwrap().clone();
        Self {
            inner: Arc::clone(&self.inner),
            callback_url: self.callback_url.clone(),
            manual_abort: Mutex::new(manual_abort),
            credential_rx: Mutex::new(None),
        }
    }

    /// Stop listening and release timers without settling the wait.
    pub fn close(&self) {
        self.inner.shutdown.cancel();
    }

    /// Hand the login over to manual code entry unless a callback already
    /// claimed the exchange.
    pub fn cancel_wait(&self) {
        if !self.inner.claimed() {
            self.inner.finish(Ok(None));
        }
    }

    /// Resolves with the credential once a browser callback completes the key
    /// exchange, or with `None` once [`CallbackServer::cancel_wait`] hands
    /// the login over to manual code entry.
    pub async fn wait_for_credential(&self) -> Result<Option<OAuthCredential>, String> {
        let rx = self.credential_rx.lock().unwrap().take();
        match rx {
            Some(rx) => rx.await.map_err(|_| "callback server closed".to_string())?,
            None => Err("callback server closed".to_string()),
        }
    }
}

async fn send_html(stream: &mut tokio::net::TcpStream, status: u16, html: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        502 => "Bad Gateway",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{html}",
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

/// Port of `parseAuthorizationInput`: accepts a redirect URL, a query string,
/// or a bare code.
fn parse_authorization_input(input: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }

    if let Ok(url) = url::Url::parse(value)
        && let Some((_, code)) = url.query_pairs().find(|(key, _)| key == "code")
    {
        return Some(code.to_string());
    }

    if value.contains("code=") {
        return url::form_urlencoded::parse(value.as_bytes())
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.to_string());
    }

    Some(value.to_string())
}

/// Port of `errorDetail`.
fn error_detail(body: &Value) -> Option<String> {
    let string_field = |value: &Value| value.as_str().map(str::to_string);
    if let Some(description) = body.get("error_description").and_then(string_field) {
        return Some(description);
    }
    if let Some(message) = body.get("message").and_then(string_field) {
        return Some(message);
    }
    if let Some(error) = body.get("error").and_then(string_field) {
        return Some(error);
    }
    if let Some(Value::Object(error)) = body.get("error")
        && let Some(message) = error.get("message").and_then(string_field)
    {
        return Some(message);
    }
    None
}
