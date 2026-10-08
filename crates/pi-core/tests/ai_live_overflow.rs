//! Rust entry point for the credential-gated live suite
//! `pi-core/ai/test/context-overflow.test.ts`
//! ("Context overflow error handling").
//!
//! 35 TS `it` cases: each provider sends a prompt that exceeds the model's
//! context window and asserts the overflow signal (stopReason "error" plus a
//! provider-specific error-message pattern, or the Xiaomi "length" variant,
//! or the conditional z.ai/ollama handling). The shared TS body
//! (`testContextOverflow`) and `logResult` are ported below; assertions use
//! the same regexes as the TypeScript source and
//! `pi_core::ai::utils::overflow::is_context_overflow`.
//!
//! Gates translated verbatim (env vars, azure/bedrock helpers,
//! `resolveApiKey` for the Copilot/Codex OAuth suites, and the local-LLM
//! probes for Ollama/LM Studio/llama.cpp). Without credentials each case
//! prints `SKIP: <case> requires <ENV>` and the test passes.
//!
//! Deviation: vitest `{ timeout: 120000 }` metadata has no Rust equivalent.

mod common;

use common::live::{
    self, LiveOptions, as_openai_completions, get_builtin_models, get_model_or_panic,
    live_complete, live_env, now_millis, skip,
};
use pi_core::ai::types::{
    AssistantMessage, Context, Message, Model, ModelCost, ModelInput, RoleUser, StopReason,
    UserContent, UserMessage,
};
use pi_core::ai::utils::overflow::is_context_overflow;

// Lorem ipsum paragraph for realistic token estimation (verbatim from the TS
// suite, including the trailing space).
const LOREM_IPSUM: &str = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. Duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla pariatur. Excepteur sint occaecat cupidatat non proident, sunt in culpa qui officia deserunt mollit anim id est laborum. ";

// Generate a string that will exceed the context window
// Using chars/4 as token estimate (works better with varied text than
// repeated chars)
fn generate_overflow_content(context_window: u64) -> String {
    let target_tokens = context_window + 10_000; // Exceed by 10k tokens
    let target_chars = target_tokens as f64 * 4.0 * 1.5;
    let repetitions = (target_chars / LOREM_IPSUM.len() as f64).ceil() as usize;
    LOREM_IPSUM.repeat(repetitions)
}

struct OverflowResult {
    provider: String,
    model: String,
    context_window: u64,
    stop_reason: StopReason,
    error_message: Option<String>,
    usage: pi_core::ai::types::Usage,
    has_usage_data: bool,
    response: AssistantMessage,
}

/// testContextOverflow from context-overflow.test.ts.
async fn test_context_overflow(model: &Model, api_key: &str) -> OverflowResult {
    let overflow_content = generate_overflow_content(model.context_window);

    let context = Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text(overflow_content),
            timestamp: now_millis(),
        })],
        tools: None,
    };

    // The TS suites pass `{ apiKey }` explicitly (including the Bedrock
    // suite's literal empty string).
    let options = LiveOptions {
        api_key: Some(api_key.to_string()),
        ..Default::default()
    };
    let response = live_complete(model, &context, &options).await;

    let has_usage_data = response.usage.input > 0 || response.usage.cache_read > 0;

    OverflowResult {
        provider: model.provider.clone(),
        model: model.id.clone(),
        context_window: model.context_window,
        stop_reason: response.stop_reason,
        error_message: response.error_message.clone(),
        usage: response.usage.clone(),
        has_usage_data,
        response,
    }
}

fn stop_reason_text(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Pending => "pending",
        StopReason::Stop => "stop",
        StopReason::Length => "length",
        StopReason::ToolUse => "toolUse",
        StopReason::Error => "error",
        StopReason::Aborted => "aborted",
        StopReason::Deferred => "deferred",
    }
}

/// logResult from context-overflow.test.ts.
fn log_result(result: &OverflowResult) {
    println!(
        "\n{} / {}:\n  contextWindow: {}\n  stopReason: {}\n  errorMessage: {:?}\n  usage: {}\n  hasUsageData: {}",
        result.provider,
        result.model,
        result.context_window,
        stop_reason_text(result.stop_reason),
        result.error_message,
        serde_json::to_string(&result.usage).unwrap_or_default(),
        result.has_usage_data,
    );
}

