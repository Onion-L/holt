//! Shared helpers for the credential-gated live AI suites (`tests/ai_live_*`).
//!
//! Rust counterpart of the TypeScript live-test scaffolding:
//!
//! - `pi-core/ai/test/oauth.ts` `resolveApiKey`: API-key/OAuth resolution from
//!   `~/.pi/agent/auth.json`, refreshing expired OAuth tokens through the
//!   builtin provider's OAuth flow and saving the credential back.
//! - The credential gates from `pi-core/ai/test/azure-utils.ts`,
//!   `bedrock-utils.ts`, and `cloudflare-utils.ts`, translated verbatim.
//! - A `compat::stream`-shaped dispatch ([`live_stream`]) that reproduces the
//!   TypeScript compat entry's behavior (env API-key injection, the Cloudflare
//!   auth branch) *and* carries the TypeScript `StreamOptions` extras
//!   (`thinking`, `thinkingEnabled`, `reasoningEffort`, `effort`, `reasoning`,
//!   `interleavedThinking`, `azureDeploymentName`, `project`, `location`,
//!   `requestMetadata`) into the per-API option structs. The Rust generic
//!   `StreamOptions` cannot express those extras, so dispatching through
//!   `pi_core::ai::compat::stream` alone would silently drop them; the
//!   dispatcher calls the same per-API stream functions the compat registry
//!   wraps.
//!
//! Live suites skip by printing `SKIP: <suite> requires <ENV>` on stderr and
//! returning early, exactly when the TypeScript `describe.skipIf` /
//! `it.skipIf` condition is false. Tests without credentials therefore pass.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pi_core::ai::api::anthropic_messages::{AnthropicEffort, AnthropicOptions};
use pi_core::ai::api::azure_openai_responses::AzureOpenAIResponsesOptions;
use pi_core::ai::api::bedrock_converse_stream::BedrockOptions;
use pi_core::ai::api::google_generative_ai::{GoogleOptions, GoogleThinkingConfig};
use pi_core::ai::api::google_vertex::GoogleVertexOptions;
use pi_core::ai::api::mistral_conversations::MistralOptions;
use pi_core::ai::api::openai_codex_responses::OpenAICodexResponsesOptions;
use pi_core::ai::api::openai_completions::OpenAICompletionsOptions;
use pi_core::ai::api::openai_responses::OpenAIResponsesOptions;
use pi_core::ai::auth::types::Credential;
use pi_core::ai::env_api_keys::get_env_api_key;
use pi_core::ai::providers::builtin::{builtin_models, builtin_providers, get_builtin_model};

/// Re-export of the generated per-provider catalog read (`getModels`).
#[allow(unused_imports)]
pub use pi_core::ai::providers::builtin::get_builtin_models;
use pi_core::ai::types::{
    AssistantMessage, CacheRetention, Context, FetchFunction, Model, OnPayloadCallback,
    ProviderHeaders, ProviderRequestOptions, StreamOptions, ThinkingLevel, Transport,
};
use pi_core::ai::utils::event_stream::AssistantMessageEventStream;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Environment gates (ports of the TypeScript test utils)
// ---------------------------------------------------------------------------

