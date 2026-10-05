//! The provider-stream transport layer: the built-in transport, the idle
//! watchdog that bounds time between events (never total request duration),
//! and the mid-stream retry that re-dials content-free failures. Every
//! engine-owned request — Turns, subagents, compaction, title tasks — runs
//! behind exactly one of these guards.

use std::{sync::Arc, time::Duration};

use chrono::Utc;
use pi_core::ai::{
    compat,
    types::{
        AssistantMessage, AssistantMessageEvent, Context as PiContext, ErrorReason,
        Model as PiModel, SimpleStreamOptions, StopReason,
    },
    utils::event_stream::{AssistantMessageEventStream, create_assistant_message_event_stream},
};
use tokio_util::sync::CancellationToken;

/// How long a provider stream may stay silent before the engine fails the
/// request. Five minutes without any event — the first or any later one —
/// means the connection is gone, and without a deadline a main Turn or a
/// child agent stays `streaming` until an application restart.
pub(crate) const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Give a cancelled provider a short window to emit its own terminal abort
/// event before the engine synthesizes one. Providers use that event to finish
/// transport-specific cleanup; the bound keeps cancellation from hanging when
/// a transport ignores its signal.
const STREAM_CANCEL_SETTLE_TIMEOUT: Duration = Duration::from_secs(1);

/// The built-in provider transport: the compat stream over the resolved
/// model, behind the stream idle watchdog. Tests inject their own through
/// `EngineConfig::stream_fn` and get the same guard at
/// `AgentRuntime::new`, so every engine-owned request path — Turns,
/// subagents, compaction, title tasks — runs exactly one watchdog.
pub(crate) fn default_stream_fn() -> pi_core::agent::types::StreamFn {
    let raw: pi_core::agent::types::StreamFn = Arc::new(
        |model: &PiModel, context: &PiContext, options: Option<&SimpleStreamOptions>| {
            Ok(compat::stream_simple(model, context, options))
        },
    );
    guard_stream_fn(raw, STREAM_IDLE_TIMEOUT)
}

/// Wrap a raw transport so a stream that stops producing events cannot
/// stall a run forever: every `next()` — the first one included — races an
/// idle deadline that resets on each forwarded event, so the rule is time
/// between events, never total request duration.
///
/// The raw transport sees a child `CancellationToken` in place of the
/// caller's: a timeout cancels the provider without mutating the Turn's
/// own token, while the link task below keeps the parent's cancellation
/// authority. Every terminal branch cancels the child and drops the
/// upstream stream, so nothing keeps consuming the provider afterwards.
pub(crate) fn guard_stream_fn(
    raw: pi_core::agent::types::StreamFn,
    idle: Duration,
) -> pi_core::agent::types::StreamFn {
    Arc::new(
        move |model: &PiModel, context: &PiContext, options: Option<&SimpleStreamOptions>| {
            let parent = options.and_then(|options| options.base.base.signal.clone());
            let mut child_options = options.cloned().unwrap_or_default();
            let child = CancellationToken::new();
            child_options.base.base.signal = Some(child.clone());
            let upstream = raw(model, context, Some(&child_options))?;
            if let Some(parent) = parent.clone() {
                let linked = child.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = parent.cancelled() => linked.cancel(),
                        _ = linked.cancelled() => {}
                    }
                });
            }
            let output = create_assistant_message_event_stream();
            let fallback = AssistantMessage {
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                timestamp: Utc::now().timestamp_millis(),
                ..Default::default()
            };
            tokio::spawn(forward_with_idle_watchdog(
                upstream,
                output.clone(),
                parent,
                child,
                idle,
                fallback,
            ));
            Ok(output)
        },
    )
}

