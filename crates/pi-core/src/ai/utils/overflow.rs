//! Port of `pi-core/ai/src/utils/overflow.ts`: context overflow detection.

use regex::Regex;
use std::sync::OnceLock;

use crate::ai::types::{AssistantMessage, StopReason};

/// Port of the `OVERFLOW_PATTERNS` list (case-insensitive).
fn overflow_patterns() -> &'static Vec<Regex> {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"prompt is too long",
            r"request_too_large",
            r"input is too long for requested model",
            r"exceeds the context window",
            r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
            r"input token count.*exceeds the maximum",
            r"maximum prompt length is \d+",
            r"reduce the length of the messages",
            r"maximum context length is \d+ tokens",
            r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
            r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)",
            r"exceeds the limit of \d+",
            r"exceeds the available context size",
            r"greater than the context length",
            r"context window exceeds limit",
            r"exceeded model token limit",
            r"too large for model with \d+ maximum context length",
            r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
            r"model_context_window_exceeded",
            r"prompt too long; exceeded (?:max )?context length",
            r"range of input length should be",
            r"context[_ ]length[_ ]exceeded",
            r"too many tokens",
            r"token limit exceeded",
            r"^4(?:00|13)\s*(?:status code)?\s*\(no body\)",
        ]
        .into_iter()
        .map(|pattern| Regex::new(&format!("(?i){pattern}")).expect("overflow pattern compiles"))
        .collect()
    })
}

/// Port of the `NON_OVERFLOW_PATTERNS` list.
fn non_overflow_patterns() -> &'static Vec<Regex> {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"^(Throttling error|Service unavailable):",
            r"rate limit",
            r"too many requests",
        ]
        .into_iter()
        .map(|pattern| {
            Regex::new(&format!("(?i){pattern}")).expect("non-overflow pattern compiles")
        })
        .collect()
    })
}

/// Port of `isContextOverflow`.
pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool {
    // Case 1: error message patterns.
    if message.stop_reason == StopReason::Error
        && let Some(error_message) = &message.error_message
    {
        let is_non_overflow = non_overflow_patterns()
            .iter()
            .any(|p| p.is_match(error_message));
        if !is_non_overflow
            && overflow_patterns()
                .iter()
                .any(|p| p.is_match(error_message))
        {
            return true;
        }
    }

    let Some(context_window) = context_window else {
        return false;
    };
    let context_window = context_window as f64;
    let input_tokens = (message.usage.input + message.usage.cache_read) as f64;

    // Case 2: silent overflow (z.ai style).
    if message.stop_reason == StopReason::Stop && input_tokens > context_window {
        return true;
    }

    // Case 3: length-stop overflow (Xiaomi MiMo style).
    if message.stop_reason == StopReason::Length
        && message.usage.output == 0
        && input_tokens >= context_window * 0.99
    {
        return true;
    }

    false
}

/// Port of `isRecoverableLength`.
pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: u64) -> bool {
    message.stop_reason == StopReason::Length
        && desired_max_output > 0
        && (message.usage.output as f64) < desired_max_output as f64
}

/// Port of `getOverflowPatterns` (for tests).
pub fn get_overflow_patterns() -> &'static Vec<Regex> {
    overflow_patterns()
}