/// Reads an environment variable the way the TypeScript suites use it:
/// unset *or empty* is falsy for `process.env.X` truthiness checks.
pub fn live_env(name: &str) -> Option<String> {
    if offline() {
        return None;
    }
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

fn offline() -> bool {
    std::env::var("PI_TEST_OFFLINE").is_ok_and(|value| value == "1")
}

/// Port of `hasAzureOpenAICredentials` (azure-utils.ts).
pub fn has_azure_openai_credentials() -> bool {
    let has_key = live_env("AZURE_OPENAI_API_KEY").is_some();
    let has_base_url = live_env("AZURE_OPENAI_BASE_URL").is_some()
        || live_env("AZURE_OPENAI_RESOURCE_NAME").is_some();
    has_key && has_base_url
}

/// Port of `resolveAzureDeploymentName` (azure-utils.ts): parses
/// `AZURE_OPENAI_DEPLOYMENT_NAME_MAP` as `model=deployment` entries.
pub fn resolve_azure_deployment_name(model_id: &str) -> Option<String> {
    let map_value = live_env("AZURE_OPENAI_DEPLOYMENT_NAME_MAP")?;
    for entry in map_value.split(',') {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((entry_model_id, deployment_name)) = trimmed.split_once('=') else {
            continue;
        };
        let entry_model_id = entry_model_id.trim();
        if entry_model_id.is_empty() || deployment_name.is_empty() {
            continue;
        }
        if entry_model_id == model_id {
            return Some(deployment_name.trim().to_string());
        }
    }
    None
}

/// Port of `hasBedrockCredentials` (bedrock-utils.ts).
pub fn has_bedrock_credentials() -> bool {
    live_env("AWS_PROFILE").is_some()
        || (live_env("AWS_ACCESS_KEY_ID").is_some() && live_env("AWS_SECRET_ACCESS_KEY").is_some())
        || live_env("AWS_BEARER_TOKEN_BEDROCK").is_some()
}

/// Port of `hasCloudflareWorkersAICredentials` (cloudflare-utils.ts).
pub fn has_cloudflare_workers_ai_credentials() -> bool {
    live_env("CLOUDFLARE_API_KEY").is_some() && live_env("CLOUDFLARE_ACCOUNT_ID").is_some()
}

/// Port of `hasCloudflareAiGatewayCredentials` (cloudflare-utils.ts).
pub fn has_cloudflare_ai_gateway_credentials() -> bool {
    live_env("CLOUDFLARE_API_KEY").is_some()
        && live_env("CLOUDFLARE_ACCOUNT_ID").is_some()
        && live_env("CLOUDFLARE_GATEWAY_ID").is_some()
}

/// Prints the standard live-suite skip line and returns.
pub fn skip(suite: &str, env: &str) {
    if offline() {
        eprintln!("SKIP: {suite}: PI_TEST_OFFLINE=1 disables live credentials and services");
        return;
    }
    eprintln!("SKIP: {suite} requires {env}");
}

// ---------------------------------------------------------------------------
// OAuth token resolution (port of pi-core/ai/test/oauth.ts)
// ---------------------------------------------------------------------------

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn auth_storage_path() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(
        std::path::Path::new(&home)
            .join(".pi")
            .join("agent")
            .join("auth.json"),
    )
}

fn save_auth_storage(path: &std::path::Path, storage: &serde_json::Map<String, serde_json::Value>) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_ok()
    {
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    if let Ok(json) = serde_json::to_string_pretty(storage)
        && std::fs::write(path, json).is_ok()
    {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

/// Port of `resolveApiKey(provider)` from `oauth.ts`: resolves an API key for
/// a provider from `~/.pi/agent/auth.json`. API-key credentials return the
/// key directly; OAuth credentials refresh when expired (saving the rotated
/// credential back) and return the derived request API key.
pub async fn resolve_api_key(provider: &str) -> Option<String> {
    if offline() {
        return None;
    }
    let path = auth_storage_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let mut storage: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&content).ok()?;
    let entry = storage.get(provider)?.clone();
    let credential: Credential = serde_json::from_value(entry).ok()?;
    match credential {
        Credential::ApiKey(api_key) => api_key.key,
        Credential::OAuth(oauth_credential) => {
            let oauth = builtin_providers()
                .into_iter()
                .find(|candidate| candidate.id() == provider)?
                .auth()
                .oauth
                .clone()?;
            let mut credential = oauth_credential;
            if now_millis() >= credential.expires {
                credential = match oauth.refresh(&credential, CancellationToken::new()).await {
                    Ok(refreshed) => refreshed,
                    Err(_) => return None,
                };
            }
            let saved = serde_json::to_value(Credential::OAuth(credential.clone())).ok()?;
            storage.insert(provider.to_string(), saved);
            save_auth_storage(&path, &storage);
            oauth.to_auth(&credential).await.ok()?.api_key
        }
    }
}

// ---------------------------------------------------------------------------
// Live request options (TS `StreamOptions & Record<string, unknown>`)
// ---------------------------------------------------------------------------

/// The shared TS assertion scenarios reused across the live suites
/// (`basicTextGeneration`, `handleToolCall`, `handleStreaming`,
/// `handleThinking`, `handleImage`, `multiTurn`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    Basic,
    ToolCall,
    Streaming,
    Thinking,
    MultiTurn,
    Image,
}

