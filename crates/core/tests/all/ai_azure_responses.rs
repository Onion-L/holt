//! Ports of the Azure OpenAI Responses TypeScript suites:
//!
//! - `azure-openai-base-url.test.ts` (16 cases: base URL normalization,
//!   invalid URL failure, prompt_cache_key clamping, `store: false`,
//!   `supportsStrictMode: false`, resource-name default URL, pi User-Agent)
//! - `azure-openai-tool-choice.test.ts` (2 cases: provider-specific and
//!   provider-neutral tool choice forwarding with tools preserved)
//! - `azure-openai-responses-reasoning-replay.test.ts` (2 cases: reasoning
//!   `encrypted_content` preserved from `output_item.done` / backfilled from
//!   `response.completed`)
//! - `azure-utils.ts` (offline helper behavior: `parseDeploymentNameMap` /
//!   `resolveAzureDeploymentName` against a scoped env)
//!
//! The TS suites mock the `openai` module and capture the `AzureOpenAI`
//! constructor arguments plus the params handed to `responses.create`. The
//! Rust adapter issues the request itself, so the same observable surface is
//! captured through a mock `HttpFetch` transport: the request URL up to
//! `/deployments/...` is the baseURL the SDK client would have received, the
//! JSON body is the create params, and the request headers are the merged
//! `defaultHeaders`. The TS mock's `create` throws after capture; here a
//! minimal completed SSE response ends the stream offline instead — only the
//! captured artifacts are asserted.
//!
//! `hasAzureOpenAICredentials` in `azure-utils.ts` gates live-credential
//! tests and is TS test plumbing with no Rust counterpart; it stays
//! unported.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use pi_core::ai::api::azure_openai_responses::{
    AzureOpenAIResponsesOptions, parse_deployment_name_map, resolve_deployment_name,
    stream as stream_azure, stream_simple,
};
use pi_core::ai::api::openai_responses_shared::{
    convert_responses_messages, process_responses_stream,
};
use pi_core::ai::auth::resolve::now_millis;
use pi_core::ai::providers::builtin::get_builtin_model;
use pi_core::ai::session_resources::get_pi_user_agent;
use pi_core::ai::types::{
    AssistantMessage, ConstrainedSamplingConfig, ConstrainedSamplingStrict, Context, Message,
    Model, ModelCompat, ModelCost, ModelInput, ProviderEnv, ProviderRequestOptions, RoleAssistant,
    RoleUser, SimpleStreamOptions, StopReason, StreamOptions, Tool, ToolChoice,
    ToolConstrainedSampling, UserContent, UserMessage,
};
use pi_core::ai::utils::event_stream::create_assistant_message_event_stream;
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Shared fixtures (azure-openai-base-url.test.ts context/model helpers)

fn gpt_4o_mini() -> Model {
    get_builtin_model("azure-openai-responses", "gpt-4o-mini")
        .expect("builtin azure-openai-responses/gpt-4o-mini model")
}

fn hello_context() -> Context {
    Context {
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text("hello".to_string()),
            timestamp: 0,
        })],
        ..Default::default()
    }
}

/// Mock transport standing in for the TS `AzureOpenAI` client mock: it
/// records the outgoing request and answers with a minimal completed
/// Responses SSE stream.
struct CaptureFetch {
    requests: Mutex<Vec<HttpRequest>>,
}

impl CaptureFetch {
    fn new() -> Self {
        CaptureFetch {
            requests: Mutex::new(Vec::new()),
        }
    }

    /// The TS suites assert `constructorCalls` has length 1; the request is
    /// captured once for the single client construction.
    fn sole_request(&self) -> HttpRequest {
        let requests = self.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "expected exactly one captured request");
        requests[0].clone()
    }
}

impl HttpFetch for CaptureFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        self.requests.lock().unwrap().push(request);
        let body = format!(
            "data: {}\n\n",
            json!({
                "type": "response.completed",
                "sequence_number": 0,
                "response": {"id": "resp_mock", "status": "completed", "output": []},
            })
        );
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
            })
        })
    }
}

