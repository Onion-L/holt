//! Minimal no-Node usage of the pi-telemetry port: runs a small span tree on
//! the in-memory backend and prints the recorded spans.

use pi_core::telemetry::{
    InMemoryTelemetryContext, SpanAttributes, SpanOptions, TelemetryContextExt, TelemetrySpanExt,
};

fn main() {
    let context = InMemoryTelemetryContext::new();

    let result = context.start_span(
        SpanOptions::new("demo.operation").with("kind", "read"),
        |operation| {
            operation.add_event("started", SpanAttributes::new().with("seq", 1.0));
            operation.start_span(
                SpanOptions::new("demo.request").with("provider", "example"),
                |request| {
                    request.set_attributes(SpanAttributes::new().with("response", "cached"));
                    42
                },
            )
        },
    );
    assert_eq!(result, 42);

    for span in context.get_spans() {
        println!(
            "span id={} parent={:?} name={} settled={} events={}",
            span.id,
            span.parent_id,
            span.name,
            span.settled,
            span.events.len(),
        );
    }
}
