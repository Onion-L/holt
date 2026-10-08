//! Delayed versions of the upstream Responses, Mistral and Pi SSE fixtures.
//! Keep the body open until a delta is observed, so buffering cannot pass.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use pi_core::ai::compat;
use pi_core::ai::types::{
    AssistantMessageEvent, Context, Model, ProviderRequestOptions, StopReason, StreamOptions,
};
use pi_core::ai::utils::http::{HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

struct ChannelFetch {
    receiver: Mutex<Option<mpsc::UnboundedReceiver<Result<Bytes, HttpFetchError>>>>,
    requested: Notify,
}

impl HttpFetch for ChannelFetch {
    fn fetch<'a>(
        &'a self,
        _: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        let receiver = self.receiver.lock().unwrap().take().unwrap();
        self.requested.notify_one();
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: Box::pin(futures::stream::unfold(receiver, |mut receiver| async {
                    receiver.recv().await.map(|chunk| (chunk, receiver))
                })),
            })
        })
    }
}

fn start(
    api: &str,
) -> (
    pi_core::ai::utils::event_stream::AssistantMessageEventStream,
    mpsc::UnboundedSender<Result<Bytes, HttpFetchError>>,
    Arc<ChannelFetch>,
    CancellationToken,
) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let fetch = Arc::new(ChannelFetch {
        receiver: Mutex::new(Some(receiver)),
        requested: Notify::new(),
    });
    let signal = CancellationToken::new();
    let model = Model {
        id: "test-model".into(),
        name: "Test".into(),
        api: api.into(),
        provider: api.into(),
        base_url: "https://provider.test/v1".into(),
        max_tokens: 100,
        ..Default::default()
    };
    let options = StreamOptions {
        base: ProviderRequestOptions {
            api_key: Some("test-key".into()),
            fetch: Some(fetch.clone()),
            signal: Some(signal.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    let stream = match api {
        "google-generative-ai" => pi_core::ai::api::google_generative_ai::stream_with_transport(
            &model,
            &Context::default(),
            Some(&pi_core::ai::api::google_generative_ai::GoogleOptions {
                base: options,
                ..Default::default()
            }),
            Some(fetch.clone()),
        ),
        "google-vertex" => pi_core::ai::api::google_vertex::stream_with_transport(
            &model,
            &Context::default(),
            Some(&pi_core::ai::api::google_vertex::GoogleVertexOptions {
                base: options,
                ..Default::default()
            }),
            Some(fetch.clone()),
        ),
        _ => compat::stream(&model, &Context::default(), Some(&options)),
    };
    (stream, sender, fetch, signal)
}

fn frame(value: Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}

async fn assert_incremental(api: &str) {
    let (stream, sender, _, _) = start(api);
    let events = match api {
        "pi-messages" => vec![
            json!({"type":"text_start","contentIndex":0}),
            json!({"type":"text_delta","contentIndex":0,"delta":"hello"}),
        ],
        "mistral-conversations" => {
            vec![json!({"id":"r1","choices":[{"delta":{"content":"hello"},"finish_reason":null}]})]
        }
        _ => vec![
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m1","role":"assistant","content":[]}}),
            json!({"type":"response.content_part.added","item_id":"m1","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
            json!({"type":"response.output_text.delta","item_id":"m1","output_index":0,"content_index":0,"delta":"hello"}),
        ],
    };
    for event in events {
        sender.send(Ok(frame(event))).unwrap();
    }
    let delta = tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(event) = stream.next().await {
            if let AssistantMessageEvent::TextDelta { delta, .. } = event {
                return delta;
            }
        }
        panic!("{api}: stream ended before delta");
    })
    .await;
    // A failure must also release the producer task.
    sender
        .send(Err(HttpFetchError::Body("connection lost".into())))
        .ok();
    drop(sender);
    let result = tokio::time::timeout(Duration::from_secs(1), stream.result())
        .await
        .unwrap();
    assert_eq!(delta.expect("delta must arrive before body EOF"), "hello");
    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(result.error_message.unwrap().contains("connection lost"));
    if api != "pi-messages" {
        assert!(
            matches!(result.content.first(), Some(pi_core::ai::types::AssistantContent::Text(text)) if text.text == "hello")
        );
    }
}

macro_rules! incremental_test {
    ($name:ident, $api:literal) => {
        #[tokio::test]
        async fn $name() {
            assert_incremental($api).await;
        }
    };
}
incremental_test!(responses_yields_before_eof, "openai-responses");
incremental_test!(azure_yields_before_eof, "azure-openai-responses");
incremental_test!(mistral_yields_before_eof, "mistral-conversations");
incremental_test!(pi_yields_before_eof, "pi-messages");

#[tokio::test]
async fn pending_body_can_be_cancelled() {
    for api in [
        "openai-completions",
        "openai-responses",
        "azure-openai-responses",
        "mistral-conversations",
        "pi-messages",
        "google-generative-ai",
        "google-vertex",
    ] {
        let (stream, sender, fetch, signal) = start(api);
        tokio::time::timeout(Duration::from_secs(1), fetch.requested.notified())
            .await
            .unwrap();
        signal.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), stream.result()).await;
        drop(sender);
        let result =
            result.unwrap_or_else(|_| panic!("{api}: cancellation did not interrupt pending body"));
        assert_eq!(result.stop_reason, StopReason::Aborted, "{api}");
        let mut terminal_count = 0;
        while let Some(event) = stream.next().await {
            if matches!(
                event,
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
            ) {
                terminal_count += 1;
            }
        }
        assert_eq!(terminal_count, 1, "{api}");
    }
}

#[tokio::test]
async fn default_transport_cancels_a_pending_body() {
    use pi_core::ai::utils::http::{HttpBody, HttpMethod};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let mut received = Vec::new();
        while !received.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = socket.read(&mut request).await.unwrap();
            assert!(count > 0, "request ended before headers");
            received.extend_from_slice(&request[..count]);
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });
    let signal = CancellationToken::new();
    let response = pi_core::ai::utils::reqwest_fetch::default_fetch()
        .fetch(HttpRequest {
            method: HttpMethod::Get,
            url: format!("http://{address}"),
            headers: vec![],
            body: HttpBody::Empty,
            signal: Some(signal.clone()),
        })
        .await
        .unwrap();
    let mut body = response.body;
    signal.cancel();
    let chunk = tokio::time::timeout(Duration::from_secs(1), body.next()).await;
    server.abort();
    let _ = server.await;
    assert!(matches!(
        chunk.expect("cancel must wake body"),
        Some(Err(HttpFetchError::Cancelled))
    ));
}

#[tokio::test]
async fn pi_preserves_split_utf8_and_flushes_the_terminal_tail() {
    let text = "\u{4f60}\u{597d}";
    let events = [
        json!({"type":"text_start","contentIndex":0}),
        json!({"type":"text_delta","contentIndex":0,"delta":text}),
        json!({"type":"text_end","contentIndex":0,"content":text}),
        json!({"type":"done","reason":"stop","usage":pi_core::ai::types::Usage::default()}),
    ];
    let body = events
        .iter()
        .map(|event| format!("data: {event}"))
        .collect::<Vec<_>>()
        .join("\r\n\r\n");
    let (stream, sender, _, _) = start("pi-messages");
    for byte in body.bytes() {
        sender.send(Ok(Bytes::from(vec![byte]))).unwrap();
    }
    drop(sender);
    let result = tokio::time::timeout(Duration::from_secs(1), stream.result())
        .await
        .unwrap();
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert!(
        matches!(result.content.first(), Some(pi_core::ai::types::AssistantContent::Text(content)) if content.text == text)
    );
}