fn scoped_env(pairs: &[(&str, &str)]) -> ProviderEnv {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn azure_options(
    env: Option<ProviderEnv>,
    fetch: Arc<CaptureFetch>,
) -> AzureOpenAIResponsesOptions {
    AzureOpenAIResponsesOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("test-api-key".to_string()),
                fetch: Some(fetch),
                env,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

fn body_of(request: &HttpRequest) -> Value {
    match &request.body {
        HttpBody::Json(value) => value.clone(),
        _ => panic!("expected JSON body"),
    }
}

/// Case-insensitive lookup on a bare header list (the captured
/// `defaultHeaders`).
fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The baseURL handed to the client, i.e. the request URL before the SDK's
/// `/deployments/<model>/responses?api-version=...` suffix.
fn client_base_url(request: &HttpRequest) -> String {
    let index = request.url.find("/deployments/").expect("deployments path");
    request.url[..index].to_string()
}

/// Port of `captureClientBaseUrl`: sets AZURE_OPENAI_BASE_URL (as a scoped
/// env instead of `process.env`), streams once, and returns the captured
/// client baseURL.
async fn capture_client_base_url(base_url: &str) -> String {
    let fetch = Arc::new(CaptureFetch::new());
    let options = azure_options(
        Some(scoped_env(&[("AZURE_OPENAI_BASE_URL", base_url)])),
        fetch.clone(),
    );
    let _ = stream_azure(&gpt_4o_mini(), &hello_context(), Some(&options))
        .result()
        .await;
    client_base_url(&fetch.sole_request())
}

/// Port of `captureClientHeaders`: streams once with an explicit
/// azureBaseUrl and optional header overrides, returning the merged
/// defaultHeaders sent on the request.
async fn capture_client_headers(headers: Option<Vec<(String, String)>>) -> Vec<(String, String)> {
    let fetch = Arc::new(CaptureFetch::new());
    let provider_headers = headers.map(|entries| {
        entries
            .into_iter()
            .map(|(name, value)| (name, Some(value)))
            .collect::<pi_core::ai::types::ProviderHeaders>()
    });
    let options = AzureOpenAIResponsesOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("test-api-key".to_string()),
                fetch: Some(fetch.clone()),
                headers: provider_headers,
                ..Default::default()
            },
            ..Default::default()
        },
        azure_base_url: Some("https://my-resource.openai.azure.com".to_string()),
        ..Default::default()
    };
    let _ = stream_azure(&gpt_4o_mini(), &hello_context(), Some(&options))
        .result()
        .await;
    fetch.sole_request().headers
}

// ---------------------------------------------------------------------------
// azure-openai-responses base URL normalization
// (azure-openai-base-url.test.ts)

#[tokio::test]
async fn normalizes_cognitive_services_root_endpoints_to_openai_v1() {
    let base_url =
        capture_client_base_url("https://marc-quicktests-resource.cognitiveservices.azure.com")
            .await;
    assert_eq!(
        base_url,
        "https://marc-quicktests-resource.cognitiveservices.azure.com/openai/v1"
    );
}

#[tokio::test]
async fn normalizes_microsoft_foundry_root_endpoints_to_openai_v1() {
    let base_url = capture_client_base_url("https://marc-quicktests-resource.ai.azure.com").await;
    assert_eq!(
        base_url,
        "https://marc-quicktests-resource.ai.azure.com/openai/v1"
    );
}

#[tokio::test]
async fn normalizes_azure_openai_root_endpoints_to_openai_v1() {
    let base_url = capture_client_base_url("https://my-resource.openai.azure.com").await;
    assert_eq!(base_url, "https://my-resource.openai.azure.com/openai/v1");
}

#[tokio::test]
async fn normalizes_openai_to_openai_v1() {
    let base_url =
        capture_client_base_url("https://my-resource.cognitiveservices.azure.com/openai").await;
    assert_eq!(
        base_url,
        "https://my-resource.cognitiveservices.azure.com/openai/v1"
    );
}

#[tokio::test]
async fn preserves_openai_v1_endpoints() {
    let base_url =
        capture_client_base_url("https://my-resource.cognitiveservices.azure.com/openai/v1").await;
    assert_eq!(
        base_url,
        "https://my-resource.cognitiveservices.azure.com/openai/v1"
    );
}

#[tokio::test]
async fn normalizes_openai_v1_responses_to_openai_v1() {
    let base_url =
        capture_client_base_url("https://my-resource.services.ai.azure.com/openai/v1/responses")
            .await;
    assert_eq!(
        base_url,
        "https://my-resource.services.ai.azure.com/openai/v1"
    );
}

