//! Port of `pi-core/ai/src/utils/diagnostics.ts`.

use std::any::type_name;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ai::types::{AssistantMessage, AssistantMessageDiagnostic, DiagnosticErrorInfo};

fn short_type_name<E: ?Sized>() -> String {
    let full = type_name::<E>();
    full.rsplit("::").next().unwrap_or(full).to_string()
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

/// Port of `formatThrownValue`: renders an error's message, falling back to
/// its short type name when the message is empty. TypeScript's
/// string/other-value branches have no Rust equivalent.
pub fn format_thrown_value<E: std::error::Error + ?Sized>(error: &E) -> String {
    let message = error.to_string();
    if message.is_empty() {
        short_type_name::<E>()
    } else {
        message
    }
}

/// Port of `extractDiagnosticError`.
///
/// TypeScript reads `error.name` and `error.stack`; Rust errors carry neither
/// field, so the short type name substitutes for `name` and `stack` is always
/// `None`. `code` requires a provider-specific error contract and stays
/// `None` unless callers enrich the info themselves.
pub fn extract_diagnostic_error<E: std::error::Error + ?Sized>(error: &E) -> DiagnosticErrorInfo {
    let message = error.to_string();
    let name = short_type_name::<E>();
    DiagnosticErrorInfo {
        name: Some(name.clone()),
        message: if message.is_empty() { name } else { message },
        stack: None,
        code: None,
    }
}

/// Port of `createAssistantMessageDiagnostic`.
pub fn create_assistant_message_diagnostic<E: std::error::Error + ?Sized>(
    kind: &str,
    error: &E,
    details: Option<serde_json::Value>,
) -> AssistantMessageDiagnostic {
    AssistantMessageDiagnostic {
        kind: kind.to_string(),
        timestamp: now_millis(),
        error: Some(extract_diagnostic_error(error)),
        details,
    }
}

/// Port of `appendAssistantMessageDiagnostic`.
pub fn append_assistant_message_diagnostic(
    message: &mut AssistantMessage,
    diagnostic: AssistantMessageDiagnostic,
) {
    message
        .diagnostics
        .get_or_insert_with(Vec::new)
        .push(diagnostic);
}