fn matches(pattern: &str, message: Option<&str>) -> bool {
    let Some(message) = message else {
        return false;
    };
    regex::Regex::new(pattern)
        .unwrap_or_else(|error| panic!("invalid pattern {pattern}: {error}"))
        .is_match(message)
}

fn assert_overflow(result: &OverflowResult, case: &str) {
    assert!(
        is_context_overflow(&result.response, Some(result.context_window)),
        "{case}: isContextOverflow"
    );
}

/// The expected overflow outcome per provider (from the TS assertions).
enum Expect {
    /// stopReason "error" plus a regex on the error message, then overflow.
    Error(&'static str),
    /// stopReason "error" plus overflow, no message pattern.
    ErrorOnly,
    /// Xiaomi: stopReason "length", usage.output == 0, overflow.
    XiaomiLength,
}

async fn run_overflow_case(model: &Model, api_key: &str, case: &str, expect: &Expect) {
    let result = test_context_overflow(model, api_key).await;
    log_result(&result);
    match expect {
        Expect::Error(pattern) => {
            assert_eq!(result.stop_reason, StopReason::Error, "{case}: stopReason");
            assert!(
                matches(pattern, result.error_message.as_deref()),
                "{case}: errorMessage {:?} matches {pattern}",
                result.error_message
            );
            assert_overflow(&result, case);
        }
        Expect::ErrorOnly => {
            assert_eq!(result.stop_reason, StopReason::Error, "{case}: stopReason");
            assert_overflow(&result, case);
        }
        Expect::XiaomiLength => {
            assert_eq!(result.stop_reason, StopReason::Length, "{case}: stopReason");
            assert_eq!(result.usage.output, 0, "{case}: usage.output");
            assert_overflow(&result, case);
        }
    }
}

#[tokio::test]
async fn overflow_anthropic_api_key() {
    // TS: describe.skipIf(!process.env.ANTHROPIC_API_KEY)
    let Some(key) = live_env("ANTHROPIC_API_KEY") else {
        skip(
            "Anthropic (API Key) claude-haiku-4-5 overflow",
            "ANTHROPIC_API_KEY",
        );
        return;
    };
    let model = get_model_or_panic("anthropic", "claude-haiku-4-5");
    run_overflow_case(
        &model,
        &key,
        "Anthropic (API Key): claude-haiku-4-5 - should detect overflow via isContextOverflow",
        &Expect::Error(r"(?i)prompt is too long"),
    )
    .await;
}

#[tokio::test]
async fn overflow_anthropic_oauth_env() {
    // TS: describe.skipIf(!process.env.ANTHROPIC_OAUTH_TOKEN)
    let Some(key) = live_env("ANTHROPIC_OAUTH_TOKEN") else {
        skip(
            "Anthropic (OAuth) claude-sonnet-4 overflow",
            "ANTHROPIC_OAUTH_TOKEN",
        );
        return;
    };
    let model = get_model_or_panic("anthropic", "claude-sonnet-4-6");
    run_overflow_case(
        &model,
        &key,
        "Anthropic (OAuth): claude-sonnet-4 - should detect overflow via isContextOverflow",
        &Expect::Error(r"(?i)prompt is too long"),
    )
    .await;
}

#[tokio::test]
async fn overflow_github_copilot() {
    // TS: describe("GitHub Copilot (OAuth)") — Google and Anthropic models.
    let Some(token) = live::resolve_api_key("github-copilot").await else {
        skip(
            "GitHub Copilot (OAuth) overflow",
            "~/.pi/agent/auth.json github-copilot credentials",
        );
        return;
    };

    // Google model via Copilot
    let model = get_builtin_models("github-copilot")
        .into_iter()
        .find(|candidate| candidate.id.starts_with("gemini-"))
        .expect("No Google models available through GitHub Copilot");
    run_overflow_case(
        &model,
        &token,
        "GitHub Copilot (OAuth): Google model - should detect overflow via isContextOverflow",
        &Expect::Error(r"(?i)exceeds the limit of \d+"),
    )
    .await;

    // Anthropic model via Copilot
    let model = get_model_or_panic("github-copilot", "claude-sonnet-4.6");
    run_overflow_case(
        &model,
        &token,
        "GitHub Copilot (OAuth): claude-sonnet-4 - should detect overflow via isContextOverflow",
        &Expect::Error(r"(?i)exceeds the limit of \d+|input is too long"),
    )
    .await;
}

#[tokio::test]
async fn overflow_openai_completions() {
    // TS: describe.skipIf(!process.env.OPENAI_API_KEY)
    let Some(key) = live_env("OPENAI_API_KEY") else {
        skip("OpenAI Completions gpt-4o-mini overflow", "OPENAI_API_KEY");
        return;
    };
    let model = as_openai_completions(&get_model_or_panic("openai", "gpt-4o-mini"));
    run_overflow_case(
        &model,
        &key,
        "OpenAI Completions: gpt-4o-mini - should detect overflow via isContextOverflow",
        &Expect::Error(r"(?i)maximum context length"),
    )
    .await;
}

#[tokio::test]
async fn overflow_openai_responses() {
    // TS: describe.skipIf(!process.env.OPENAI_API_KEY)
    let Some(key) = live_env("OPENAI_API_KEY") else {
        skip("OpenAI Responses gpt-4o overflow", "OPENAI_API_KEY");
        return;
    };
    let model = get_model_or_panic("openai", "gpt-4o");
    run_overflow_case(
        &model,
        &key,
        "OpenAI Responses: gpt-4o - should detect overflow via isContextOverflow",
        &Expect::Error(r"(?i)exceeds the context window"),
    )
    .await;
}

#[tokio::test]
async fn overflow_azure_openai_responses() {
    // TS: describe.skipIf(!hasAzureOpenAICredentials())
    if !live::has_azure_openai_credentials() {
        skip(
            "Azure OpenAI Responses gpt-4o-mini overflow",
            "AZURE_OPENAI_API_KEY + AZURE_OPENAI_BASE_URL|AZURE_OPENAI_RESOURCE_NAME",
        );
        return;
    }
    let model = get_model_or_panic("azure-openai-responses", "gpt-4o-mini");
    let key = live_env("AZURE_OPENAI_API_KEY").unwrap_or_default();
    run_overflow_case(
        &model,
        &key,
        "Azure OpenAI Responses: gpt-4o-mini - should detect overflow via isContextOverflow",
        &Expect::Error(r"(?i)context|maximum"),
    )
    .await;
}

#[tokio::test]
async fn overflow_google() {
    // TS: describe.skipIf(!process.env.GEMINI_API_KEY)
    let Some(key) = live_env("GEMINI_API_KEY") else {
        skip("Google gemini-2.5-flash overflow", "GEMINI_API_KEY");
        return;
    };
    let model = get_model_or_panic("google", "gemini-2.5-flash");
    run_overflow_case(
        &model,
        &key,
        "Google: gemini-2.5-flash - should detect overflow via isContextOverflow",
        &Expect::Error(r"(?i)input token count.*exceeds the maximum"),
    )
    .await;
}

#[tokio::test]
async fn overflow_openai_codex_oauth() {
    // TS: describe("OpenAI Codex (OAuth)").
    let Some(token) = live::resolve_api_key("openai-codex").await else {
        skip(
            "OpenAI Codex (OAuth) gpt-5.5 overflow",
            "~/.pi/agent/auth.json openai-codex credentials",
        );
        return;
    };
    let model = get_model_or_panic("openai-codex", "gpt-5.5");
    run_overflow_case(
        &model,
        &token,
        "OpenAI Codex (OAuth): gpt-5.5 - should detect overflow via isContextOverflow",
        &Expect::ErrorOnly,
    )
    .await;
}

#[tokio::test]
async fn overflow_amazon_bedrock() {
    // TS: describe.skipIf(!hasBedrockCredentials()) — passes an empty apiKey.
    if !live::has_bedrock_credentials() {
        skip(
            "Amazon Bedrock claude-sonnet-4-5 overflow",
            "AWS_PROFILE | AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY | AWS_BEARER_TOKEN_BEDROCK",
        );
        return;
    }
    let model = get_model_or_panic(
        "amazon-bedrock",
        "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
    );
    run_overflow_case(
        &model,
        "",
        "Amazon Bedrock: claude-sonnet-4-5 - should detect overflow via isContextOverflow",
        &Expect::ErrorOnly,
    )
    .await;
}

struct OverflowSuite {
    case: &'static str,
    provider: &'static str,
    model: &'static str,
    env: &'static str,
    expect: Expect,
}

#[tokio::test]
async fn overflow_env_provider_matrix() {
    let suites = [
        OverflowSuite {
            case: "xAI: grok-4.3 - should detect overflow via isContextOverflow",
            provider: "xai",
            model: "grok-4.3",
            env: "XAI_API_KEY",
            expect: Expect::Error(r"(?i)maximum prompt length is \d+"),
        },
        OverflowSuite {
            case: "Groq: llama-3.3-70b-versatile - should detect overflow via isContextOverflow",
            provider: "groq",
            model: "llama-3.3-70b-versatile",
            env: "GROQ_API_KEY",
            expect: Expect::Error(r"(?i)reduce the length of the messages"),
        },
        OverflowSuite {
            case: "Hugging Face: Kimi-K2.5 - should detect overflow via isContextOverflow",
            provider: "huggingface",
            model: "moonshotai/Kimi-K2.5",
            env: "HF_TOKEN",
            expect: Expect::ErrorOnly,
        },
        OverflowSuite {
            case: "Together AI: Kimi-K2.6 - should detect overflow via isContextOverflow",
            provider: "together",
            model: "moonshotai/Kimi-K2.6",
            env: "TOGETHER_API_KEY",
            expect: Expect::ErrorOnly,
        },
        OverflowSuite {
            case: "Mistral: devstral-medium-latest - should detect overflow via isContextOverflow",
            provider: "mistral",
            model: "devstral-medium-latest",
            env: "MISTRAL_API_KEY",
            expect: Expect::Error(r"(?i)too large for model with \d+ maximum context length"),
        },
        OverflowSuite {
            case: "MiniMax: MiniMax-M2.7 - should detect overflow via isContextOverflow",
            provider: "minimax",
            model: "MiniMax-M2.7",
            env: "MINIMAX_API_KEY",
            expect: Expect::ErrorOnly,
        },
        OverflowSuite {
            case: "Xiaomi MiMo (API billing): mimo-v2.5-pro - should detect overflow via isContextOverflow",
            provider: "xiaomi",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_API_KEY",
            expect: Expect::XiaomiLength,
        },
        OverflowSuite {
            case: "Xiaomi MiMo Token Plan (CN): mimo-v2.5-pro - should detect overflow via isContextOverflow",
            provider: "xiaomi-token-plan-cn",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_TOKEN_PLAN_CN_API_KEY",
            expect: Expect::XiaomiLength,
        },
        OverflowSuite {
            case: "Xiaomi MiMo Token Plan (AMS): mimo-v2.5-pro - should detect overflow via isContextOverflow",
            provider: "xiaomi-token-plan-ams",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
            expect: Expect::XiaomiLength,
        },
        OverflowSuite {
            case: "Xiaomi MiMo Token Plan (SGP): mimo-v2.5-pro - should detect overflow via isContextOverflow",
            provider: "xiaomi-token-plan-sgp",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
            expect: Expect::XiaomiLength,
        },
        OverflowSuite {
            case: "Qwen Token Plan: qwen3.7-max - should detect overflow via isContextOverflow",
            provider: "qwen-token-plan",
            model: "qwen3.7-max",
            env: "QWEN_TOKEN_PLAN_API_KEY",
            expect: Expect::Error(r"(?i)input length"),
        },
        OverflowSuite {
            case: "Qwen Token Plan Individual: qwen3.8-max - should detect overflow via isContextOverflow",
            provider: "qwen-token-plan-individual",
            model: "qwen3.8-max",
            env: "QWEN_TOKEN_PLAN_API_KEY",
            expect: Expect::Error(r"(?i)input length"),
        },
        OverflowSuite {
            case: "Qwen Token Plan (CN): qwen3.7-max - should detect overflow via isContextOverflow",
            provider: "qwen-token-plan-cn",
            model: "qwen3.7-max",
            env: "QWEN_TOKEN_PLAN_CN_API_KEY",
            expect: Expect::Error(r"(?i)input length"),
        },
        OverflowSuite {
            case: "Kimi For Coding: kimi-for-coding - should detect overflow via isContextOverflow",
            provider: "kimi-coding",
            model: "kimi-for-coding",
            env: "KIMI_API_KEY",
            expect: Expect::ErrorOnly,
        },
        OverflowSuite {
            case: "Vercel AI Gateway: google/gemini-2.5-flash - should detect overflow via isContextOverflow",
            provider: "vercel-ai-gateway",
            model: "google/gemini-2.5-flash",
            env: "AI_GATEWAY_API_KEY",
            expect: Expect::ErrorOnly,
        },
    ];

    for suite in suites {
        let Some(key) = live_env(suite.env) else {
            skip(suite.case, suite.env);
            continue;
        };
        let model = get_model_or_panic(suite.provider, suite.model);
        run_overflow_case(&model, &key, suite.case, &suite.expect).await;
    }
}

#[tokio::test]
async fn overflow_cerebras() {
    // TS: describe.skipIf(!process.env.CEREBRAS_API_KEY) — prefers
    // gpt-oss-120b / zai-glm-4.7 / llama3.1-8b, else the first model.
    let Some(key) = live_env("CEREBRAS_API_KEY") else {
        skip("Cerebras overflow", "CEREBRAS_API_KEY");
        return;
    };
    let preferred_cerebras_model_ids = ["gpt-oss-120b", "zai-glm-4.7", "llama3.1-8b"];
    let cerebras_models = get_builtin_models("cerebras");
    let model = cerebras_models
        .iter()
        .find(|candidate| preferred_cerebras_model_ids.contains(&candidate.id.as_str()))
        .unwrap_or_else(|| {
            cerebras_models
                .first()
                .expect("No Cerebras models available")
        });
    run_overflow_case(
        model,
        &key,
        "Cerebras: available model - should detect overflow via isContextOverflow",
        // Cerebras returns a status code with no body (400, 413, or 429).
        &Expect::Error(r"(?i)4(00|13|29).*\(no body\)"),
    )
    .await;
}

#[tokio::test]
async fn overflow_zai() {
    // z.ai behavior is inconsistent: explicit overflow error text, silent
    // acceptance with usage.input > contextWindow, or rate limiting.
    let Some(key) = live_env("ZAI_API_KEY") else {
        skip("z.ai glm-5.2 overflow", "ZAI_API_KEY");
        return;
    };
    let model = get_model_or_panic("zai", "glm-5.2");
    let result = test_context_overflow(&model, &key).await;
    log_result(&result);
    let case = "z.ai: glm-5.2 - should detect overflow via isContextOverflow when z.ai reports it";
    match result.stop_reason {
        StopReason::Error => {
            if matches(
                r"(?i)model_context_window_exceeded",
                result.error_message.as_deref(),
            ) {
                assert_overflow(&result, case);
            } else {
                println!(
                    "  z.ai returned non-overflow error (possibly rate limited), skipping overflow detection"
                );
            }
        }
        StopReason::Stop => {
            if result.has_usage_data && result.usage.input > model.context_window {
                assert_overflow(&result, case);
            } else {
                println!(
                    "  z.ai returned stop without overflow usage data, skipping overflow detection"
                );
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn overflow_openrouter() {
    // TS: describe.skipIf(!process.env.OPENROUTER_API_KEY) — five backends.
    let Some(key) = live_env("OPENROUTER_API_KEY") else {
        skip("OpenRouter overflow", "OPENROUTER_API_KEY");
        return;
    };
    for model_id in [
        "anthropic/claude-sonnet-4",
        "deepseek/deepseek-v3.2",
        "mistralai/mistral-large-2512",
        "google/gemini-2.5-flash",
        "meta-llama/llama-4-scout",
    ] {
        let model = get_model_or_panic("openrouter", model_id);
        run_overflow_case(
            &model,
            &key,
            &format!("OpenRouter: {model_id} - should detect overflow via isContextOverflow"),
            &Expect::Error(r"(?i)maximum context length is \d+ tokens"),
        )
        .await;
    }
}

#[tokio::test]
async fn overflow_ollama_local() {
    // TS: describe.skipIf(!ollamaInstalled) — ollama silently truncates.
    let server = match live::setup_ollama().await {
        live::OllamaSetup::NotInstalled => {
            skip("Ollama (local) overflow", "ollama binary");
            return;
        }
        live::OllamaSetup::PullFailed => {
            eprintln!("SKIP: Ollama (local) overflow requires pulling gpt-oss:20b to succeed");
            return;
        }
        live::OllamaSetup::Running(server) => server,
    };
    let model = Model {
        id: "gpt-oss:20b".to_string(),
        api: "openai-completions".to_string(),
        provider: "ollama".to_string(),
        base_url: "http://localhost:11434/v1".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        context_window: 128_000,
        max_tokens: 16_000,
        cost: ModelCost::default(),
        name: "Ollama GPT-OSS 20B".to_string(),
        ..Default::default()
    };
    let result = test_context_overflow(&model, "ollama").await;
    log_result(&result);
    // Ollama silently truncates input instead of erroring; when it does
    // error, the overflow must still be detected.
    match result.stop_reason {
        StopReason::Stop if result.has_usage_data => {
            println!(
                "  Ollama silently truncated input to {} tokens",
                result.usage.input
            );
        }
        StopReason::Error => {
            assert_overflow(
                &result,
                "Ollama (local): gpt-oss:20b - should detect overflow via isContextOverflow",
            );
        }
        _ => {}
    }
    drop(server);
}

#[tokio::test]
async fn overflow_lm_studio_local() {
    // TS: describe.skipIf(!lmStudioRunning).
    if !live::lm_studio_running() {
        skip(
            "LM Studio (local) overflow",
            "http://localhost:1234/v1/models reachable",
        );
        return;
    }
    let model = Model {
        id: "local-model".to_string(),
        api: "openai-completions".to_string(),
        provider: "lm-studio".to_string(),
        base_url: "http://localhost:1234/v1".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        context_window: 8192,
        max_tokens: 2048,
        cost: ModelCost::default(),
        name: "LM Studio Local Model".to_string(),
        ..Default::default()
    };
    let result = test_context_overflow(&model, "lm-studio").await;
    log_result(&result);
    assert_eq!(
        result.stop_reason,
        StopReason::Error,
        "LM Studio (local): should detect overflow via isContextOverflow"
    );
    assert_overflow(
        &result,
        "LM Studio (local): should detect overflow via isContextOverflow",
    );
}

#[tokio::test]
async fn overflow_llama_cpp_local() {
    // TS: describe.skipIf(!llamaCppRunning) — small context matches the
    // server's --ctx-size setting.
    if !live::llama_cpp_running() {
        skip(
            "llama.cpp (local) overflow",
            "http://localhost:8081 health + /v1/completions probe",
        );
        return;
    }
    let model = Model {
        id: "local-model".to_string(),
        api: "openai-completions".to_string(),
        provider: "llama.cpp".to_string(),
        base_url: "http://localhost:8081/v1".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        context_window: 4096,
        max_tokens: 2048,
        cost: ModelCost::default(),
        name: "llama.cpp Local Model".to_string(),
        ..Default::default()
    };
    let result = test_context_overflow(&model, "llama.cpp").await;
    log_result(&result);
    assert_eq!(
        result.stop_reason,
        StopReason::Error,
        "llama.cpp (local): should detect overflow via isContextOverflow"
    );
    assert_overflow(
        &result,
        "llama.cpp (local): should detect overflow via isContextOverflow",
    );
}