#[tokio::test]
async fn preserves_explicit_non_azure_proxy_paths() {
    let base_url = capture_client_base_url("https://my-proxy.example.com/v1").await;
    assert_eq!(base_url, "https://my-proxy.example.com/v1");
}

#[tokio::test]
async fn strips_query_params_when_normalizing_azure_host_urls() {
    let base_url = capture_client_base_url(
        "https://my-resource.openai.azure.com/openai?api-version=2024-12-01",
    )
    .await;
    assert_eq!(base_url, "https://my-resource.openai.azure.com/openai/v1");
}

#[tokio::test]
async fn preserves_query_params_on_non_azure_proxy_urls() {
    let base_url = capture_client_base_url("https://my-proxy.example.com/v1?custom=true").await;
    assert_eq!(base_url, "https://my-proxy.example.com/v1?custom=true");
}

#[tokio::test]
async fn throws_on_invalid_urls() {
    let fetch = Arc::new(CaptureFetch::new());
    let options = azure_options(
        Some(scoped_env(&[("AZURE_OPENAI_BASE_URL", "not-a-url")])),
        fetch.clone(),
    );
    let result = stream_azure(&gpt_4o_mini(), &hello_context(), Some(&options))
        .result()
        .await;

    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(
        result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("Invalid Azure OpenAI base URL")),
        "error message was {:?}",
        result.error_message
    );
}

#[tokio::test]
async fn clamps_prompt_cache_key_to_openais_64_character_limit() {
    let fetch = Arc::new(CaptureFetch::new());
    let mut options = azure_options(None, fetch.clone());
    options.azure_base_url = Some("https://my-resource.openai.azure.com".to_string());
    options.base.session_id = Some("x".repeat(67));

    let _ = stream_azure(&gpt_4o_mini(), &hello_context(), Some(&options))
        .result()
        .await;

    let body = body_of(&fetch.sole_request());
    assert_eq!(
        body.get("prompt_cache_key").and_then(Value::as_str),
        Some("x".repeat(64).as_str())
    );
}

#[tokio::test]
async fn disables_server_side_response_storage() {
    let fetch = Arc::new(CaptureFetch::new());
    let mut options = azure_options(None, fetch.clone());
    options.azure_base_url = Some("https://my-resource.openai.azure.com".to_string());

    let _ = stream_azure(&gpt_4o_mini(), &hello_context(), Some(&options))
        .result()
        .await;

    let body = body_of(&fetch.sole_request());
    assert_eq!(body.get("store"), Some(&json!(false)));
}

#[tokio::test]
async fn honors_supports_strict_mode_false() {
    let mut model = gpt_4o_mini();
    model.compat = Some(ModelCompat {
        supports_strict_mode: Some(false),
        ..Default::default()
    });
    let fetch = Arc::new(CaptureFetch::new());
    let mut options = azure_options(None, fetch.clone());
    options.azure_base_url = Some("https://my-resource.openai.azure.com".to_string());

    let context = Context {
        tools: Some(vec![Tool {
            name: "preferred".to_string(),
            description: "Preferred constrained tool".to_string(),
            parameters: json!({"type": "object", "properties": {"value": {"type": "string"}}}),
            constrained_sampling: Some(ToolConstrainedSampling::Config(
                ConstrainedSamplingConfig::JsonSchema {
                    strict: ConstrainedSamplingStrict::Prefer,
                },
            )),
        }]),
        ..hello_context()
    };

    let _ = stream_azure(&model, &context, Some(&options))
        .result()
        .await;

    let body = body_of(&fetch.sole_request());
    let tools = body.get("tools").and_then(Value::as_array).expect("tools");
    assert_eq!(tools.len(), 1);
    assert!(
        tools[0].get("strict").is_none(),
        "tool must not carry a strict flag: {tools:?}"
    );
}

#[tokio::test]
async fn builds_correct_default_url_from_azure_openai_resource_name() {
    let fetch = Arc::new(CaptureFetch::new());
    let options = azure_options(
        Some(scoped_env(&[("AZURE_OPENAI_RESOURCE_NAME", "my-resource")])),
        fetch.clone(),
    );
    let _ = stream_azure(&gpt_4o_mini(), &hello_context(), Some(&options))
        .result()
        .await;
    let request = fetch.sole_request();
    assert_eq!(
        client_base_url(&request),
        "https://my-resource.openai.azure.com/openai/v1"
    );
}

