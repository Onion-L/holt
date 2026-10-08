//! Port of `pi-core/telemetry/test/conformance.test.ts`.

use std::sync::Arc;

use pi_core::telemetry::testing::{
    TelemetryAdapterFixture, TelemetryAdapterFixtureFactory, create_telemetry_adapter_conformance,
};
use pi_core::telemetry::{
    BoxSpanFuture, InMemoryTelemetryContext, RecordedTelemetrySpan, SpanAttributes, SpanOptions,
    TelemetryContext, TelemetryContextExt,
};

struct InMemoryFixture(InMemoryTelemetryContext);

impl TelemetryAdapterFixture for InMemoryFixture {
    fn context(&self) -> Arc<dyn TelemetryContext> {
        Arc::new(self.0.clone())
    }

    fn get_spans(&self) -> BoxSpanFuture<'_, Vec<RecordedTelemetrySpan>> {
        let spans = self.0.get_spans();
        Box::pin(async move { spans })
    }
}

fn in_memory_factory() -> TelemetryAdapterFixtureFactory {
    Arc::new(|| {
        let fixture: Box<dyn TelemetryAdapterFixture> =
            Box::new(InMemoryFixture(InMemoryTelemetryContext::new()));
        Box::pin(async move { fixture })
    })
}

#[test]
fn in_memory_telemetry_context_conformance() {
    let conformance = create_telemetry_adapter_conformance(in_memory_factory());
    assert!(!conformance.is_empty());
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    for test_case in &conformance {
        runtime.block_on(test_case.run());
    }
}

#[test]
fn returns_detached_snapshots_without_exposing_mutable_recording_state() {
    let context = InMemoryTelemetryContext::new();
    let mut open_settled = None;
    let mut open_end_sequence = None;
    context.start_span(
        SpanOptions::new("snapshot").with("tags", vec!["initial".to_string()]),
        |span| {
            span.add_event("event", SpanAttributes::new().with("value", 1.0));
            let open = &context.get_spans()[0];
            open_settled = Some(open.settled);
            open_end_sequence = open.end_sequence;
        },
    );

    assert_eq!(open_settled, Some(false));
    assert_eq!(open_end_sequence, None);
    let first = &context.get_spans()[0];
    assert!(first.settled);
    assert_eq!(first.end_sequence, Some(1));

    // Snapshots are owned copies; mutating them cannot affect the recorder.
    let mut mutated = first.clone();
    mutated.attributes.set("tags", vec!["mutated".to_string()]);
    mutated.events[0].attributes.set("value", 2.0);

    let second = &context.get_spans()[0];
    assert_eq!(
        second.attributes,
        SpanAttributes::new().with("tags", vec!["initial".to_string()])
    );
    assert_eq!(
        second.events,
        vec![pi_core::telemetry::RecordedTelemetryEvent {
            name: "event".to_string(),
            attributes: SpanAttributes::new().with("value", 1.0),
        }]
    );
    assert_ne!(mutated.attributes, second.attributes);
}
