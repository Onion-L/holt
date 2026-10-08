//! Event stream behavior tests mirroring the TypeScript `EventStream` usage
//! contract (synchronous push, async iteration, final result resolution).

use std::time::Duration;

use pi_core::ai::types::{AssistantMessage, AssistantMessageEvent, ErrorReason, StopReason};
use pi_core::ai::utils::event_stream::{EventStream, create_assistant_message_event_stream};

fn pending_message() -> AssistantMessage {
    AssistantMessage {
        api: "faux".to_string(),
        provider: "faux".to_string(),
        model: "faux-1".to_string(),
        ..Default::default()
    }
}

#[tokio::test]
async fn yields_events_in_order_and_resolves_result_on_done() {
    let stream = create_assistant_message_event_stream();

    stream.push(AssistantMessageEvent::Start {
        partial: pending_message(),
    });
    stream.push(AssistantMessageEvent::TextStart {
        content_index: 0,
        partial: pending_message(),
    });

    // Push the terminal event from a separate task to prove push is
    // synchronous and the consumer wakes up.
    let producer = stream.clone();
    let producer_task = tokio::spawn(async move {
        let mut message = pending_message();
        message.stop_reason = StopReason::Stop;
        message.content = vec![pi_core::ai::types::AssistantContent::Text(
            pi_core::ai::types::TextContent::default(),
        )];
        producer.push(AssistantMessageEvent::Done {
            reason: pi_core::ai::types::DoneReason::Stop,
            message: message.clone(),
        });
        message
    });

    let mut kinds = Vec::new();
    while let Some(event) = stream.next().await {
        let is_terminal = matches!(
            event,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        );
        kinds.push(event);
        if is_terminal {
            break;
        }
    }
    let done = producer_task.await.unwrap();

    assert_eq!(kinds.len(), 3, "start, text_start, done");
    assert!(matches!(kinds[0], AssistantMessageEvent::Start { .. }));
    assert!(matches!(kinds[2], AssistantMessageEvent::Done { .. }));

    let result = stream.result().await;
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(&result, &done);
}

#[tokio::test]
async fn error_event_resolves_result_with_the_error_message() {
    let stream = create_assistant_message_event_stream();
    stream.push(AssistantMessageEvent::Start {
        partial: pending_message(),
    });

    let producer = stream.clone();
    tokio::spawn(async move {
        let mut error = pending_message();
        error.stop_reason = StopReason::Aborted;
        error.error_message = Some("Request was aborted".to_string());
        producer.push(AssistantMessageEvent::Error {
            reason: ErrorReason::Aborted,
            error,
        });
    });

    let mut events = 0;
    while stream.next().await.is_some() {
        events += 1;
    }
    assert_eq!(events, 2);

    let result = stream.result().await;
    assert_eq!(result.stop_reason, StopReason::Aborted);
    assert_eq!(result.error_message.as_deref(), Some("Request was aborted"));
}

#[tokio::test]
async fn end_releases_the_consumer_and_pushes_afterward_are_dropped() {
    let stream: EventStream<u32, u32> = EventStream::new(|_| false, |value| value);

    stream.push(1);
    let producer = stream.clone();
    let end_task = tokio::spawn(async move {
        producer.end(None);
    });

    let mut seen = Vec::new();
    while let Some(value) = stream.next().await {
        seen.push(value);
    }
    end_task.await.unwrap();

    assert_eq!(seen, vec![1]);
    assert!(stream.is_done());

    // Pushes after completion are dropped.
    stream.push(2);
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn end_with_result_resolves_result() {
    let stream: EventStream<u32, String> =
        EventStream::new(|_| false, |value: u32| value.to_string());
    stream.push(7);
    let producer = stream.clone();
    producer.end(Some("final".to_string()));

    assert_eq!(stream.next().await, Some(7));
    assert!(stream.next().await.is_none());
    assert_eq!(stream.result().await, "final");
}

#[tokio::test]
async fn result_waits_for_the_terminal_event() {
    let stream = create_assistant_message_event_stream();
    let waiter = stream.clone();
    let result_task = tokio::spawn(async move { waiter.result().await });

    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!result_task.is_finished());

    let mut message = pending_message();
    message.stop_reason = StopReason::Length;
    stream.push(AssistantMessageEvent::Done {
        reason: pi_core::ai::types::DoneReason::Length,
        message: message.clone(),
    });

    let result = result_task.await.unwrap();
    assert_eq!(result.stop_reason, StopReason::Length);
}