/// The `reasoningEffort` values used by the TypeScript live suites.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveEffort {
    Low,
    Medium,
    High,
    Xhigh,
}

impl LiveEffort {
    /// The unified `ThinkingLevel` for APIs that model effort as a level.
    pub fn thinking_level(self) -> ThinkingLevel {
        match self {
            LiveEffort::Low => ThinkingLevel::Low,
            LiveEffort::Medium => ThinkingLevel::Medium,
            LiveEffort::High => ThinkingLevel::High,
            LiveEffort::Xhigh => ThinkingLevel::Xhigh,
        }
    }

    /// The literal TypeScript string (`reasoningEffort: "high"`).
    pub fn as_str(self) -> &'static str {
        match self {
            LiveEffort::Low => "low",
            LiveEffort::Medium => "medium",
            LiveEffort::High => "high",
            LiveEffort::Xhigh => "xhigh",
        }
    }
}

/// The Google `thinking: { enabled, budgetTokens?, level? }` extra. `level`
/// carries the raw `@google/genai` enum strings (`"LOW"`, `"MEDIUM"`, ...).
#[derive(Clone, Debug, PartialEq)]
pub struct LiveThinking {
    pub enabled: bool,
    pub budget_tokens: Option<i64>,
    pub level: Option<String>,
}

/// The TypeScript `StreamOptionsWithExtras` shape as used by the live suites:
/// the base request options plus the per-API extras the Rust generic
/// `StreamOptions` cannot express.
#[derive(Clone, Default)]
pub struct LiveOptions {
    pub api_key: Option<String>,
    pub headers: Option<ProviderHeaders>,
    pub signal: Option<CancellationToken>,
    pub fetch: Option<FetchFunction>,
    pub transport: Option<Transport>,
    pub on_payload: Option<OnPayloadCallback>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    // TS extras beyond the Rust `StreamOptions`:
    pub thinking: Option<LiveThinking>,
    pub thinking_enabled: Option<bool>,
    pub thinking_budget_tokens: Option<u64>,
    pub effort: Option<LiveEffort>,
    pub reasoning_effort: Option<LiveEffort>,
    pub reasoning: Option<ThinkingLevel>,
    pub interleaved_thinking: Option<bool>,
    pub azure_deployment_name: Option<String>,
    pub project: Option<String>,
    pub location: Option<String>,
    pub request_metadata: Option<BTreeMap<String, String>>,
}

impl LiveOptions {
    /// Options carrying only the explicit API key (the `{ apiKey: token }`
    /// shape used by the OAuth suites).
    pub fn with_api_key(api_key: &str) -> Self {
        Self {
            api_key: Some(api_key.to_string()),
            ..Default::default()
        }
    }

    fn base(&self) -> StreamOptions {
        StreamOptions {
            base: ProviderRequestOptions {
                signal: self.signal.clone(),
                api_key: self.api_key.clone(),
                fetch: self.fetch.clone(),
                headers: self.headers.clone(),
                on_payload: self.on_payload.clone(),
                ..Default::default()
            },
            transport: self.transport,
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            cache_retention: self.cache_retention,
            session_id: self.session_id.clone(),
            ..Default::default()
        }
    }
}

const AMBIENT_AUTH_MARKER: &str = "<authenticated>";

