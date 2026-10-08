//! Port of `pi-core/ai/test/overflow.test.ts`: context overflow and
//! recoverable-length detection over assistant messages.

use pi_core::ai::types::{AssistantMessage, RoleAssistant, StopReason, Usage, UsageCost};
use pi_core::ai::utils::overflow::{is_context_overflow, is_recoverable_length};

/// Port of `createErrorMessage`.
fn create_error_message(error_message: &str) -> AssistantMessage {
    AssistantMessage {
        role: RoleAssistant,
        content: Vec::new(),
        api: "openai-completions".to_string(),
        provider: "ollama".to_string(),
        model: "qwen3.5:35b".to_string(),
        usage: Usage {
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            total_tokens: 0,
            cost: UsageCost::default(),
            ..Default::default()
        },
        stop_reason: StopReason::Error,
        error_message: Some(error_message.to_string()),
        timestamp: 0,
        ..Default::default()
    }
}

/// Port of `createLengthStopMessage`.
struct LengthStopOptions<'a> {
    input: u64,
    cache_read: u64,
    output: u64,
    cache_write: Option<u64>,
    api: Option<&'a str>,
    provider: Option<&'a str>,
    model: Option<&'a str>,
}

fn create_length_stop_message(options: LengthStopOptions<'_>) -> AssistantMessage {
    let cache_write = options.cache_write.unwrap_or(0);
    AssistantMessage {
        role: RoleAssistant,
        content: Vec::new(),
        api: options.api.unwrap_or("openai-completions").to_string(),
        provider: options.provider.unwrap_or("test-provider").to_string(),
        model: options.model.unwrap_or("test-model").to_string(),
        usage: Usage {
            input: options.input,
            output: options.output,
            cache_read: options.cache_read,
            cache_write,
            total_tokens: options
                .input
                .saturating_add(options.cache_read)
                .saturating_add(cache_write)
                .saturating_add(options.output),
            cost: UsageCost::default(),
            ..Default::default()
        },
        stop_reason: StopReason::Length,
        timestamp: 0,
        ..Default::default()
    }
}

#[test]
fn detects_explicit_ollama_prompt_too_long_errors() {
    let message =
        create_error_message("400 `prompt too long; exceeded max context length by 100918 tokens`");
    assert!(is_context_overflow(&message, Some(32768)));
}

#[test]
fn detects_together_ai_context_length_errors() {
    let message = create_error_message(
        "400 The input (516368 tokens) is longer than the model's context length (262144 tokens).",
    );
    assert!(is_context_overflow(&message, Some(262144)));
}

#[test]
fn detects_litellm_wrapped_openai_maximum_context_length_errors() {
    let message = create_error_message(
        "Error: 503 litellm.ServiceUnavailableError: litellm.MidStreamFallbackError: litellm.APIConnectionError: APIConnectionError: OpenAIException - Requested token count exceeds the model's maximum context length of 131072 tokens.",
    );
    assert!(is_context_overflow(&message, Some(131072)));
}

#[test]
fn detects_openai_compatible_parenthesized_maximum_context_length_errors() {
    let message = create_error_message(
        "Error: 400 Input length (265330) exceeds model's maximum context length (262144).",
    );
    assert!(is_context_overflow(&message, Some(262144)));
}

#[test]
fn detects_openrouter_poolside_maximum_allowed_input_length_errors() {
    let message = create_error_message(
        "Provider returned error: Input length 131393 exceeds the maximum allowed input length of 131040 tokens.",
    );
    assert!(is_context_overflow(&message, Some(131072)));
}

#[test]
fn detects_ds4_configured_context_size_errors() {
    let message = create_error_message(
        "400 Prompt has 256468 tokens, but the configured context size is 256000 tokens",
    );
    assert!(is_context_overflow(&message, Some(256000)));

    let comma_message = create_error_message(
        "Prompt has 5,958,968 tokens, but the configured context size is 256,000 tokens",
    );
    assert!(is_context_overflow(&comma_message, Some(256000)));
}

#[test]
fn does_not_treat_generic_non_overflow_ollama_errors_as_overflow() {
    let message = create_error_message("500 `model runner crashed unexpectedly`");
    assert!(!is_context_overflow(&message, Some(32768)));
}

#[test]
fn does_not_treat_bedrock_throttling_too_many_tokens_as_overflow() {
    // Bedrock returns this for HTTP 429 rate limiting, NOT context overflow.
    // formatBedrockError uses a human-readable prefix for ThrottlingException.
    let message =
        create_error_message("Throttling error: Too many tokens, please wait before trying again.");
    assert!(!is_context_overflow(&message, Some(200000)));
}

#[test]
fn does_not_treat_bedrock_service_unavailable_as_overflow() {
    let message =
        create_error_message("Service unavailable: The service is temporarily unavailable.");
    assert!(!is_context_overflow(&message, Some(200000)));
}

#[test]
fn does_not_treat_generic_rate_limit_errors_as_overflow() {
    let message = create_error_message("Rate limit exceeded, please retry after 30 seconds.");
    assert!(!is_context_overflow(&message, Some(200000)));
}

#[test]
fn does_not_treat_http_429_style_errors_as_overflow() {
    let message = create_error_message("Too many requests. Please slow down.");
    assert!(!is_context_overflow(&message, Some(200000)));
}

#[test]
fn detects_xiaomi_style_overflow_length_stop_with_zero_output_and_filled_context() {
    let message = create_length_stop_message(LengthStopOptions {
        input: 58,
        cache_read: 1048512,
        output: 0,
        cache_write: None,
        api: None,
        provider: Some("xiaomi"),
        model: Some("mimo-v2.5-pro"),
    });
    assert!(is_context_overflow(&message, Some(1048576)));
}

#[test]
fn treats_a_length_stop_below_the_desired_output_limit_as_recoverable() {
    let message = create_length_stop_message(LengthStopOptions {
        input: 3,
        cache_read: 253584,
        cache_write: Some(25554),
        output: 16,
        api: Some("openai-responses"),
        provider: Some("openai"),
        model: Some("gpt-5.6-sol"),
    });
    assert!(is_recoverable_length(&message, 128000));
}

#[test]
fn does_not_recover_a_length_stop_that_reached_the_desired_output_limit() {
    let message = create_length_stop_message(LengthStopOptions {
        input: 4062,
        cache_read: 0,
        output: 1024,
        cache_write: None,
        api: None,
        provider: None,
        model: None,
    });
    assert!(!is_recoverable_length(&message, 1024));
}

#[test]
fn treats_zero_output_length_stops_as_recoverable_without_context_metadata() {
    let message = create_length_stop_message(LengthStopOptions {
        input: 100,
        cache_read: 0,
        output: 0,
        cache_write: None,
        api: None,
        provider: None,
        model: None,
    });
    assert!(is_recoverable_length(&message, 128000));
}

#[test]
fn does_not_treat_normal_length_stops_with_output_as_context_overflow() {
    let message = create_length_stop_message(LengthStopOptions {
        input: 1000,
        cache_read: 0,
        output: 4096,
        cache_write: None,
        api: None,
        provider: None,
        model: None,
    });
    assert!(!is_context_overflow(&message, Some(200000)));
}

#[test]
fn does_not_treat_zero_output_length_stops_far_below_context_as_context_overflow() {
    let message = create_length_stop_message(LengthStopOptions {
        input: 100,
        cache_read: 0,
        output: 0,
        cache_write: None,
        api: None,
        provider: None,
        model: None,
    });
    assert!(!is_context_overflow(&message, Some(200000)));
}
