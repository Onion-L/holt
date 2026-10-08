//! Port of `pi-core/ai/src/utils/error-body.ts`: shared normalization and
//! composition of provider HTTP error bodies.
//!
//! TypeScript probes SDK error objects for status/body fields because JS
//! errors carry dynamic properties. Rust errors are typed, so the port takes
//! the status/body/message triple directly and preserves the observable
//! composition behavior: `message_carries_body`, body truncation, and the
//! `formatProviderError` display strings.

/// Port of `MAX_PROVIDER_ERROR_BODY_CHARS`.
pub const MAX_PROVIDER_ERROR_BODY_CHARS: usize = 4000;

/// Port of `NormalizedProviderError`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedProviderError {
    /// HTTP status code, when one could be extracted.
    pub status: Option<u16>,
    /// Raw HTTP body reason, already trimmed and truncated to the cap.
    pub body: Option<String>,
    /// `error.message`, or `safeJsonStringify(error)` for a non-`Error` throw.
    pub message: String,
    /// True when `message` already contains the body (no separate body to add).
    pub message_carries_body: bool,
}

/// The raw failure inputs a provider extracts from its HTTP/SDK layer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderErrorParts {
    pub status: Option<u16>,
    pub body: Option<String>,
    pub message: String,
}

/// Port of `normalizeProviderError` over the typed failure parts. The body is
/// trimmed, dropped when empty, and truncated to the cap; a message that
/// already contains the body is flagged so providers avoid double-printing.
pub fn normalize_provider_error(parts: ProviderErrorParts) -> NormalizedProviderError {
    let body = parts.body.and_then(|body| {
        let trimmed = body.trim().to_string();
        (!trimmed.is_empty()).then(|| truncate_error_text(&trimmed, MAX_PROVIDER_ERROR_BODY_CHARS))
    });
    let message_carries_body = match &body {
        None => true,
        Some(body) => parts.message.contains(body),
    };
    NormalizedProviderError {
        status: parts.status,
        body,
        message: parts.message,
        message_carries_body,
    }
}

/// Port of `formatProviderError`: composes a display string from a normalized
/// error and an optional provider prefix.
///
/// - no prefix: `"<status>: <body>"`
/// - prefix:    `"<prefix> (<status>): <body>"`
pub fn format_provider_error(norm: &NormalizedProviderError, prefix: Option<&str>) -> String {
    if norm.message_carries_body || norm.status.is_none() || norm.body.is_none() {
        return match (prefix, norm.status) {
            (Some(prefix), Some(status)) => format!("{prefix} ({status}): {}", norm.message),
            _ => norm.message.clone(),
        };
    }
    match prefix {
        Some(prefix) => format!(
            "{prefix} ({}): {}",
            norm.status.unwrap(),
            norm.body.clone().unwrap()
        ),
        None => format!("{}: {}", norm.status.unwrap(), norm.body.clone().unwrap()),
    }
}

/// Port of `truncateErrorText`.
pub fn truncate_error_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars).collect();
    let removed = text.chars().count() - max_chars;
    format!("{truncated}... [truncated {removed} chars]")
}