/// The watchdog's forwarding loop: copy events upstream→output until one
/// of the four endings — a terminal event (forwarded unchanged), an idle
/// timeout, a parent cancellation, or an upstream end without a terminal
/// event. The last three push a synthetic terminal event so the consumer's
/// `result().await` can never hang, and all four cancel the child token
/// before returning (which drops `upstream` with it).
async fn forward_with_idle_watchdog(
    upstream: AssistantMessageEventStream,
    output: AssistantMessageEventStream,
    parent: Option<CancellationToken>,
    child: CancellationToken,
    idle: Duration,
    fallback: AssistantMessage,
) {
    let mut partial: Option<AssistantMessage> = None;
    let mut seen_event = false;
    loop {
        let phase = if seen_event {
            "between_events"
        } else {
            "first_event"
        };
        let event = tokio::select! {
            _ = parent_cancelled(&parent) => {
                // The Turn's own cancellation keeps its meaning:
                // interrupted by the user, never a provider failure. When
                // the transport's own abort event wins the race instead, it
                // was already forwarded below and this branch never runs.
                child.cancel();
                finish_cancelled_stream(upstream, output, partial, fallback).await;
                return;
            }
            event = tokio::time::timeout(idle, upstream.next()) => match event {
                Ok(event) => event,
                Err(_) => {
                    child.cancel();
                    tracing::warn!(
                        target: "holt::agent",
                        provider = %fallback.provider,
                        model = %fallback.model,
                        timeout_ms = idle.as_millis() as u64,
                        phase,
                        "provider stream stalled; failing the request"
                    );
                    output.push(synthetic_terminal(
                        partial.unwrap_or_else(|| fallback.clone()),
                        StopReason::Error,
                        &stream_stall_message(idle),
                    ));
                    return;
                }
            },
        };
        let Some(event) = event else {
            // The transport ended its stream without a terminal event:
            // `output.end(None)` would leave `result().await` pending
            // forever, so settle it as a provider error instead.
            child.cancel();
            output.push(synthetic_terminal(
                partial.unwrap_or_else(|| fallback.clone()),
                StopReason::Error,
                "The provider closed the stream without completing the response",
            ));
            return;
        };
        let terminal = event.is_terminal();
        if !terminal && let Some(message) = stream_event_partial(&event) {
            partial = Some(message.clone());
        }
        output.push(event);
        if terminal {
            child.cancel();
            return;
        }
        seen_event = true;
    }
}

/// Let a cancelled provider finish its own abort handshake, but never wait
/// indefinitely for a transport that ignores cancellation.
async fn finish_cancelled_stream(
    upstream: AssistantMessageEventStream,
    output: AssistantMessageEventStream,
    mut partial: Option<AssistantMessage>,
    fallback: AssistantMessage,
) {
    let settle = tokio::time::sleep(STREAM_CANCEL_SETTLE_TIMEOUT);
    tokio::pin!(settle);
    loop {
        let event = tokio::select! {
            event = upstream.next() => event,
            _ = &mut settle => {
                output.push(synthetic_terminal(
                    partial.unwrap_or_else(|| fallback.clone()),
                    StopReason::Aborted,
                    "Request was aborted",
                ));
                return;
            }
        };
        let Some(event) = event else {
            output.push(synthetic_terminal(
                partial.unwrap_or_else(|| fallback.clone()),
                StopReason::Aborted,
                "Request was aborted",
            ));
            return;
        };
        let terminal = event.is_terminal();
        if !terminal && let Some(message) = stream_event_partial(&event) {
            partial = Some(message.clone());
        }
        output.push(event);
        if terminal {
            return;
        }
    }
}

/// Await the caller's cancellation token, or never resolve when the
/// request carries none (the watchdog's own branches are the only
/// endings left).
async fn parent_cancelled(parent: &Option<CancellationToken>) {
    match parent {
        Some(parent) => parent.cancelled().await,
        None => std::future::pending().await,
    }
}

/// A terminal event carrying `message`'s already-received content, so a
/// synthetic failure does not discard streamed text or tool calls.
fn synthetic_terminal(
    mut message: AssistantMessage,
    stop_reason: StopReason,
    error: &str,
) -> AssistantMessageEvent {
    message.stop_reason = stop_reason;
    message.error_message = Some(error.to_string());
    AssistantMessageEvent::Error {
        reason: match stop_reason {
            StopReason::Aborted => ErrorReason::Aborted,
            _ => ErrorReason::Error,
        },
        error: message,
    }
}

