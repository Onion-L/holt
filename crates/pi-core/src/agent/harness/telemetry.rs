//! Port of `pi-core/agent/src/harness/telemetry.ts`.
//!
//! The schema constants are embedded verbatim from the TypeScript oracle
//! (`scripts/oracle/export-agent-telemetry-schemas.mts` writes
//! `data/telemetry-schemas.json`), so the span vocabularies match exactly.
//! TypeScript's compile-time conditional types (exact start-attribute
//! inference, event-name unions) have no Rust equivalent — the runtime
//! helpers bind contexts and forward names and attributes like the
//! originals.

use std::sync::Arc;

use crate::telemetry::{
    AttributeValue, SpanAttributes, TelemetryContext, TelemetryContextExt,
    TelemetrySchemaDefinition, TelemetrySpan,
};

/// The embedded schema payload produced by the TypeScript oracle.
const SCHEMA_JSON: &str = include_str!("data/telemetry-schemas.json");

#[derive(serde::Deserialize)]
struct SchemaPayload {
    ai: TelemetrySchemaDefinition,
    harness: TelemetrySchemaDefinition,
}

fn payload() -> &'static SchemaPayload {
    static PAYLOAD: std::sync::OnceLock<SchemaPayload> = std::sync::OnceLock::new();
    PAYLOAD.get_or_init(|| {
        serde_json::from_str(SCHEMA_JSON).expect("embedded telemetry schemas are valid")
    })
}

/// Port of `AI_TELEMETRY_SCHEMA`.
pub fn ai_telemetry_schema() -> &'static TelemetrySchemaDefinition {
    &payload().ai
}

/// Port of `HARNESS_TELEMETRY_SCHEMA`.
pub fn harness_telemetry_schema() -> &'static TelemetrySchemaDefinition {
    &payload().harness
}

/// The harness span names in TypeScript declaration order (the typed maps
/// sort alphabetically; the oracle payload preserves insertion order).
pub fn harness_span_names() -> Vec<String> {
    span_names_in_order(SCHEMA_JSON, "harness")
}

/// The AI span names in TypeScript declaration order.
pub fn ai_span_names() -> Vec<String> {
    span_names_in_order(SCHEMA_JSON, "ai")
}

fn span_names_in_order(payload: &str, schema: &str) -> Vec<String> {
    let value: serde_json::Value =
        serde_json::from_str(payload).expect("embedded telemetry schemas are valid JSON");
    value[schema]["spans"]
        .as_object()
        .expect("spans object")
        .keys()
        .cloned()
        .collect()
}

/// Port of `AGENT_TELEMETRY_SCHEMAS`.
pub fn agent_telemetry_schemas() -> Vec<&'static TelemetrySchemaDefinition> {
    vec![ai_telemetry_schema(), harness_telemetry_schema()]
}

/// The `pi.ai.request` start-attribute keys (helper mirroring the typed
/// start-attribute union).
pub const AI_REQUEST_START_ATTRIBUTES: &[&str] = &[
    "pi.ai.operation",
    "pi.ai.provider",
    "pi.ai.model",
    "pi.ai.api",
    "pi.ai.streaming",
];

/// Port of `startAiSpan`.
pub async fn start_ai_span<T, F, Fut>(
    telemetry_context: &Arc<dyn TelemetryContext>,
    name: &str,
    attributes: SpanAttributes,
    callback: F,
) -> T
where
    T: Send + 'static,
    F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
{
    telemetry_context
        .start_span_async(
            crate::telemetry::SpanOptions {
                name: name.to_string(),
                attributes,
            },
            callback,
        )
        .await
}

/// Port of `startHarnessSpan`.
pub async fn start_harness_span<T, F, Fut>(
    telemetry_context: &Arc<dyn TelemetryContext>,
    name: &str,
    attributes: SpanAttributes,
    callback: F,
) -> T
where
    T: Send + 'static,
    F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
{
    telemetry_context
        .start_span_async(
            crate::telemetry::SpanOptions {
                name: name.to_string(),
                attributes,
            },
            callback,
        )
        .await
}

/// Convenience helper mirroring the composed typed starter over
/// [`crate::telemetry::create_typed_span_starter`] with both agent schemas.
pub fn agent_typed_span_starter()
-> crate::telemetry::TypedSpanStarter<Vec<&'static TelemetrySchemaDefinition>> {
    let context: Arc<dyn TelemetryContext> = Arc::new(crate::telemetry::NoopTelemetryContext);
    crate::telemetry::create_typed_span_starter(context, agent_telemetry_schemas())
}

/// Builds a one-attribute map (`SpanAttributes` in the public docs).
pub fn span_attributes(
    entries: impl IntoIterator<Item = (impl Into<String>, AttributeValue)>,
) -> SpanAttributes {
    let mut attributes = SpanAttributes::new();
    for (name, value) in entries {
        attributes.set(name, value);
    }
    attributes
}

/// The schema versions exposed for host-side registration.
pub fn agent_schema_versions() -> Vec<u64> {
    agent_telemetry_schemas()
        .into_iter()
        .map(|schema| schema.version)
        .collect()
}