/// Drives a live request the way the TypeScript suites do through
/// `complete`/`stream` from `compat.ts`: env API-key injection, the Cloudflare
/// auth branch, and dispatch into the per-API stream implementation with the
/// TypeScript option extras.
///
/// Deviation note: when a Cloudflare model has no resolved request auth, the
/// TypeScript compat entry routes through `builtinModels().stream(...)`, whose
/// Rust counterpart only accepts the generic `StreamOptions`; the TS extras
/// (e.g. `thinking`) cannot ride that path in Rust and are dropped.
pub fn live_stream(
    model: &Model,
    context: &Context,
    live: &LiveOptions,
) -> AssistantMessageEventStream {
    let explicit_key = live
        .api_key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty());
    let cloudflare_header = live
        .headers
        .as_ref()
        .and_then(|headers| headers.get("cf-aig-authorization"))
        .is_some_and(|value| value.is_some());
    if model.provider.starts_with("cloudflare-") && !explicit_key && !cloudflare_header {
        return builtin_models(Default::default()).stream(model, context, Some(live.base()));
    }

    let mut base = live.base();
    if !explicit_key
        && let Some(key) = get_env_api_key(&model.provider, None)
        && key != AMBIENT_AUTH_MARKER
    {
        base.base.api_key = Some(key);
    }

    match model.api.as_str() {
        "anthropic-messages" => pi_core::ai::api::anthropic_messages::stream(
            model,
            context,
            Some(&AnthropicOptions {
                base,
                thinking_enabled: live.thinking_enabled,
                thinking_budget_tokens: live.thinking_budget_tokens,
                effort: live.effort.map(|effort| match effort {
                    LiveEffort::Low => AnthropicEffort::Low,
                    LiveEffort::Medium => AnthropicEffort::Medium,
                    LiveEffort::High => AnthropicEffort::High,
                    LiveEffort::Xhigh => AnthropicEffort::Xhigh,
                }),
                interleaved_thinking: live.interleaved_thinking,
                ..Default::default()
            }),
        ),
        "openai-completions" => pi_core::ai::api::openai_completions::stream(
            model,
            context,
            Some(&OpenAICompletionsOptions {
                base,
                reasoning_effort: live.reasoning_effort.map(|effort| effort.thinking_level()),
                ..Default::default()
            }),
        ),
        "openai-responses" => pi_core::ai::api::openai_responses::stream(
            model,
            context,
            Some(&OpenAIResponsesOptions {
                base,
                reasoning_effort: live.reasoning_effort.map(|effort| effort.thinking_level()),
                ..Default::default()
            }),
        ),
        "openai-codex-responses" => pi_core::ai::api::openai_codex_responses::stream(
            model,
            context,
            Some(&OpenAICodexResponsesOptions {
                base,
                reasoning_effort: live
                    .reasoning_effort
                    .map(|effort| effort.as_str().to_string()),
                ..Default::default()
            }),
        ),
        "azure-openai-responses" => pi_core::ai::api::azure_openai_responses::stream(
            model,
            context,
            Some(&AzureOpenAIResponsesOptions {
                base,
                azure_deployment_name: live.azure_deployment_name.clone(),
                ..Default::default()
            }),
        ),
        "google-generative-ai" => pi_core::ai::api::google_generative_ai::stream(
            model,
            context,
            Some(&GoogleOptions {
                base,
                thinking: live.thinking.clone().map(|thinking| GoogleThinkingConfig {
                    enabled: thinking.enabled,
                    budget_tokens: thinking.budget_tokens,
                    level: thinking.level,
                }),
                ..Default::default()
            }),
        ),
        "google-vertex" => pi_core::ai::api::google_vertex::stream(
            model,
            context,
            Some(&GoogleVertexOptions {
                base,
                thinking: live.thinking.clone().map(|thinking| GoogleThinkingConfig {
                    enabled: thinking.enabled,
                    budget_tokens: thinking.budget_tokens,
                    level: thinking.level,
                }),
                project: live.project.clone(),
                location: live.location.clone(),
                ..Default::default()
            }),
        ),
        "mistral-conversations" => pi_core::ai::api::mistral_conversations::stream(
            model,
            context,
            Some(&MistralOptions {
                base,
                ..Default::default()
            }),
        ),
        "bedrock-converse-stream" => pi_core::ai::api::bedrock_converse_stream::stream(
            model,
            context,
            Some(&BedrockOptions {
                base,
                reasoning: live.reasoning,
                interleaved_thinking: live.interleaved_thinking,
                request_metadata: live.request_metadata.clone(),
                ..Default::default()
            }),
        ),
        _ => pi_core::ai::compat::stream(model, context, Some(&base)),
    }
}