/// The stable, human-readable failure text of an idle timeout — it names
/// the interval so the transcript says how long the silence was.
fn stream_stall_message(idle: Duration) -> String {
    format!(
        "The provider stopped responding: no event arrived for {} ms, \
         so the request was closed",
        idle.as_millis()
    )
}

/// The mid-stream retry budget, on top of the request layer's HTTP retries.
const STREAM_MAX_RETRIES: u32 = 2;

/// Re-send a request whose stream failed after the provider accepted it
/// (`Start` seen) but before any content reached the consumer — an SSE
/// `overloaded_error`, a dropped body, a stall — since nothing has to be
/// taken back. A failure before `Start` already went through pi-core's HTTP
/// retries, and one after content surfaces as-is. Each scheduled retry
/// reports through `on_retry`, like the request layer's.
pub(crate) fn retry_stream_fn(
    inner: pi_core::agent::types::StreamFn,
    on_retry: pi_core::ai::types::OnRetryCallback,
) -> pi_core::agent::types::StreamFn {
    Arc::new(
        move |model: &PiModel, context: &PiContext, options: Option<&SimpleStreamOptions>| {
            let upstream = inner(model, context, options)?;
            let output = create_assistant_message_event_stream();
            tokio::spawn(forward_with_stream_retry(
                upstream,
                output.clone(),
                inner.clone(),
                model.clone(),
                context.clone(),
                options.cloned(),
                on_retry.clone(),
            ));
            Ok(output)
        },
    )
}

/// Forward `upstream` into `output`, re-dialing `inner` for each retryable
/// content-free failure. Only the first attempt's `Start` is forwarded, so
/// the consumer sees one message however many attempts it took.
async fn forward_with_stream_retry(
    mut upstream: AssistantMessageEventStream,
    output: AssistantMessageEventStream,
    inner: pi_core::agent::types::StreamFn,
    model: PiModel,
    context: PiContext,
    options: Option<SimpleStreamOptions>,
    on_retry: pi_core::ai::types::OnRetryCallback,
) {
    let signal = options
        .as_ref()
        .and_then(|options| options.base.base.signal.clone());
    let mut start_forwarded = false;
    let mut retries = 0;
    loop {
        let mut started = false;
        let mut content = false;
        let failed = loop {
            let Some(event) = upstream.next().await else {
                output.end(None);
                return;
            };
            match &event {
                AssistantMessageEvent::Start { .. } => {
                    started = true;
                    if start_forwarded {
                        continue;
                    }
                    start_forwarded = true;
                }
                AssistantMessageEvent::Error { .. } => break event,
                AssistantMessageEvent::Done { .. } => {
                    output.push(event);
                    return;
                }
                _ => content = true,
            }
            output.push(event);
        };
        let AssistantMessageEvent::Error { error: message, .. } = &failed else {
            unreachable!("the inner loop only breaks on an error event");
        };
        if !started
            || content
            || retries >= STREAM_MAX_RETRIES
            || signal.as_ref().is_some_and(CancellationToken::is_cancelled)
            || !pi_core::ai::utils::retry::is_retryable_assistant_error(message)
        {
            output.push(failed);
            return;
        }
        let message = message.clone();
        retries += 1;
        let delay_ms = 1000 * 2u64.pow(retries - 1);
        on_retry(
            retries,
            STREAM_MAX_RETRIES,
            delay_ms,
            message.error_message.as_deref().unwrap_or_default(),
        );
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
            _ = parent_cancelled(&signal) => {
                output.push(synthetic_terminal(message, StopReason::Aborted, "Request was aborted"));
                return;
            }
        }
        upstream = match inner(&model, &context, options.as_ref()) {
            Ok(upstream) => upstream,
            Err(_) => {
                output.push(failed);
                return;
            }
        };
    }
}

