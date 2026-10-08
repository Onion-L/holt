//! Port of `pi-core/ai/src/utils/event-stream.ts`.
//!
//! TypeScript pushes events into the stream from a producer and consumes them
//! with an async iterator while awaiting a final result promise. The Rust
//! port keeps the same shape: synchronous `push`/`end` from the producer
//! side, an async `next` for the consumer, and an async `result` that
//! resolves with the final value once a terminal event arrives.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

use crate::ai::types::{AssistantMessage, AssistantMessageEvent};
use crate::telemetry::lock;

type ExtractResult<T, R> = Box<dyn FnOnce(T) -> R + Send>;

struct StreamShared<T, R> {
    is_complete: Box<dyn Fn(&T) -> bool + Send + Sync>,
    extract_result: Mutex<Option<ExtractResult<T, R>>>,
    state: Mutex<StreamState<T, R>>,
}

struct StreamState<T, R> {
    queue: VecDeque<T>,
    done: bool,
    result: Option<R>,
    next_wakers: VecDeque<Waker>,
    result_wakers: Vec<Waker>,
}

impl<T, R> StreamState<T, R> {
    fn wake_result_waiters(&mut self) {
        for waker in self.result_wakers.drain(..) {
            waker.wake();
        }
    }
}

/// Port of `EventStream<T, R>`: a generic event stream with a final result.
pub struct EventStream<T, R> {
    shared: Arc<StreamShared<T, R>>,
}

impl<T, R> Clone for EventStream<T, R> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T: Send + Clone + 'static, R: Send + 'static> EventStream<T, R> {
    /// Port of the `EventStream` constructor. `is_complete` decides whether an
    /// event terminates the stream; `extract_result` produces the final
    /// result from the terminal event.
    pub fn new(
        is_complete: impl Fn(&T) -> bool + Send + Sync + 'static,
        extract_result: impl FnOnce(T) -> R + Send + 'static,
    ) -> Self {
        Self {
            shared: Arc::new(StreamShared {
                is_complete: Box::new(is_complete),
                extract_result: Mutex::new(Some(Box::new(extract_result))),
                state: Mutex::new(StreamState {
                    queue: VecDeque::new(),
                    done: false,
                    result: None,
                    next_wakers: VecDeque::new(),
                    result_wakers: Vec::new(),
                }),
            }),
        }
    }

    /// Port of `push`: delivers the event to a waiting consumer or queues it.
    /// A terminal event resolves the result; pushes after completion are
    /// dropped.
    pub fn push(&self, event: T) {
        let is_complete = (self.shared.is_complete)(&event);
        {
            let mut state = lock(&self.shared.state);
            if state.done {
                return;
            }
            if is_complete {
                state.done = true;
                let extract = self
                    .shared
                    .extract_result
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(extract) = extract {
                    state.result = Some(extract(event.clone()));
                }
                state.wake_result_waiters();
            }
        }

        let mut state = lock(&self.shared.state);
        if let Some(waker) = state.next_wakers.pop_front() {
            waker.wake();
        }
        state.queue.push_back(event);
    }

    /// Port of `end`: marks the stream complete, optionally resolving the
    /// result, and releases all waiting consumers.
    pub fn end(&self, result: Option<R>) {
        let mut state = lock(&self.shared.state);
        state.done = true;
        if let Some(result) = result
            && state.result.is_none()
        {
            state.result = Some(result);
        }
        while let Some(waker) = state.next_wakers.pop_front() {
            waker.wake();
        }
        state.wake_result_waiters();
    }

    /// Port of the async iterator step: yields queued events until the stream
    /// is complete, returning `None` afterwards.
    pub async fn next(&self) -> Option<T> {
        std::future::poll_fn(|cx| {
            let mut state = lock(&self.shared.state);
            if let Some(event) = state.queue.pop_front() {
                return Poll::Ready(Some(event));
            }
            if state.done {
                return Poll::Ready(None);
            }
            state.next_wakers.push_back(cx.waker().clone());
            Poll::Pending
        })
        .await
    }

    /// Port of `result`: resolves with the final result once the stream
    /// completes.
    pub async fn result(&self) -> R
    where
        R: Clone,
    {
        std::future::poll_fn(|cx| {
            let mut state = lock(&self.shared.state);
            if let Some(result) = &state.result {
                return Poll::Ready(result.clone());
            }
            state.result_wakers.push(cx.waker().clone());
            Poll::Pending
        })
        .await
    }

    /// Whether the stream has reached a terminal event.
    pub fn is_done(&self) -> bool {
        lock(&self.shared.state).done
    }
}

/// Port of `AssistantMessageEventStream`.
pub type AssistantMessageEventStream = EventStream<AssistantMessageEvent, AssistantMessage>;

/// Port of `createAssistantMessageEventStream`.
pub fn create_assistant_message_event_stream() -> AssistantMessageEventStream {
    EventStream::new(
        |event: &AssistantMessageEvent| event.is_terminal(),
        |event| {
            event
                .terminal_message()
                .cloned()
                .expect("terminal event carries the final message")
        },
    )
}

/// Collects all remaining events from a stream clone (test helper mirroring
/// `for await` loops over the TS stream).
pub async fn collect_events<T: Send + Clone + 'static, R: Send + 'static>(
    stream: &EventStream<T, R>,
) -> Vec<T> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

/// Future returned by [`EventStream::next`]; re-exported for implementors.
pub type NextFuture<'a, T> = Pin<Box<dyn Future<Output = Option<T>> + Send + 'a>>;