/// The TypeScript suites' `await complete(model, context, options)`.
pub async fn live_complete(
    model: &Model,
    context: &Context,
    live: &LiveOptions,
) -> AssistantMessage {
    live_stream(model, context, live).result().await
}

// ---------------------------------------------------------------------------
// Shared fixtures and small utilities
// ---------------------------------------------------------------------------

/// `getModel(provider, id)` from compat.ts: the builtin catalog read.
pub fn get_model_or_panic(provider: &str, model_id: &str) -> Model {
    get_builtin_model(provider, model_id)
        .unwrap_or_else(|| panic!("Model not found: {provider}/{model_id}"))
}

/// The `@earendil-works/pi-ai` OpenAI-Completions override shape used by the
/// TS suites (`{ ...baseModel, api: "openai-completions" }`).
pub fn as_openai_completions(model: &Model) -> Model {
    Model {
        api: "openai-completions".to_string(),
        ..model.clone()
    }
}

/// The calculator tool schema from `stream.test.ts` (`StringEnum` produces
/// `{ type: "string", enum: [...] }` for Google compatibility).
pub fn calculator_tool() -> pi_core::ai::types::Tool {
    pi_core::ai::types::Tool {
        name: "math_operation".to_string(),
        description: "Perform basic arithmetic operations".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "required": ["a", "b", "operation"],
            "properties": {
                "a": { "type": "number", "description": "First number" },
                "b": { "type": "number", "description": "Second number" },
                "operation": {
                    "type": "string",
                    "enum": ["add", "subtract", "multiply", "divide"],
                    "description": "The operation to perform. One of 'add', 'subtract', 'multiply', 'divide'."
                }
            }
        }),
        constrained_sampling: None,
    }
}

/// `Type.Object({})` from typebox: `{"type":"object","properties":{}}`.
pub fn empty_object_schema() -> serde_json::Value {
    serde_json::json!({ "type": "object", "properties": {} })
}

/// The shared red-circle test image (`pi-core/ai/test/data/red-circle.png`)
/// as base64, matching `readFileSync(imagePath).toString("base64")`.
pub fn red_circle_base64() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("pi-core/ai/test/data/red-circle.png");
    let bytes =
        std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// An `onPayload` capture buffer (the TS suites' `let capturedPayload`).
pub fn payload_capture() -> (Arc<Mutex<Option<serde_json::Value>>>, OnPayloadCallback) {
    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&captured);
    let callback: OnPayloadCallback = Arc::new(move |payload, _model| {
        *sink.lock().unwrap() = Some(payload);
        Box::pin(async { None })
    });
    (captured, callback)
}

/// JavaScript `String.prototype.length`: the number of UTF-16 code units.
/// Used for the TS suites' `text.length >= N` abort thresholds.
pub fn js_length(text: &str) -> usize {
    text.encode_utf16().count()
}