/// The partial assistant message carried by every non-terminal stream
/// event — the state a synthetic terminal must preserve.
fn stream_event_partial(event: &AssistantMessageEvent) -> Option<&AssistantMessage> {
    match event {
        AssistantMessageEvent::Start { partial }
        | AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolcallStart { partial, .. }
        | AssistantMessageEvent::ToolcallDelta { partial, .. }
        | AssistantMessageEvent::ToolcallEnd { partial, .. } => Some(partial),
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::ai::types::{AssistantContent, DoneReason, TextContent};
    use std::sync::Mutex;

    // -----------------------------------------------------------------------
    // The provider-stream idle watchdog
    // -----------------------------------------------------------------------

    /// A partial assistant message carrying `text`, the state a stalled
    /// stream must not lose.
    fn watchdog_partial(text: &str) -> AssistantMessage {
        AssistantMessage {
            api: "test-api".into(),
            provider: "test-provider".into(),
            model: "test-model".into(),
            content: vec![AssistantContent::Text(TextContent {
                text: text.into(),
                ..Default::default()
            })],
            ..Default::default()
        }
    }

    /// Drain a guarded stream to its end, bounded: a watchdog bug shows up
    /// as a hang, which must fail the test instead of the runner.
    async fn settle_stream(stream: &AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut events = Vec::new();
            while let Some(event) = stream.next().await {
                events.push(event);
            }
            events
        })
        .await
        .expect("the guarded stream always settles")
    }

    /// The text of an assistant message's first content block.
    fn first_text(message: &AssistantMessage) -> &str {
        let AssistantContent::Text(text) = &message.content[0] else {
            panic!("expected text content: {:?}", message.content);
        };
        &text.text
    }

    #[tokio::test]
    async fn a_stream_that_never_emits_fails_after_the_idle_deadline() {
        let raw: pi_core::agent::types::StreamFn =
            Arc::new(|_, _, _| Ok(create_assistant_message_event_stream()));
        let guarded = guard_stream_fn(raw, Duration::from_millis(50));
        let stream = guarded(&PiModel::default(), &PiContext::default(), None).unwrap();

        let events = settle_stream(&stream).await;

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], AssistantMessageEvent::Error { .. }));
        let message = stream.result().await;
        assert_eq!(message.stop_reason, StopReason::Error);
        let error = message.error_message.expect("synthetic error message");
        assert!(error.contains("50 ms"), "unexpected message: {error}");
    }

    #[tokio::test]
    async fn a_stream_that_stalls_between_events_fails_and_keeps_the_partial() {
        let raw: pi_core::agent::types::StreamFn = Arc::new(|_, _, _| {
            let stream = create_assistant_message_event_stream();
            stream.push(AssistantMessageEvent::Start {
                partial: watchdog_partial("streamed so far"),
            });
            Ok(stream)
        });
        let guarded = guard_stream_fn(raw, Duration::from_millis(50));
        let stream = guarded(&PiModel::default(), &PiContext::default(), None).unwrap();

        let events = settle_stream(&stream).await;

        // The forwarded start plus one synthetic terminal error.
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
        let message = stream.result().await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(first_text(&message), "streamed so far");
        assert!(message.error_message.as_deref().unwrap().contains("50 ms"));
    }

    #[tokio::test]
    async fn the_idle_deadline_resets_on_every_event() {
        let raw: pi_core::agent::types::StreamFn = Arc::new(|_, _, _| {
            let stream = create_assistant_message_event_stream();
            let publisher = stream.clone();
            tokio::spawn(async move {
                // Gaps shorter than the deadline, but a total span longer
                // than it: only the trailing silence may fail the stream.
                for text in ["one", "two", "three"] {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    publisher.push(AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: text.into(),
                        partial: watchdog_partial(text),
                    });
                }
            });
            Ok(stream)
        });
        let guarded = guard_stream_fn(raw, Duration::from_millis(250));
        let stream = guarded(&PiModel::default(), &PiContext::default(), None).unwrap();

        let events = settle_stream(&stream).await;

        // All three deltas pass through, then exactly one terminal error.
        assert_eq!(events.len(), 4);
        for (event, text) in events.iter().zip(["one", "two", "three"]) {
            let AssistantMessageEvent::TextDelta { delta, .. } = event else {
                panic!("unexpected event: {event:?}");
            };
            assert_eq!(delta, text);
        }
        assert!(matches!(events[3], AssistantMessageEvent::Error { .. }));
        assert_eq!(stream.result().await.stop_reason, StopReason::Error);
    }

    #[tokio::test]
    async fn parent_cancellation_aborts_without_a_timeout() {
        let parent = CancellationToken::new();
        let raw: pi_core::agent::types::StreamFn = Arc::new(|_, _, _| {
            // A transport that emits one event and then goes silent: only
            // the parent token can end this stream.
            let stream = create_assistant_message_event_stream();
            stream.push(AssistantMessageEvent::Start {
                partial: watchdog_partial("partial"),
            });
            Ok(stream)
        });
        let guarded = guard_stream_fn(raw, Duration::from_secs(30));
        let mut options = SimpleStreamOptions::default();
        options.base.base.signal = Some(parent.clone());
        let stream = guarded(&PiModel::default(), &PiContext::default(), Some(&options)).unwrap();

        // Cancel well inside the idle deadline.
        tokio::time::sleep(Duration::from_millis(50)).await;
        parent.cancel();

        let events = settle_stream(&stream).await;

        assert_eq!(events.len(), 2);
        let AssistantMessageEvent::Error { reason, .. } = &events[1] else {
            panic!("unexpected terminal event: {:?}", events[1]);
        };
        assert_eq!(*reason, ErrorReason::Aborted);
        let message = stream.result().await;
        assert_eq!(message.stop_reason, StopReason::Aborted);
        assert_eq!(first_text(&message), "partial");
        // The timeout branch is the only producer of the stall message, so
        // its absence proves the cancellation branch settled the stream.
        assert!(
            !message
                .error_message
                .as_deref()
                .unwrap()
                .contains("no event arrived")
        );
    }

    #[tokio::test]
    async fn a_terminal_done_event_passes_through_unchanged() {
        let mut done = watchdog_partial("all good");
        done.stop_reason = StopReason::Stop;
        let expected = done.clone();
        let raw: pi_core::agent::types::StreamFn = Arc::new(move |_, _, _| {
            let stream = create_assistant_message_event_stream();
            stream.push(AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: done.clone(),
            });
            Ok(stream)
        });
        let guarded = guard_stream_fn(raw, Duration::from_millis(50));
        let stream = guarded(&PiModel::default(), &PiContext::default(), None).unwrap();

        let events = settle_stream(&stream).await;

        // The terminal event and nothing synthetic.
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], AssistantMessageEvent::Done { .. }));
        assert_eq!(stream.result().await, expected);
    }

    #[tokio::test]
    async fn a_stream_that_ends_without_a_terminal_event_settles_as_an_error() {
        let raw: pi_core::agent::types::StreamFn = Arc::new(|_, _, _| {
            let stream = create_assistant_message_event_stream();
            stream.push(AssistantMessageEvent::Start {
                partial: watchdog_partial("cut off"),
            });
            // Complete with no result: `result().await` on this stream
            // alone would never resolve.
            stream.end(None);
            Ok(stream)
        });
        let guarded = guard_stream_fn(raw, Duration::from_secs(30));
        let stream = guarded(&PiModel::default(), &PiContext::default(), None).unwrap();

        let events = settle_stream(&stream).await;

        assert_eq!(events.len(), 2);
        assert!(matches!(events[1], AssistantMessageEvent::Error { .. }));
        let message = stream.result().await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(first_text(&message), "cut off");
        assert!(
            message
                .error_message
                .as_deref()
                .unwrap()
                .contains("closed the stream")
        );
    }

    // -----------------------------------------------------------------------
    // Mid-stream retries
    // -----------------------------------------------------------------------

    /// One scripted attempt: whether the provider accepted the request
    /// (`Start`), the text streamed before the ending, and the ending — an
    /// error message, or `None` for a clean `Done`.
    type Attempt = (bool, Option<&'static str>, Option<&'static str>);

    /// A transport replaying one scripted attempt per call (the last one
    /// repeats), plus a call counter and the `on_retry` notices it saw.
    type RetryNotices = Arc<Mutex<Vec<(u32, u64, String)>>>;

    fn scripted_retry_stream(
        attempts: Vec<Attempt>,
    ) -> (
        pi_core::agent::types::StreamFn,
        Arc<std::sync::atomic::AtomicUsize>,
        RetryNotices,
    ) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let raw: pi_core::agent::types::StreamFn = Arc::new(move |_, _, _| {
            let index = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (started, text, error) = attempts[index.min(attempts.len() - 1)];
            let stream = create_assistant_message_event_stream();
            let mut partial = watchdog_partial(text.unwrap_or_default());
            if text.is_none() {
                partial.content.clear();
            }
            if started {
                stream.push(AssistantMessageEvent::Start {
                    partial: partial.clone(),
                });
            }
            if let Some(text) = text {
                stream.push(AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: text.into(),
                    partial: partial.clone(),
                });
            }
            stream.push(match error {
                Some(error) => synthetic_terminal(partial, StopReason::Error, error),
                None => AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: partial,
                },
            });
            Ok(stream)
        });
        let notices = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&notices);
        let on_retry: pi_core::ai::types::OnRetryCallback =
            Arc::new(move |attempt, _, delay_ms, error| {
                sink.lock()
                    .unwrap()
                    .push((attempt, delay_ms, error.to_string()));
            });
        (retry_stream_fn(raw, on_retry), calls, notices)
    }

    const OVERLOADED: &str =
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;

    #[tokio::test(start_paused = true)]
    async fn a_content_free_mid_stream_failure_is_resent() {
        let (stream_fn, calls, notices) = scripted_retry_stream(vec![
            (true, None, Some(OVERLOADED)),
            (true, Some("answer"), None),
        ]);
        let stream = stream_fn(&PiModel::default(), &PiContext::default(), None).unwrap();

        let events = settle_stream(&stream).await;

        // One Start, the second attempt's content, its Done.
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
        assert!(matches!(events[2], AssistantMessageEvent::Done { .. }));
        assert_eq!(first_text(&stream.result().await), "answer");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            *notices.lock().unwrap(),
            vec![(1, 1000, OVERLOADED.to_string())]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn mid_stream_retries_stop_at_the_budget() {
        let (stream_fn, calls, notices) =
            scripted_retry_stream(vec![(true, None, Some(OVERLOADED))]);
        let stream = stream_fn(&PiModel::default(), &PiContext::default(), None).unwrap();

        // 1s + 2s of backoff outlasts `settle_stream`'s bound.
        let message = tokio::time::timeout(Duration::from_secs(10), stream.result())
            .await
            .expect("the retrying stream settles");
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.error_message.as_deref(), Some(OVERLOADED));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        let delays: Vec<u64> = notices.lock().unwrap().iter().map(|n| n.1).collect();
        assert_eq!(delays, vec![1000, 2000]);
    }

    #[tokio::test(start_paused = true)]
    async fn failures_the_stream_layer_must_not_resend_surface_at_once() {
        for attempt in [
            // Streamed content would have to be taken back.
            (true, Some("half an answer"), Some(OVERLOADED)),
            // Never accepted: the HTTP layer already retried it.
            (false, None, Some(OVERLOADED)),
            // Not transient.
            (true, None, Some("invalid x-api-key")),
        ] {
            let (stream_fn, calls, notices) = scripted_retry_stream(vec![attempt]);
            let stream = stream_fn(&PiModel::default(), &PiContext::default(), None).unwrap();

            settle_stream(&stream).await;

            assert_eq!(stream.result().await.stop_reason, StopReason::Error);
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert!(notices.lock().unwrap().is_empty());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_during_the_backoff_aborts() {
        let (stream_fn, calls, _) = scripted_retry_stream(vec![(true, None, Some(OVERLOADED))]);
        let cancel = CancellationToken::new();
        let mut options = SimpleStreamOptions::default();
        options.base.base.signal = Some(cancel.clone());
        let stream = stream_fn(&PiModel::default(), &PiContext::default(), Some(&options)).unwrap();

        // The first attempt fails and the backoff starts; cancel inside it.
        tokio::time::sleep(Duration::from_millis(500)).await;
        cancel.cancel();
        settle_stream(&stream).await;

        assert_eq!(stream.result().await.stop_reason, StopReason::Aborted);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
