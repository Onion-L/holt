//! Port of `pi-core/telemetry/src/memory.ts`:
//! [`InMemoryTelemetryContext`], the backend-neutral reference implementation
//! that records spans in process memory.

use std::sync::{Arc, Mutex};

use super::noop::{dispatch_noop_async, dispatch_noop_sync};
use super::{
    BoxSpanFuture, SpanAttributes, SpanBodyAsync, SpanBodySync, SpanErrorInfo, SpanOptions,
    SpanStatus, TelemetryContext, TelemetrySpan, lock,
};

/// Port of `RecordedTelemetryEvent`.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedTelemetryEvent {
    pub name: String,
    pub attributes: SpanAttributes,
}

/// Port of `RecordedTelemetrySpan`: a detached snapshot of one recorded span.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedTelemetrySpan {
    pub id: u64,
    pub parent_id: Option<u64>,
    pub name: String,
    pub attributes: SpanAttributes,
    pub events: Vec<RecordedTelemetryEvent>,
    pub status: SpanStatus,
    pub settled: bool,
    pub end_sequence: Option<u64>,
}

struct MutableSpan {
    id: u64,
    parent_id: Option<u64>,
    name: String,
    attributes: SpanAttributes,
    events: Vec<RecordedTelemetryEvent>,
    status: SpanStatus,
    explicit_status: bool,
    settled: bool,
    end_sequence: Option<u64>,
}

struct State {
    spans: Vec<MutableSpan>,
    next_span_id: u64,
    next_end_sequence: u64,
}

fn create_span(state: &mut State, parent_index: Option<usize>, options: &SpanOptions) -> usize {
    let id = state.next_span_id;
    state.next_span_id += 1;
    let parent_id = parent_index.map(|index| state.spans[index].id);
    state.spans.push(MutableSpan {
        id,
        parent_id,
        name: options.name.clone(),
        attributes: options.attributes.clone(),
        events: Vec::new(),
        status: SpanStatus::Ok,
        explicit_status: false,
        settled: false,
        end_sequence: None,
    });
    state.spans.len() - 1
}

fn settle(state: &mut State, index: usize, result: &Result<(), SpanErrorInfo>) {
    let span = &mut state.spans[index];
    if span.settled {
        return;
    }
    if let Err(info) = result
        && !span.explicit_status
    {
        span.status = SpanStatus::Error(Some(info.clone()));
    }
    span.settled = true;
    span.end_sequence = Some(state.next_end_sequence);
    state.next_end_sequence += 1;
}

fn snapshot(span: &MutableSpan) -> RecordedTelemetrySpan {
    RecordedTelemetrySpan {
        id: span.id,
        parent_id: span.parent_id,
        name: span.name.clone(),
        attributes: span.attributes.clone(),
        events: span.events.clone(),
        status: span.status.clone(),
        settled: span.settled,
        end_sequence: span.end_sequence,
    }
}

/// Shared handle to one recorded span; `index` is stable because spans are
/// append-only. All state lives behind one lock, mirroring the
/// single-threaded TypeScript recorder exactly (including settlement order
/// and the parent-settled fallback).
#[derive(Clone)]
struct InMemorySpan {
    state: Arc<Mutex<State>>,
    index: usize,
}

impl InMemorySpan {
    fn into_telemetry_span(self) -> Arc<dyn TelemetrySpan> {
        Arc::new(self)
    }
}

impl TelemetrySpan for InMemorySpan {
    fn add_event(&self, name: &str, attributes: SpanAttributes) {
        let mut state = lock(&self.state);
        let span = &mut state.spans[self.index];
        if span.settled {
            return;
        }
        span.events.push(RecordedTelemetryEvent {
            name: name.to_string(),
            attributes,
        });
    }

    fn set_attributes(&self, attributes: SpanAttributes) {
        let mut state = lock(&self.state);
        let span = &mut state.spans[self.index];
        if span.settled {
            return;
        }
        for (name, value) in attributes {
            span.attributes.set(name, value);
        }
    }

    fn set_status(&self, status: SpanStatus) {
        let mut state = lock(&self.state);
        let span = &mut state.spans[self.index];
        if span.settled {
            return;
        }
        span.status = status;
        span.explicit_status = true;
    }

    fn dispatch_child_span_sync(
        &self,
        options: SpanOptions,
        body: SpanBodySync<'_>,
    ) -> Result<(), SpanErrorInfo> {
        let parent_settled = lock(&self.state).spans[self.index].settled;
        if parent_settled {
            return dispatch_noop_sync(body);
        }
        let child_index = {
            let mut state = lock(&self.state);
            create_span(&mut state, Some(self.index), &options)
        };
        let child = InMemorySpan {
            state: Arc::clone(&self.state),
            index: child_index,
        }
        .into_telemetry_span();
        let result = body(child);
        settle(&mut lock(&self.state), child_index, &result);
        result
    }

    fn dispatch_child_span_async<'a>(
        &'a self,
        options: SpanOptions,
        body: SpanBodyAsync<'a>,
    ) -> BoxSpanFuture<'a, Result<(), SpanErrorInfo>> {
        let parent_settled = lock(&self.state).spans[self.index].settled;
        if parent_settled {
            return dispatch_noop_async(body);
        }
        let child_index = {
            let mut state = lock(&self.state);
            create_span(&mut state, Some(self.index), &options)
        };
        let child = InMemorySpan {
            state: Arc::clone(&self.state),
            index: child_index,
        }
        .into_telemetry_span();
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let result = body(child).await;
            settle(&mut lock(&state), child_index, &result);
            result
        })
    }
}

/// Port of `InMemoryTelemetryContext`.
pub struct InMemoryTelemetryContext {
    state: Arc<Mutex<State>>,
}

impl Default for InMemoryTelemetryContext {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for InMemoryTelemetryContext {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

impl InMemoryTelemetryContext {
    /// Creates an isolated recording scope.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                spans: Vec::new(),
                next_span_id: 1,
                next_end_sequence: 1,
            })),
        }
    }

    /// Returns detached snapshots in span-start order.
    pub fn get_spans(&self) -> Vec<RecordedTelemetrySpan> {
        let state = lock(&self.state);
        state.spans.iter().map(snapshot).collect()
    }

    fn handle(&self, index: usize) -> Arc<dyn TelemetrySpan> {
        InMemorySpan {
            state: Arc::clone(&self.state),
            index,
        }
        .into_telemetry_span()
    }
}

impl TelemetryContext for InMemoryTelemetryContext {
    fn dispatch_span_sync(
        &self,
        options: SpanOptions,
        body: SpanBodySync<'_>,
    ) -> Result<(), SpanErrorInfo> {
        let index = {
            let mut state = lock(&self.state);
            create_span(&mut state, None, &options)
        };
        let result = body(self.handle(index));
        settle(&mut lock(&self.state), index, &result);
        result
    }

    fn dispatch_span_async<'a>(
        &'a self,
        options: SpanOptions,
        body: SpanBodyAsync<'a>,
    ) -> BoxSpanFuture<'a, Result<(), SpanErrorInfo>> {
        let index = {
            let mut state = lock(&self.state);
            create_span(&mut state, None, &options)
        };
        let state = Arc::clone(&self.state);
        let span = self.handle(index);
        Box::pin(async move {
            let result = body(span).await;
            settle(&mut lock(&state), index, &result);
            result
        })
    }
}