// ---------------------------------------------------------------------------
// azure-openai-responses user agent
// (azure-openai-base-url.test.ts)

#[tokio::test]
async fn uses_pi_s_user_agent_by_default() {
    let headers = capture_client_headers(None).await;
    let expected = get_pi_user_agent();
    assert_eq!(
        header_value(&headers, "User-Agent"),
        Some(expected.as_str())
    );
}

#[tokio::test]
async fn lets_explicit_headers_override_the_default_user_agent() {
    let headers = capture_client_headers(Some(vec![(
        "User-Agent".to_string(),
        "custom-agent".to_string(),
    )]))
    .await;
    assert_eq!(header_value(&headers, "User-Agent"), Some("custom-agent"));
}

// ---------------------------------------------------------------------------
// Azure OpenAI tool choice
// (azure-openai-tool-choice.test.ts)

fn deployment_model() -> Model {
    Model {
        id: "test-deployment".to_string(),
        name: "Test Deployment".to_string(),
        api: "azure-openai-responses".to_string(),
        provider: "azure-openai-responses".to_string(),
        base_url: "http://127.0.0.1:9/openai/v1".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 10_000,
        max_tokens: 1_000,
        ..Default::default()
    }
}

fn summarize_context() -> Context {
    Context {
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text("Summarize this".to_string()),
            timestamp: 1,
        })],
        tools: Some(vec![Tool {
            name: "read".to_string(),
            description: "Read a file".to_string(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            constrained_sampling: None,
        }]),
        ..Default::default()
    }
}

#[tokio::test]
async fn forwards_provider_specific_tool_choice_while_preserving_tool_definitions() {
    // The TS case captures the payload in `onPayload` and throws; the Rust
    // port captures the same payload as the request body.
    let fetch = Arc::new(CaptureFetch::new());
    let mut options = azure_options(None, fetch.clone());
    options.tool_choice = Some(json!("required"));

    let _ = stream_azure(&deployment_model(), &summarize_context(), Some(&options))
        .result()
        .await;

    let payload = body_of(&fetch.sole_request());
    assert_eq!(payload.get("tool_choice"), Some(&json!("required")));
    assert_eq!(
        payload.get("tools").and_then(Value::as_array).map(Vec::len),
        Some(1)
    );
}

#[tokio::test]
async fn forwards_provider_neutral_tool_choice_from_simple_options() {
    let fetch = Arc::new(CaptureFetch::new());
    let options = SimpleStreamOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("test-key".to_string()),
                fetch: Some(fetch.clone()),
                ..Default::default()
            },
            ..Default::default()
        },
        tool_choice: Some(ToolChoice::None),
        ..Default::default()
    };

    let _ = stream_simple(&deployment_model(), &summarize_context(), Some(&options))
        .result()
        .await;

    let payload = body_of(&fetch.sole_request());
    assert_eq!(payload.get("tool_choice"), Some(&json!("none")));
    assert_eq!(
        payload.get("tools").and_then(Value::as_array).map(Vec::len),
        Some(1)
    );
}

// ---------------------------------------------------------------------------
// Azure OpenAI Responses reasoning replay
// (azure-openai-responses-reasoning-replay.test.ts)

fn reasoning_model() -> Model {
    Model {
        id: "gpt-5-mini".to_string(),
        name: "GPT-5 Mini".to_string(),
        api: "azure-openai-responses".to_string(),
        provider: "azure-openai-responses".to_string(),
        base_url: "https://example.invalid".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 400_000,
        max_tokens: 128_000,
        ..Default::default()
    }
}

/// Port of `createOutput`.
fn create_output(model: &Model) -> AssistantMessage {
    AssistantMessage {
        role: RoleAssistant,
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        usage: Default::default(),
        stop_reason: StopReason::Pending,
        timestamp: now_millis(),
        ..Default::default()
    }
}

/// Port of `createEvents`.
fn replay_events(done_item: &Value, completed_item: Value) -> Vec<Result<Value, String>> {
    vec![
        Ok(json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "sequence_number": 0,
            "item": {"type": "reasoning", "id": done_item["id"], "summary": []},
        })),
        Ok(json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "sequence_number": 1,
            "item": done_item,
        })),
        Ok(json!({
            "type": "response.completed",
            "sequence_number": 2,
            "response": {"id": "resp_test", "status": "completed", "output": [completed_item]},
        })),
    ]
}