/// Port of `safeJsonStringify`.
pub fn safe_json_stringify(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalize(
        status: Option<u16>,
        body: Option<&str>,
        message: &str,
    ) -> NormalizedProviderError {
        normalize_provider_error(ProviderErrorParts {
            status,
            body: body.map(str::to_string),
            message: message.to_string(),
        })
    }

    #[test]
    fn extracts_status_and_body() {
        let norm = normalize(
            Some(403),
            Some(r#"{"error":"blocked by gateway WAF"}"#),
            "Mistral request failed",
        );
        assert_eq!(norm.status, Some(403));
        assert_eq!(
            norm.body.as_deref(),
            Some(r#"{"error":"blocked by gateway WAF"}"#)
        );
        assert!(!norm.message_carries_body);
    }

    #[test]
    fn preserves_message_when_it_already_carries_the_body() {
        let body =
            serde_json::json!({"error": {"code": 403, "message": "Permission denied"}}).to_string();
        let norm = normalize(Some(403), None, &body);
        assert_eq!(norm.status, Some(403));
        assert!(norm.message_carries_body);
        assert_eq!(norm.message, body);
    }

    #[test]
    fn treats_empty_body_as_no_body() {
        let norm = normalize(Some(403), Some(""), "403 status code (no body)");
        assert!(norm.body.is_none());
        assert!(norm.message_carries_body);
    }

    #[test]
    fn flags_message_that_contains_the_body() {
        let norm = normalize(
            Some(500),
            Some("upstream exploded"),
            "500: upstream exploded",
        );
        assert!(norm.message_carries_body);
    }

    #[test]
    fn truncates_the_body_at_the_cap() {
        let long_body = "x".repeat(MAX_PROVIDER_ERROR_BODY_CHARS + 50);
        let norm = normalize(Some(500), Some(&long_body), "failed");
        let body = norm.body.expect("body present");
        assert!(body.contains("... [truncated 50 chars]"));
        assert!(body.len() < long_body.len());
    }

    #[test]
    fn formats_status_and_body_with_and_without_prefix() {
        let norm = normalize(
            Some(403),
            Some(r#"{"error":"blocked by gateway WAF"}"#),
            "403 status code (no body)",
        );
        assert_eq!(
            format_provider_error(&norm, Some("OpenAI API error")),
            r#"OpenAI API error (403): {"error":"blocked by gateway WAF"}"#
        );
        assert_eq!(
            format_provider_error(&norm, None),
            r#"403: {"error":"blocked by gateway WAF"}"#
        );
    }

    #[test]
    fn preserves_message_with_prefix_when_it_carries_the_body() {
        let body = serde_json::json!({"error": {"message": "Permission denied"}}).to_string();
        let norm = normalize(Some(403), None, &body);
        assert_eq!(
            format_provider_error(&norm, Some("OpenAI API error")),
            format!("OpenAI API error (403): {body}")
        );
    }

    #[test]
    fn returns_plain_message_without_status() {
        let norm = normalize(None, None, "network failure");
        assert_eq!(
            format_provider_error(&norm, Some("Prefix")),
            "network failure"
        );
    }

    // Port of "reads the parsed body off an openai APIError when the message is
    // opaque". In TypeScript the parsed body lives on `error.error` and
    // `pickBodyText` serializes it with `safeJsonStringify`; Rust providers
    // perform that serialization when they build `ProviderErrorParts`, so the
    // test runs the same serialization before normalizing.
    #[test]
    fn openai_parsed_body_surfaces_when_message_is_opaque() {
        let parsed_body = serde_json::json!({"error": "blocked by gateway WAF"});
        let norm = normalize(
            Some(403),
            Some(&safe_json_stringify(&parsed_body)),
            "403 status code (no body)",
        );
        assert_eq!(norm.status, Some(403));
        assert_eq!(
            norm.body.as_deref(),
            Some(r#"{"error":"blocked by gateway WAF"}"#)
        );
        assert!(!norm.message_carries_body);
        let formatted = format_provider_error(&norm, None);
        assert!(formatted.contains("403"));
        assert!(formatted.contains("blocked by gateway WAF"));
        assert_ne!(formatted, "403 status code (no body)");
    }

    // Port of "extracts status and body from a Bedrock-shaped
    // ServiceException": status from `$metadata.httpStatusCode`, body from the
    // `$response.body` string, message kept as the exception text.
    #[test]
    fn bedrock_service_exception_extracts_status_and_body() {
        let norm = normalize(
            Some(403),
            Some(r#"{"message":"blocked by gateway WAF"}"#),
            "UnknownError",
        );
        assert_eq!(norm.status, Some(403));
        assert_eq!(
            norm.body.as_deref(),
            Some(r#"{"message":"blocked by gateway WAF"}"#)
        );
        assert_eq!(norm.message, "UnknownError");
        assert!(!norm.message_carries_body);
    }

    // Port of "still surfaces a plain parsed JSON body object": the parsed
    // object is serialized with insertion order preserved (serde_json
    // preserve_order matches JSON.stringify).
    #[test]
    fn plain_parsed_json_object_body_is_serialized_to_string() {
        let parsed_body =
            serde_json::json!({"message": "schema validation failed", "field": "tools[0]"});
        let norm = normalize(
            Some(400),
            Some(&safe_json_stringify(&parsed_body)),
            "400 status code (no body)",
        );
        assert_eq!(
            norm.body.as_deref(),
            Some(r#"{"message":"schema validation failed","field":"tools[0]"}"#)
        );
        assert!(!norm.message_carries_body);
    }

    // N/A ports from error-body.test.ts:
    // - "ignores a Bedrock response stream instead of serializing its
    //   internals" and "ignores a class-instance response body without a pipe
    //   method": the JS-only `pipe`/prototype sniffing has no Rust analog —
    //   typed `ProviderErrorParts` cannot carry a stream or class instance as
    //   a body.
    // - "ignores a class-instance `error` field": same reasoning.
    // - "JSON-stringifies a non-Error thrown value": Rust failures are always
    //   typed, so there is no non-Error throw to stringify.
}