/// JavaScript number-to-string for the `${result}` tool-result texts (the
/// suites only produce integers: 714, 887, 42).
pub fn js_number_to_string(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e21 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

// ---------------------------------------------------------------------------
// Local-LLM gates (Ollama / LM Studio / llama.cpp probes from the TS suites)
// ---------------------------------------------------------------------------

fn command_succeeds(program: &str, args: &[&str]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// TS: `execSync("which ollama", { stdio: "ignore" })` succeeds, unless
/// `PI_NO_LOCAL_LLM` is set.
pub fn ollama_installed() -> bool {
    if offline() {
        return false;
    }
    if live_env("PI_NO_LOCAL_LLM").is_some() {
        return false;
    }
    command_succeeds("which", &["ollama"])
}

/// Kills the spawned `ollama serve` on drop (TS afterAll SIGTERM).
pub struct OllamaServer(pub std::process::Child);

impl Drop for OllamaServer {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        let _ = self.0.wait();
    }
}

/// TS polls `fetch("http://localhost:11434/api/tags")` until it answers 2xx
/// (unbounded); the port uses a raw HTTP GET over `TcpStream` with a bounded
/// wait.
async fn wait_for_ollama_server() {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        if let Ok(mut stream) = TcpStream::connect("localhost:11434") {
            let _ = stream.write_all(b"GET /api/tags HTTP/1.0\r\nHost: localhost\r\n\r\n");
            let mut buffer = [0u8; 128];
            if let Ok(read) = stream.read(&mut buffer)
                && read > 0
                && let Ok(head) = std::str::from_utf8(&buffer[..read])
                && let Some(status) = head.split_whitespace().nth(1)
                && status.starts_with('2')
            {
                return;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "ollama server did not become ready"
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// The TS `beforeAll` for Ollama suites: pull `gpt-oss:20b` when missing,
/// then start `ollama serve` and wait for readiness.
pub enum OllamaSetup {
    /// `which ollama` failed (or `PI_NO_LOCAL_LLM`): the describe is skipped.
    NotInstalled,
    /// The model pull failed; TS warns "tests will be skipped" and leaves the
    /// suite's model undefined.
    PullFailed,
    Running(OllamaServer),
}

pub async fn setup_ollama() -> OllamaSetup {
    use std::process::{Command, Stdio};

    if !ollama_installed() {
        return OllamaSetup::NotInstalled;
    }

    // Check if model is available, if not pull it.
    let has_model = match Command::new("ollama").arg("list").output() {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).contains("gpt-oss:20b")
        }
        _ => false,
    };
    if !has_model {
        println!("Pulling gpt-oss:20b model for Ollama tests...");
        let pulled = Command::new("ollama")
            .args(["pull", "gpt-oss:20b"])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !pulled {
            eprintln!("Failed to pull gpt-oss:20b model, tests will be skipped");
            return OllamaSetup::PullFailed;
        }
    }

    // Start ollama server (the readiness wait tolerates an already-running
    // server: a second `ollama serve` exits, the port still answers).
    match Command::new("ollama")
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => {
            wait_for_ollama_server().await;
            OllamaSetup::Running(OllamaServer(child))
        }
        // The TS spawn does not fail the suite; the readiness wait below
        // still targets the shared local server.
        Err(_) => {
            wait_for_ollama_server().await;
            OllamaSetup::Running(OllamaServer(
                Command::new("true").spawn().expect("spawn no-op holder"),
            ))
        }
    }
}

/// TS LM Studio probe: `curl -s --max-time 1 http://localhost:1234/v1/models`.
pub fn lm_studio_running() -> bool {
    if offline() {
        return false;
    }
    if live_env("PI_NO_LOCAL_LLM").is_some() {
        return false;
    }
    command_succeeds(
        "curl",
        &["-s", "--max-time", "1", "http://localhost:1234/v1/models"],
    )
}

/// TS llama.cpp probe: a `/health` curl plus a POST status probe that must
/// not answer 404/405/000.
pub fn llama_cpp_running() -> bool {
    if offline() {
        return false;
    }
    if live_env("PI_NO_LOCAL_LLM").is_some() {
        return false;
    }
    if !command_succeeds(
        "curl",
        &["-s", "--max-time", "1", "http://localhost:8081/health"],
    ) {
        return false;
    }
    match std::process::Command::new("curl")
        .args([
            "-s",
            "--max-time",
            "1",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-X",
            "POST",
            "http://localhost:8081/v1/completions",
            "-H",
            "content-type: application/json",
            "-d",
            r#"{"model":"local-model","prompt":"ping","max_tokens":1}"#,
        ])
        .output()
    {
        Ok(output) if output.status.success() => {
            let probe_status = String::from_utf8_lossy(&output.stdout).trim().to_string();
            probe_status != "404" && probe_status != "405" && probe_status != "000"
        }
        _ => false,
    }
}