async fn run_replay(model: &Model, done_item: Value, completed_item: Value) -> AssistantMessage {
    let mut output = create_output(model);
    let event_stream = create_assistant_message_event_stream();
    process_responses_stream(
        futures::stream::iter(replay_events(&done_item, completed_item)),
        &mut output,
        &event_stream,
        model,
        None,
    )
    .await
    .expect("processResponsesStream succeeds");
    output
}

/// Port of `getReplayedReasoning`.
fn replayed_reasoning(model: &Model, assistant: AssistantMessage) -> Value {
    let context = Context {
        messages: vec![
            Message::User(UserMessage {
                role: RoleUser,
                content: UserContent::Text("first".to_string()),
                timestamp: now_millis() - 1,
            }),
            Message::Assistant(Box::new(assistant)),
            Message::User(UserMessage {
                role: RoleUser,
                content: UserContent::Text("follow-up".to_string()),
                timestamp: now_millis(),
            }),
        ],
        ..Default::default()
    };
    let allowed_providers: BTreeSet<String> =
        ["azure-openai-responses".to_string()].into_iter().collect();
    let input = convert_responses_messages(model, &context, &allowed_providers, None)
        .expect("convertResponsesMessages succeeds");
    input
        .into_iter()
        .find(|item| item.get("type") == Some(&json!("reasoning")))
        .expect("replayed reasoning item")
}

#[tokio::test]
async fn preserves_existing_encrypted_content_from_output_item_done() {
    let model = reasoning_model();
    let done_item = json!({
        "type": "reasoning",
        "id": "rs_done",
        "summary": [],
        "encrypted_content": "from-output-item-done",
    });
    let completed_item = json!({
        "type": "reasoning",
        "id": "rs_done",
        "summary": [],
        "encrypted_content": "from-response-completed",
    });

    let output = run_replay(&model, done_item, completed_item).await;

    let reasoning = replayed_reasoning(&model, output);
    assert_eq!(reasoning.get("type"), Some(&json!("reasoning")));
    assert_eq!(reasoning.get("id"), Some(&json!("rs_done")));
    assert_eq!(
        reasoning.get("encrypted_content"),
        Some(&json!("from-output-item-done"))
    );
}

#[tokio::test]
async fn fills_encrypted_content_when_output_item_done_omitted_it() {
    let model = reasoning_model();
    let done_item = json!({
        "type": "reasoning",
        "id": "rs_missing",
        "summary": [],
    });
    let completed_item = json!({
        "type": "reasoning",
        "id": "rs_missing",
        "summary": [],
        "encrypted_content": "from-response-completed",
    });

    let output = run_replay(&model, done_item, completed_item).await;

    let reasoning = replayed_reasoning(&model, output);
    assert_eq!(reasoning.get("type"), Some(&json!("reasoning")));
    assert_eq!(reasoning.get("id"), Some(&json!("rs_missing")));
    assert_eq!(
        reasoning.get("encrypted_content"),
        Some(&json!("from-response-completed"))
    );
}

// ---------------------------------------------------------------------------
// Azure deployment-name helpers
// (azure-utils.ts: parseDeploymentNameMap / resolveAzureDeploymentName)

#[test]
fn deployment_map_is_empty_without_a_value() {
    assert!(parse_deployment_name_map(None).is_empty());
    assert!(parse_deployment_name_map(Some("")).is_empty());
    assert!(parse_deployment_name_map(Some("   ")).is_empty());
}

#[test]
fn deployment_map_parses_trims_and_skips_malformed_entries() {
    let map = parse_deployment_name_map(Some(
        " gpt-4o-mini = mini-deploy ,gpt-5=gpt5, bad, =x, y=, ,gpt-4=gpt4 ",
    ));

    assert_eq!(
        map.get("gpt-4o-mini").map(String::as_str),
        Some("mini-deploy")
    );
    assert_eq!(map.get("gpt-5").map(String::as_str), Some("gpt5"));
    assert_eq!(map.get("gpt-4").map(String::as_str), Some("gpt4"));
    // "bad" has no "=", "=x" has no model id, "y=" has no deployment name,
    // and the empty segment is skipped.
    assert_eq!(map.len(), 3);
}

