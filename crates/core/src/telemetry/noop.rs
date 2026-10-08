//! Port of `pi-core/telemetry/src/noop.ts`: the shared telemetry context used
//! when an application does not provide one. All recording methods are inert
//! and one frozen span object is reused for every start, including children.

use std::sync::{Arc, LazyLock};

use super::{
    BoxSpanFuture, SpanAttributes, SpanBodyAsync, SpanBodySync, SpanErrorInfo, SpanOptions,
    SpanStatus, TelemetryContext, TelemetrySpan,
};

/// Port of the frozen `noopTelemetrySpan` object.
struct NoopSpan;

impl TelemetrySpan for NoopSpan {
    fn add_event(&self, _name: &str, _attributes: SpanAttributes) {}

    fn set_attributes(&self, _attributes: SpanAttributes) {}

    fn set_status(&self, _status: SpanStatus) {}

    fn dispatch_child_span_sync(
        &self,
        _options: SpanOptions,
        body: SpanBodySync<'_>,
    ) -> Result<(), SpanErrorInfo> {
        body(noop_span())
    }

    fn dispatch_child_span_async<'a>(
        &'a self,
        _options: SpanOptions,
        body: SpanBodyAsync<'a>,
    ) -> BoxSpanFuture<'a, Result<(), SpanErrorInfo>> {
        dispatch_noop_async(body)
    }
}

fn noop_span() -> Arc<dyn TelemetrySpan> {
    static NOOP_SPAN: LazyLock<Arc<dyn TelemetrySpan>> =
        LazyLock::new(|| Arc::new(NoopSpan) as Arc<dyn TelemetrySpan>);
    Arc::clone(&*NOOP_SPAN)
}

pub(crate) fn dispatch_noop_sync(body: SpanBodySync<'_>) -> Result<(), SpanErrorInfo> {
    body(noop_span())
}

pub(crate) fn dispatch_noop_async<'a>(
    body: SpanBodyAsync<'a>,
) -> BoxSpanFuture<'a, Result<(), SpanErrorInfo>> {
    Box::pin(async move { body(noop_span()).await })
}

/// Port of `NOOP_TELEMETRY_CONTEXT`.
///
/// TypeScript reuses the frozen noop span object as the context itself; the
/// Rust port keeps them as separate types while preserving the observable
/// behavior: callbacks are admitted synchronously, children resolve to the
/// same inert span, and payloads are never inspected.
pub struct NoopTelemetryContext;

impl TelemetryContext for NoopTelemetryContext {
    fn dispatch_span_sync(
        &self,
        _options: SpanOptions,
        body: SpanBodySync<'_>,
    ) -> Result<(), SpanErrorInfo> {
        dispatch_noop_sync(body)
    }

    fn dispatch_span_async<'a>(
        &'a self,
        _options: SpanOptions,
        body: SpanBodyAsync<'a>,
    ) -> BoxSpanFuture<'a, Result<(), SpanErrorInfo>> {
        dispatch_noop_async(body)
    }
}

/// Port of the exported `NOOP_TELEMETRY_CONTEXT` constant.
pub static NOOP_TELEMETRY_CONTEXT: NoopTelemetryContext = NoopTelemetryContext;

/// Returns the shared noop context as a trait object, for storage in option
/// fields that hold [`Arc<dyn TelemetryContext>`].
pub fn noop_telemetry_context() -> &'static dyn TelemetryContext {
    &NOOP_TELEMETRY_CONTEXT
}
