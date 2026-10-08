//! Port of `pi-core/agent/test/proxy.test.ts`.
//!
//! The TypeScript suite stubs `globalThis.fetch`; the Rust port injects
//! the canned response through `ProxyStreamOptions.fetch`.

use std::sync::Arc;

use bytes::Bytes;
use futures::stream::{self, StreamExt};
use serde_json::json;

use pi_core::agent::proxy::{ProxyAssistantMessageEvent, ProxyStreamOptions, stream_proxy};
use pi_core::ai::types::{AssistantMessageEvent, Context, DoneReason, Model, ModelInput, ToolCall};
use pi_core::ai::utils::event_stream::AssistantMessageEventStream;
use pi_core::ai::utils::http::HttpRequest;
use pi_core::ai::utils::http::{HttpFetch, HttpFetchError, HttpResponse};

fn model() -> Model {
    Model {
        id: "gpt-5.4".to_string(),
        name: "GPT-5.4".to_string(),
        api: "openai-responses".to_string(),
        provider: "openai".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        context_window: 400_000,
        max_tokens: 128_000,
        ..Default::default()
    }
}

struct CannedFetch {
    status: u16,
    body: String,
    requests: std::sync::Mutex<Vec<HttpRequest>>,
}

impl HttpFetch for CannedFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        let body = self.body.clone();
        let status = self.status;
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        Box::pin(async move {
            Ok(HttpResponse {
                status,
                headers: Vec::new(),
                body: stream::iter(vec![Ok(Bytes::from(body))]).boxed(),
            })
        })
    }
}

async fn collect_events(stream: &AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
    pi_core::ai::utils::event_stream::collect_events(stream).await
}

#[tokio::test]
async fn preserves_tool_call_metadata_received_only_on_toolcall_end() {
    let proxy_events: Vec<ProxyAssistantMessageEvent> = vec![
        ProxyAssistantMessageEvent::Start,
        ProxyAssistantMessageEvent::ToolcallStart {
            content_index: 0,
            id: "call_test|fc_test".to_string(),
            tool_name: "lookup".to_string(),
        },
        ProxyAssistantMessageEvent::ToolcallDelta {
            content_index: 0,
            delta: r#"{"value":"hello"}"#.to_string(),
        },
        ProxyAssistantMessageEvent::ToolcallEnd {
            content_index: 0,
            tool_call: ToolCall {
                id: "call_test|fc_test".to_string(),
                name: "lookup".to_string(),
                arguments: json!({ "value": "hello" })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                namespace: Some("dynamic_tools".to_string()),
                ..Default::default()
            },
        },
        ProxyAssistantMessageEvent::Done {
            reason: DoneReason::ToolUse,
            usage: Default::default(),
        },
    ];
    let body = proxy_events
        .iter()
        .map(|event| format!("data: {}\n\n", serde_json::to_string(event).unwrap()))
        .collect::<String>();
    let fetch: pi_core::ai::types::FetchFunction = Arc::new(CannedFetch {
        status: 200,
        body,
        requests: std::sync::Mutex::new(Vec::new()),
    });

    let stream = stream_proxy(
        &model(),
        &Context::default(),
        ProxyStreamOptions {
            auth_token: "test-token".to_string(),
            proxy_url: "https://proxy.example.com".to_string(),
            fetch: Some(fetch),
            ..Default::default()
        },
    );
    let events = collect_events(&stream).await;
    let result = stream.result().await;
    let end_event = events
        .iter()
        .find(|event| matches!(event, AssistantMessageEvent::ToolcallEnd { .. }));

    let AssistantMessageEvent::ToolcallEnd { tool_call, .. } = end_event.expect("toolcall_end")
    else {
        panic!("expected toolcall_end");
    };
    assert_eq!(tool_call.namespace.as_deref(), Some("dynamic_tools"));

    match &result.content[0] {
        pi_core::ai::types::AssistantContent::ToolCall(tool_call) => {
            assert_eq!(
                tool_call.arguments,
                json!({ "value": "hello" })
                    .as_object()
                    .cloned()
                    .unwrap_or_default()
            );
            assert_eq!(tool_call.namespace.as_deref(), Some("dynamic_tools"));
        }
        other => panic!("unexpected content: {other:?}"),
    }
}
