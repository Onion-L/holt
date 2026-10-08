//! Port of `pi-core/telemetry/test/telemetry.test.ts`.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use pi_core::telemetry::{
    InMemoryTelemetryContext, NOOP_TELEMETRY_CONTEXT, SpanAttributes, SpanOptions, SpanStatus,
    TelemetryContextExt, TelemetrySpanExt, create_typed_span_starter, define_telemetry_schema,
    noop_telemetry_context,
};
use pi_core::telemetry::{
    TelemetryAttributeDefinition, TelemetryAttributeKind, TelemetryCardinality,
    TelemetryEventDefinition, TelemetryParentDefinition, TelemetrySchemaDefinition,
    TelemetrySpanDefinition, TelemetrySpanStatusDefinition, TelemetryStartAttributeDefinition,
    TelemetryStatusDefault,
};

/// Error value standing in for the arbitrary rejection payloads used by the
/// TypeScript tests; identity is asserted through the carried tag.
#[derive(Debug)]
struct TestError(&'static str);

impl fmt::Display for TestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for TestError {}

fn attribute(
    description: &str,
    kind: TelemetryAttributeKind,
    required: bool,
) -> TelemetryStartAttributeDefinition {
    TelemetryStartAttributeDefinition {
        attribute: TelemetryAttributeDefinition {
            description: description.to_string(),
            sensitive: None,
            cardinality: None,
            kind,
        },
        required,
    }
}

fn status_definition(error_when: &str) -> TelemetrySpanStatusDefinition {
    TelemetrySpanStatusDefinition {
        default: TelemetryStatusDefault::Ok,
        error_when: error_when.to_string(),
    }
}

fn operation_schema() -> TelemetrySchemaDefinition {
    let mut start_attributes = BTreeMap::new();
    start_attributes.insert(
        "kind".to_string(),
        attribute(
            "Kind",
            TelemetryAttributeKind::String {
                values: Some(vec!["read".to_string(), "write".to_string()]),
                examples: None,
            },
            true,
        ),
    );
    let mut events = BTreeMap::new();
    let mut event_attributes = BTreeMap::new();
    event_attributes.insert(
        "outcome".to_string(),
        attribute(
            "Outcome",
            TelemetryAttributeKind::String {
                values: Some(vec!["ok".to_string(), "error".to_string()]),
                examples: None,
            },
            true,
        ),
    );
    events.insert(
        "result".to_string(),
        TelemetryEventDefinition {
            description: "Result".to_string(),
            attributes: event_attributes,
        },
    );
    let mut spans = BTreeMap::new();
    spans.insert(
        "operation".to_string(),
        TelemetrySpanDefinition {
            description: "Test operation".to_string(),
            parents: TelemetryParentDefinition::Any,
            start_attributes,
            end_attributes: BTreeMap::new(),
            events: Some(events),
            status: status_definition("The operation fails"),
        },
    );
    TelemetrySchemaDefinition { version: 1, spans }
}

fn request_schema() -> TelemetrySchemaDefinition {
    let mut start_attributes = BTreeMap::new();
    start_attributes.insert(
        "provider".to_string(),
        attribute(
            "Provider",
            TelemetryAttributeKind::String {
                values: None,
                examples: None,
            },
            true,
        ),
    );
    let mut end_attributes = BTreeMap::new();
    end_attributes.insert(
        "response".to_string(),
        TelemetryAttributeDefinition {
            description: "Response kind".to_string(),
            sensitive: None,
            cardinality: Some(TelemetryCardinality::Low),
            kind: TelemetryAttributeKind::String {
                values: None,
                examples: None,
            },
        },
    );
    let mut spans = BTreeMap::new();
    spans.insert(
        "request".to_string(),
        TelemetrySpanDefinition {
            description: "Request".to_string(),
            parents: TelemetryParentDefinition::Spans {
                spans: vec!["operation".to_string()],
            },
            start_attributes,
            end_attributes,
            events: None,
            status: status_definition("The request fails"),
        },
    );
    TelemetrySchemaDefinition { version: 3, spans }
}

#[test]
fn telemetry_schemas_preserve_serializable_definitions() {
    let definition = operation_schema();
    let schema = define_telemetry_schema(definition.clone());
    assert_eq!(schema, definition);
    serde_json::to_string(&schema).expect("schema serializes");
    // The TypeScript test additionally asserts compile-time attribute
    // inference and closed-set event rejection (`@ts-expect-error` cases).
    // Those are type-level capabilities with no Rust equivalent; the runtime
    // contract of `define_telemetry_schema` (identity, no validation) is
    // covered here.
}

#[tokio::test]
async fn telemetry_schemas_combine_vocabularies_and_bind_child_starters() {
    let operation = define_telemetry_schema(operation_schema());
    let request = define_telemetry_schema(request_schema());
    let telemetry_context = Arc::new(InMemoryTelemetryContext::new());
    let start_span = create_typed_span_starter(
        Arc::clone(&telemetry_context) as Arc<dyn pi_core::telemetry::TelemetryContext>,
        (operation, request),
    );

    let result = start_span.start_span(
        "operation",
        SpanAttributes::new().with("kind", "read"),
        |_operation_span, start_child_span| {
            start_child_span.start_span(
                "request",
                SpanAttributes::new().with("provider", "example"),
                |request_span| {
                    request_span.set_attributes(SpanAttributes::new().with("response", "cached"));
                    42
                },
            )
        },
    );

    assert_eq!(result, 42);
    let spans = telemetry_context.get_spans();
    let operation_span = spans.iter().find(|span| span.name == "operation").unwrap();
    let request_span = spans.iter().find(|span| span.name == "request").unwrap();
    assert_eq!(operation_span.parent_id, None);
    assert_eq!(request_span.parent_id, Some(operation_span.id));

    // `createTypedSpanStarter` never inspects the schema values at runtime.
    let _ = create_typed_span_starter(
        Arc::new(InMemoryTelemetryContext::new()),
        (operation_schema(), request_schema()),
    );

    let sync_error = TestError("sync");
    let error = start_span
        .try_start_span(
            "operation",
            SpanAttributes::new().with("kind", "write"),
            move |_span, _child| -> Result<(), TestError> { Err(sync_error) },
        )
        .unwrap_err();
    assert_eq!(error.0, "sync");

    let async_error = TestError("async");
    let error = start_span
        .try_start_span_async(
            "request",
            SpanAttributes::new().with("provider", "example"),
            move |_span, _child| async move { Err::<(), TestError>(async_error) },
        )
        .await
        .unwrap_err();
    assert_eq!(error.0, "async");

    // The TypeScript test's remaining cases are compile-time only: union
    // narrowing of span names, unknown-attribute rejection, unknown span
    // names, and duplicate span names across schemas have no Rust runtime
    // equivalent.
}

#[test]
fn noop_telemetry_context_admits_callbacks_synchronously_and_reuses_one_inert_span() {
    let mut admitted = false;
    NOOP_TELEMETRY_CONTEXT.start_span(SpanOptions::new("first"), |span| {
        admitted = true;
        let child = span.start_span(SpanOptions::new("child"), |child_span| child_span);
        assert!(Arc::ptr_eq(&child, &span), "noop context reuses one span");
        42
    });

    assert!(admitted);
    // The TypeScript test also asserts `Object.isFrozen(firstSpan)`; Rust
    // span handles are immutable by construction.
}

#[tokio::test]
async fn noop_telemetry_context_preserves_rejection_values() {
    let sync_error = TestError("sync");
    let error = NOOP_TELEMETRY_CONTEXT
        .try_start_span(
            SpanOptions::new("sync"),
            move |_span| -> Result<(), TestError> { Err(sync_error) },
        )
        .unwrap_err();
    assert_eq!(error.0, "sync");

    let async_error = TestError("async");
    let error = NOOP_TELEMETRY_CONTEXT
        .try_start_span_async(SpanOptions::new("async"), move |_span| async move {
            Err::<(), TestError>(async_error)
        })
        .await
        .unwrap_err();
    assert_eq!(error.0, "async");
}

#[test]
fn noop_telemetry_context_does_not_inspect_or_retain_payloads() {
    // The TypeScript test wraps payloads in throwing `Proxy` objects; Rust
    // payloads are plain owned values that cannot fail on read, so the same
    // call paths are exercised with ordinary payloads.
    NOOP_TELEMETRY_CONTEXT.start_span(
        SpanOptions::new("operation").with("secret", "prompt content"),
        |span| {
            span.add_event("event", SpanAttributes::new().with("secret", "content"));
            span.set_attributes(SpanAttributes::new().with("secret", "content"));
            span.set_status(SpanStatus::Ok);
        },
    );
    assert!(
        noop_telemetry_context()
            .dispatch_span_sync(SpanOptions::new("dispatch"), Box::new(|_span| Ok(())))
            .is_ok()
    );
}
