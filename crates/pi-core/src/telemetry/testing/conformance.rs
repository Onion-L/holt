//! Port of `pi-core/telemetry/src/testing/conformance.ts`.
//!
//! TypeScript wraps selected payloads in throwing `Proxy` objects to exercise
//! recorder passivity. Rust payloads are plain owned values that cannot fail
//! on read, so those cases assert the closest representable behavior and
//! document the difference inline. Everything else — synchronous admission,
//! settlement, status precedence, attribute merging, event ordering,
//! parentage, and settlement sequencing — is ported 1:1.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use super::types::{TelemetryAdapterConformanceCase, TelemetryAdapterFixtureFactory, case};
use crate::telemetry::{
    AttributeValue, RecordedTelemetryEvent, RecordedTelemetrySpan, SpanAttributes, SpanErrorInfo,
    SpanOptions, SpanStatus, TelemetryContextExt, TelemetrySpan, TelemetrySpanExt, lock,
};

fn find_span<'a>(spans: &'a [RecordedTelemetrySpan], name: &str) -> &'a RecordedTelemetrySpan {
    spans
        .iter()
        .find(|candidate| candidate.name == name)
        .unwrap_or_else(|| panic!("Expected recorded span {name}"))
}

/// Error value standing in for the arbitrary rejection payloads used by the
/// TypeScript cases. Value identity is asserted through the carried tag.
#[derive(Debug)]
struct CaseError(&'static str);

impl fmt::Display for CaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for CaseError {}

fn expected(name: &'static str, message: &'static str) -> SpanStatus {
    SpanStatus::Error(Some(SpanErrorInfo {
        name: name.to_string(),
        message: message.to_string(),
    }))
}

/// Minimal async gate standing in for the TypeScript promise-based release
/// latch (the conformance module must stay dependency-free).
fn gate() -> (GateRelease, GateWait) {
    let inner = Arc::new(GateInner {
        released: Mutex::new(false),
        waker: Mutex::new(None),
    });
    (
        GateRelease {
            inner: Arc::clone(&inner),
        },
        GateWait { inner },
    )
}

struct GateInner {
    released: Mutex<bool>,
    waker: Mutex<Option<Waker>>,
}

struct GateRelease {
    inner: Arc<GateInner>,
}

impl GateRelease {
    fn release(self) {
        *lock(&self.inner.released) = true;
        if let Some(waker) = lock(&self.inner.waker).take() {
            waker.wake();
        }
    }
}

struct GateWait {
    inner: Arc<GateInner>,
}

impl Future for GateWait {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if *lock(&self.inner.released) {
            Poll::Ready(())
        } else {
            *lock(&self.inner.waker) = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// Creates runner-independent cases for the callback telemetry adapter
/// contract. Port of `createTelemetryAdapterConformance`.
pub fn create_telemetry_adapter_conformance(
    factory: TelemetryAdapterFixtureFactory,
) -> Vec<TelemetryAdapterConformanceCase> {
    vec![
        // "callback lifecycle" / "admits once synchronously and preserves the result"
        case(
            &factory,
            "callback lifecycle",
            "admits once synchronously and preserves the result",
            |fixture| async move {
                let context = fixture.context();
                let mut admitted = false;
                let mut calls = 0;
                let result = context.start_span(SpanOptions::new("success"), |_span| {
                    admitted = true;
                    calls += 1;
                    42
                });

                assert!(admitted);
                assert_eq!(calls, 1);
                assert_eq!(result, 42);
                let spans = fixture.get_spans().await;
                let span = find_span(&spans, "success");
                assert_eq!(span.status, SpanStatus::Ok);
                assert!(span.settled);
            },
        ),
        // "callback lifecycle" / "preserves synchronous and asynchronous rejection values"
        case(
            &factory,
            "callback lifecycle",
            "preserves synchronous and asynchronous rejection values",
            |fixture| async move {
                let context = fixture.context();

                let error = context
                    .try_start_span(
                        SpanOptions::new("sync-error"),
                        |_span| -> Result<(), CaseError> { Err(CaseError("sync")) },
                    )
                    .unwrap_err();
                assert_eq!(error.0, "sync");

                let error = context
                    .try_start_span_async(SpanOptions::new("async-error"), |_span| async {
                        Err::<(), CaseError>(CaseError("async"))
                    })
                    .await
                    .unwrap_err();
                assert_eq!(error.0, "async");

                // The TypeScript case rejects with `undefined`; Rust carries the
                // same emptiness as an error value instead.
                let error = context
                    .try_start_span(
                        SpanOptions::new("undefined-error"),
                        |_span| -> Result<(), CaseError> { Err(CaseError("undefined")) },
                    )
                    .unwrap_err();
                assert_eq!(error.0, "undefined");

                // The remaining TypeScript rejections carry `Proxy` values whose
                // inspection throws; Rust values cannot fail to read, so ordinary
                // error values stand in for them.
                let error = context
                    .try_start_span(
                        SpanOptions::new("unreadable-error"),
                        |_span| -> Result<(), CaseError> { Err(CaseError("unreadable")) },
                    )
                    .unwrap_err();
                assert_eq!(error.0, "unreadable");

                let error = context
                    .try_start_span_async(
                        SpanOptions::new("async-unreadable-error"),
                        |_span| async { Err::<(), CaseError>(CaseError("async-unreadable")) },
                    )
                    .await
                    .unwrap_err();
                assert_eq!(error.0, "async-unreadable");

                let spans = fixture.get_spans().await;
                for name in [
                    "sync-error",
                    "async-error",
                    "undefined-error",
                    "unreadable-error",
                    "async-unreadable-error",
                ] {
                    assert!(
                        matches!(find_span(&spans, name).status, SpanStatus::Error(_)),
                        "expected error status for {name}"
                    );
                }
            },
        ),
        // "status" / "uses last explicit status without automatic overwrite"
        case(
            &factory,
            "status",
            "uses last explicit status without automatic overwrite",
            |fixture| async move {
                let context = fixture.context();
                context.start_span(SpanOptions::new("last-status"), |span| {
                    span.set_status(expected("Expected", "first"));
                    span.set_status(SpanStatus::Ok);
                });

                let error = context
                    .try_start_span(
                        SpanOptions::new("explicit-before-throw"),
                        |span| -> Result<(), CaseError> {
                            span.set_status(SpanStatus::Ok);
                            Err(CaseError("after explicit status"))
                        },
                    )
                    .unwrap_err();
                assert_eq!(error.0, "after explicit status");

                let error = context
                    .try_start_span_async(
                        SpanOptions::new("explicit-before-rejection"),
                        |span| async move {
                            span.set_status(expected("Expected", "async failure"));
                            Err::<(), CaseError>(CaseError("after async explicit status"))
                        },
                    )
                    .await
                    .unwrap_err();
                assert_eq!(error.0, "after async explicit status");

                context
                    .try_start_span(
                        SpanOptions::new("expected-failure"),
                        |span| -> Result<(), CaseError> {
                            span.set_status(expected("Expected", "returned failure"));
                            Ok(())
                        },
                    )
                    .unwrap();

                let spans = fixture.get_spans().await;
                assert_eq!(find_span(&spans, "last-status").status, SpanStatus::Ok);
                assert_eq!(
                    find_span(&spans, "explicit-before-throw").status,
                    SpanStatus::Ok
                );
                assert_eq!(
                    find_span(&spans, "explicit-before-rejection").status,
                    expected("Expected", "async failure")
                );
                assert_eq!(
                    find_span(&spans, "expected-failure").status,
                    expected("Expected", "returned failure")
                );
            },
        ),
        // "recording" / "merges attributes and records ordered events"
        case(
            &factory,
            "recording",
            "merges attributes and records ordered events",
            |fixture| async move {
                let context = fixture.context();
                context.start_span(
                    SpanOptions::new("recording")
                        .with("start", "value")
                        .with("overwrite", "start"),
                    |span| {
                        span.set_attributes(
                            SpanAttributes::new()
                                .with("count", 1.0)
                                .with("overwrite", "middle"),
                        );
                        // TypeScript includes `count: undefined`, which recorders
                        // skip; Rust callers omit the key to the same effect.
                        span.set_attributes(SpanAttributes::new().with("overwrite", "end"));
                        span.add_event("first", SpanAttributes::new().with("index", 1.0));
                        span.add_event("second", SpanAttributes::new().with("index", 2.0));
                    },
                );

                let spans = fixture.get_spans().await;
                let span = find_span(&spans, "recording");
                assert_eq!(
                    span.attributes,
                    SpanAttributes::new()
                        .with("start", "value")
                        .with("overwrite", "end")
                        .with("count", 1.0)
                );
                assert_eq!(
                    span.events,
                    vec![
                        RecordedTelemetryEvent {
                            name: "first".to_string(),
                            attributes: SpanAttributes::new().with("index", 1.0),
                        },
                        RecordedTelemetryEvent {
                            name: "second".to_string(),
                            attributes: SpanAttributes::new().with("index", 2.0),
                        },
                    ]
                );
            },
        ),
        // "recording" / "ignores failed attribute calls atomically"
        case(
            &factory,
            "recording",
            "ignores failed attribute calls atomically",
            |fixture| async move {
                // The TypeScript case feeds a `Proxy` that throws mid-iteration
                // and requires the partial write to be discarded. Rust attribute
                // maps are plain owned values that cannot fail mid-iteration, so
                // the case verifies that merged writes never throw and every
                // provided key is recorded.
                let context = fixture.context();
                context.start_span(
                    SpanOptions::new("atomic-attributes").with("retained", "value"),
                    |span| {
                        span.set_attributes(
                            SpanAttributes::new().with("partial", "must not survive"),
                        );
                    },
                );

                let spans = fixture.get_spans().await;
                let span = find_span(&spans, "atomic-attributes");
                assert_eq!(
                    span.attributes,
                    SpanAttributes::new()
                        .with("retained", "value")
                        .with("partial", "must not survive")
                );
            },
        ),
        // "recording" / "makes calls after settlement inert"
        case(
            &factory,
            "recording",
            "makes calls after settlement inert",
            |fixture| async move {
                let context = fixture.context();
                let mut captured: Option<Arc<dyn TelemetrySpan>> = None;
                context.start_span(
                    SpanOptions::new("settled").with("value", "initial"),
                    |span| {
                        captured = Some(Arc::clone(&span));
                    },
                );
                let span = captured.expect("expected callback span");

                span.set_attributes(SpanAttributes::new().with("value", "late"));
                span.add_event("late", SpanAttributes::new().with("value", true));
                span.set_status(SpanStatus::Error(None));
                let child = span.start_span(SpanOptions::new("late-child"), |_child| 7);
                assert_eq!(child, 7);

                let spans = fixture.get_spans().await;
                assert_eq!(spans.len(), 1);
                assert_eq!(
                    spans[0].attributes,
                    SpanAttributes::new().with("value", "initial")
                );
                assert!(spans[0].events.is_empty());
                assert_eq!(spans[0].status, SpanStatus::Ok);
            },
        ),
        // "parentage" / "records nested and concurrent child relationships"
        case(
            &factory,
            "parentage",
            "records nested and concurrent child relationships",
            |fixture| async move {
                let context = fixture.context();
                let (release_first, first_gate) = gate();
                context
                    .start_span_async(SpanOptions::new("parent"), move |parent| async move {
                        let first = parent.start_span_async(
                            SpanOptions::new("first-child"),
                            |_child| async move {
                                first_gate.await;
                            },
                        );
                        let second =
                            parent.start_span(SpanOptions::new("second-child"), |_child| "done");
                        assert_eq!(second, "done");
                        release_first.release();
                        first.await;
                    })
                    .await;

                let spans = fixture.get_spans().await;
                let parent = find_span(&spans, "parent");
                let first = find_span(&spans, "first-child");
                let second = find_span(&spans, "second-child");
                assert_eq!(parent.parent_id, None);
                assert_eq!(first.parent_id, Some(parent.id));
                assert_eq!(second.parent_id, Some(parent.id));
                let (parent_end, first_end, second_end) = (
                    parent.end_sequence.expect("parent end sequence"),
                    first.end_sequence.expect("first end sequence"),
                    second.end_sequence.expect("second end sequence"),
                );
                assert!(second_end < first_end);
                assert!(first_end < parent_end);
            },
        ),
        // "passivity" / "suppresses unreadable telemetry payload failures"
        case(
            &factory,
            "passivity",
            "suppresses unreadable telemetry payload failures",
            |fixture| async move {
                // The TypeScript case wraps options and payloads in throwing
                // `Proxy` objects and requires recording to stay passive. Rust
                // payloads cannot fail on read, so the case verifies the same
                // call paths flow through with ordinary payloads and without
                // inspecting them.
                let context = fixture.context();
                let mut calls = 0;
                let result = context.start_span(
                    SpanOptions::new("unreadable-options").with("secret", "value"),
                    |_span| {
                        calls += 1;
                        9
                    },
                );
                assert_eq!(calls, 1);
                assert_eq!(result, 9);

                context.start_span(SpanOptions::new("unreadable-recording"), |span| {
                    span.set_attributes(SpanAttributes::new().with("secret", "value"));
                    span.add_event(
                        "unreadable-event",
                        SpanAttributes::new().with("secret", "value"),
                    );
                    span.set_status(SpanStatus::Ok);
                });

                let recorded = fixture.get_spans().await;
                assert_eq!(recorded.len(), 2);
                assert_eq!(
                    find_span(&recorded, "unreadable-recording").attributes,
                    SpanAttributes::new().with("secret", "value")
                );
                assert!(matches!(
                    find_span(&recorded, "unreadable-recording").events[0]
                        .attributes
                        .get("secret"),
                    Some(AttributeValue::String(_))
                ));
                assert_eq!(
                    find_span(&recorded, "unreadable-recording").status,
                    SpanStatus::Ok
                );
            },
        ),
        // "passivity" / "ignores failed status calls atomically"
        case(
            &factory,
            "passivity",
            "ignores failed status calls atomically",
            |fixture| async move {
                // The TypeScript case feeds a throwing `Proxy` status, so the
                // explicit write fails and the automatic error status applies. In
                // Rust a status value cannot fail to read, so the observable
                // residue of the same rule is asserted: an explicitly set status
                // is never overwritten by the automatic one.
                let context = fixture.context();
                let error = context
                    .try_start_span(
                        SpanOptions::new("unreadable-status"),
                        |span| -> Result<(), CaseError> {
                            span.set_status(SpanStatus::Ok);
                            Err(CaseError("rejected after explicit status"))
                        },
                    )
                    .unwrap_err();
                assert_eq!(error.0, "rejected after explicit status");

                assert_eq!(
                    find_span(&fixture.get_spans().await, "unreadable-status").status,
                    SpanStatus::Ok
                );
            },
        ),
    ]
}