#[test]
fn deployment_map_drops_segments_beyond_the_typescript_split_limit() {
    // `entry.split("=", 2)` in TS truncates the result, so "a=b=c" maps
    // a -> "b" and discards the trailing "=c".
    let map = parse_deployment_name_map(Some("a=b=c"));
    assert_eq!(map.get("a").map(String::as_str), Some("b"));
}

fn deployment_options(
    env: Option<ProviderEnv>,
    azure_deployment_name: Option<&str>,
) -> AzureOpenAIResponsesOptions {
    AzureOpenAIResponsesOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                env,
                ..Default::default()
            },
            ..Default::default()
        },
        azure_deployment_name: azure_deployment_name.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn resolve_deployment_name_prefers_the_explicit_option() {
    let options = deployment_options(
        Some(scoped_env(&[(
            "AZURE_OPENAI_DEPLOYMENT_NAME_MAP",
            "gpt-4o-mini=mapped-deployment",
        )])),
        Some("explicit-deployment"),
    );
    assert_eq!(
        resolve_deployment_name(&gpt_4o_mini(), Some(&options)),
        "explicit-deployment"
    );
}

#[test]
fn resolve_deployment_name_uses_the_scoped_env_map() {
    let options = deployment_options(
        Some(scoped_env(&[(
            "AZURE_OPENAI_DEPLOYMENT_NAME_MAP",
            "gpt-4o-mini=mapped-deployment, gpt-5=gpt5-deployment",
        )])),
        None,
    );
    assert_eq!(
        resolve_deployment_name(&gpt_4o_mini(), Some(&options)),
        "mapped-deployment"
    );
}

#[test]
fn resolve_deployment_name_falls_back_to_the_model_id() {
    let options = deployment_options(
        Some(scoped_env(&[(
            "AZURE_OPENAI_DEPLOYMENT_NAME_MAP",
            "gpt-5=gpt5-deployment",
        )])),
        None,
    );
    assert_eq!(
        resolve_deployment_name(&gpt_4o_mini(), Some(&options)),
        "gpt-4o-mini"
    );
    assert_eq!(resolve_deployment_name(&gpt_4o_mini(), None), "gpt-4o-mini");
}

#[test]
fn resolve_deployment_name_treats_empty_strings_as_unset_like_typescript() {
    // An empty-string explicit option is falsy in TS, so the map wins; and
    // `mappedDeployment || model.id` treats a whitespace-only deployment
    // value (which trims to "") as unset.
    let map = scoped_env(&[("AZURE_OPENAI_DEPLOYMENT_NAME_MAP", "gpt-4o-mini=  ")]);

    let options = deployment_options(None, Some(""));
    assert_eq!(
        resolve_deployment_name(&gpt_4o_mini(), Some(&options)),
        "gpt-4o-mini"
    );

    let options = deployment_options(Some(map), None);
    assert_eq!(
        resolve_deployment_name(&gpt_4o_mini(), Some(&options)),
        "gpt-4o-mini"
    );
}

#[tokio::test]
async fn empty_string_azure_options_fall_through_like_typescript() {
    // Supplementary parity check (not asserted upstream): the TS config
    // resolves azureApiVersion / azureResourceName with `||`, so empty-string
    // options behave like unset options.
    let fetch = Arc::new(CaptureFetch::new());
    let mut options = azure_options(
        Some(scoped_env(&[("AZURE_OPENAI_RESOURCE_NAME", "my-resource")])),
        fetch.clone(),
    );
    options.azure_resource_name = Some(String::new());
    let _ = stream_azure(&gpt_4o_mini(), &hello_context(), Some(&options))
        .result()
        .await;
    assert_eq!(
        client_base_url(&fetch.sole_request()),
        "https://my-resource.openai.azure.com/openai/v1"
    );

    let fetch = Arc::new(CaptureFetch::new());
    let mut options = azure_options(None, fetch.clone());
    options.azure_base_url = Some("https://my-resource.openai.azure.com".to_string());
    options.azure_api_version = Some(String::new());
    let _ = stream_azure(&gpt_4o_mini(), &hello_context(), Some(&options))
        .result()
        .await;
    let request = fetch.sole_request();
    assert!(
        request.url.ends_with("?api-version=v1"),
        "default api version missing from {url}",
        url = request.url
    );
}
