//! Selected-chat usage, decoded at the RPC boundary and rendered in the
//! status strip: the typed `holt_proto::ChatUsage` frame the engine serves,
//! with no local re-derivation — the totals, the per-source breakdown, and
//! the occupancy all arrive decided.

use gpui::{AnyElement, Context, Render, SharedString, Task, Window, div, prelude::*, px};

use crate::{
    state::{AppState, EngineHandle},
    theme::Theme,
    token_display::compact_tokens,
    watch_coordinator::WatchCoordinator,
};
use holt_proto::ChatUsage;
use holt_rpc::{RpcError, methods};

#[cfg(test)]
#[path = "../../engine/tests/common/mod.rs"]
mod engine_fixture;

/// The strip's one-line summary: the ledger's gross total and the occupancy
/// of the request the chat runs next, `≈` where the engine says the number
/// is an estimate rather than a provider's report.
pub(crate) fn label(usage: &ChatUsage) -> String {
    let o = &usage.occupancy;
    let approx = if o.estimated { "≈" } else { "" };
    let mut label = format!(
        "Total {} · Window {approx}{}",
        compact_tokens(usage.gross),
        compact_tokens(o.tokens)
    );
    if let Some(window) = o.context_window.filter(|n| *n > 0) {
        label.push_str(&format!(
            " / {} · {approx}{:.0}%",
            compact_tokens(window),
            o.tokens as f64 / window as f64 * 100.0
        ));
    }
    label
}

/// The hover breakdown: the exact totals, the per-source split with cache
/// tokens listed apart, and what the occupancy number actually is.
fn details(usage: &ChatUsage) -> Vec<String> {
    let mut lines = vec![format!(
        "{} tokens · {} records",
        usage.gross, usage.record_count
    )];
    for (kind, sum) in &usage.by_kind {
        let name = match kind.as_str() {
            "turn" => "Turns",
            "subagent" => "Subagents",
            "compaction" => "Compaction",
            "auto-review" => "Auto-review",
            "title" => "Titles",
            other => other,
        };
        lines.push(format!(
            "{name}: input {} · output {} · cache read {} · cache write {}",
            sum.input, sum.output, sum.cache_read, sum.cache_write
        ));
    }
    lines.push(
        if usage.occupancy.estimated {
            "Window occupancy is estimated from conversation history."
        } else {
            "Window occupancy uses the latest main request's reported input, including cache tokens."
        }
        .into(),
    );
    if usage.occupancy.context_window.is_none_or(|n| n == 0) {
        lines.push("Context window size is unknown.".into());
    }
    lines
}

struct UsageTooltip(Vec<String>);

impl Render for UsageTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let card = div()
            .max_w(px(540.0))
            .px(px(9.0))
            .py(px(7.0))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(if theme.is_frost() {
                theme.glass_overlay()
            } else {
                theme.surface_raised
            })
            .shadow_md()
            .text_size(px(11.0))
            .text_color(theme.text_muted)
            .children(
                self.0
                    .iter()
                    .map(|line| div().child(SharedString::from(line.clone()))),
            );
        crate::frost::frosted(6.0, crate::frost::MENU_BLUR, card)
    }
}

pub(crate) fn render(usage: &ChatUsage, theme: &Theme) -> AnyElement {
    let details = details(usage);
    div()
        .id("chat-usage")
        .debug_selector(|| "chat-usage".into())
        .min_w_0()
        .truncate()
        .text_color(theme.text_muted)
        .child(SharedString::from(label(usage)))
        .tooltip(move |_, cx| cx.new(|_| UsageTooltip(details.clone())).into())
        .tooltip_show_delay(std::time::Duration::from_millis(350))
        .into_any_element()
}

pub(crate) fn spawn_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    chat_id: String,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        loop {
            let result = handle
                .client()
                .subscribe_checked(
                    methods::WATCH_CHAT_USAGE,
                    serde_json::json!({"chatId": chat_id}),
                )
                .await;
            let unsupported = matches!(&result, Err(RpcError::UnknownMethod(_)));
            if let Ok(mut rx) = result {
                while let Some(value) = rx.recv().await {
                    let Ok(usage) = WatchCoordinator::decode::<ChatUsage>(value) else {
                        break;
                    };
                    if this
                        .update(cx, |state, cx| {
                            if state.selected_chat.as_deref() == Some(&chat_id) {
                                state.chat_usage = Some(usage);
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }
            if this
                .update(cx, |state, cx| {
                    if state.selected_chat.as_deref() == Some(&chat_id) {
                        state.chat_usage = None;
                        cx.notify();
                    }
                })
                .is_err()
                || unsupported
            {
                return;
            }
            cx.background_executor()
                .timer(WatchCoordinator::RETRY_DELAY)
                .await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{StreamExt, channel::mpsc};
    use gpui::{Entity, TestAppContext};
    use holt_rpc::{RpcReply, RpcService, memory_client};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    fn frame(gross: u64, window: Option<u64>, estimated: bool) -> Value {
        json!({"gross":gross,"byKind":{"turn":{"input":40000,"output":2000,"cacheRead":3000,"cacheWrite":500}},
            "recordCount":3,"occupancy":{"tokens":40000,"contextWindow":window,"estimated":estimated}})
    }

    #[test]
    fn usage_labels_and_details_follow_the_wire_values() {
        let usage: ChatUsage =
            WatchCoordinator::decode(frame(412000, Some(1000000), false)).unwrap();
        assert_eq!(label(&usage), "Total 412k · Window 40k / 1M · 4%");
        assert_eq!(details(&usage)[0], "412000 tokens · 3 records");
        assert_eq!(
            details(&usage)[1],
            "Turns: input 40000 · output 2000 · cache read 3000 · cache write 500"
        );
        let unknown: ChatUsage = WatchCoordinator::decode(frame(412000, None, true)).unwrap();
        assert_eq!(label(&unknown), "Total 412k · Window ≈40k");
        assert!(
            details(&unknown)
                .iter()
                .any(|line| line.contains("estimated"))
        );
        let zero_window: ChatUsage =
            WatchCoordinator::decode(frame(412000, Some(0), false)).unwrap();
        assert_eq!(label(&zero_window), "Total 412k · Window 40k");
    }

    #[derive(Default)]
    struct FakeEngine {
        watches: Mutex<Vec<(String, mpsc::UnboundedSender<Value>)>>,
    }

    #[async_trait::async_trait]
    impl RpcService for FakeEngine {
        async fn handle(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
            if method == methods::WATCH_CHAT_USAGE {
                let id = params["chatId"].as_str().unwrap().to_string();
                if id == "old-engine" {
                    return Err(RpcError::UnknownMethod(method.into()));
                }
                let (tx, rx) = mpsc::unbounded();
                tx.unbounded_send(frame(412000, Some(1000000), true))
                    .unwrap();
                self.watches.lock().unwrap().push((id, tx));
                return Ok(RpcReply::Stream(rx.boxed()));
            }
            Ok(RpcReply::Stream(futures::stream::pending().boxed()))
        }
    }

    fn pump(runtime: &tokio::runtime::Runtime, cx: &TestAppContext) {
        for _ in 0..20 {
            runtime.block_on(async { tokio::task::yield_now().await });
            cx.run_until_parked();
        }
    }

    fn displayed_label(state: &Entity<AppState>, cx: &TestAppContext) -> Option<String> {
        cx.read(|cx| state.read(cx).chat_usage.as_ref().map(super::label))
    }

    #[gpui::test]
    fn selected_usage_updates_switches_and_hides_on_version_skew(cx: &mut TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let engine = Arc::new(FakeEngine::default());
        let client = {
            let _enter = runtime.enter();
            memory_client(engine.clone())
        };
        let state = cx.new(|_| AppState::new());
        state.update(cx, |state, cx| {
            state.attach_test_engine(client, cx);
            state.select_chat(Some("a".into()), cx);
        });
        pump(&runtime, cx);
        assert_eq!(
            displayed_label(&state, cx).as_deref(),
            Some("Total 412k · Window ≈40k / 1M · ≈4%")
        );
        engine.watches.lock().unwrap()[0]
            .1
            .unbounded_send(frame(824000, None, false))
            .unwrap();
        pump(&runtime, cx);
        assert_eq!(
            displayed_label(&state, cx).as_deref(),
            Some("Total 824k · Window 40k")
        );

        state.update(cx, |state, cx| state.select_chat(Some("b".into()), cx));
        assert!(displayed_label(&state, cx).is_none());
        pump(&runtime, cx);
        assert!(engine.watches.lock().unwrap()[0].1.is_closed());
        assert_eq!(engine.watches.lock().unwrap()[1].0, "b");
        assert_eq!(
            displayed_label(&state, cx).as_deref(),
            Some("Total 412k · Window ≈40k / 1M · ≈4%")
        );

        state.update(cx, |state, cx| {
            state.select_chat(Some("old-engine".into()), cx)
        });
        pump(&runtime, cx);
        assert!(displayed_label(&state, cx).is_none());
        state.update(cx, |state, cx| state.select_chat(None, cx));
        pump(&runtime, cx);
        assert!(displayed_label(&state, cx).is_none());
    }

    #[gpui::test]
    fn scripted_engine_totals_reach_the_selected_status_line(cx: &mut TestAppContext) {
        use super::engine_fixture::{self, Fixture, ScriptedProvider, ScriptedReply};
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let fixture = Fixture::new();
        let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]).with_usage(
            pi_core::ai::types::Usage {
                input: 700,
                output: 70,
                cache_read: 7,
                cache_write: 3,
                total_tokens: 780,
                ..Default::default()
            },
        );
        let engine = Arc::new(fixture.engine(&provider));
        runtime.block_on(engine_fixture::setup_chat(&engine, "chat-1"));
        let client = {
            let _enter = runtime.enter();
            memory_client(engine.clone())
        };
        let state = cx.new(|_| AppState::new());
        state.update(cx, |state, cx| state.attach_test_engine(client, cx));
        pump(&runtime, cx);
        state.update(cx, |state, cx| state.select_chat(Some("chat-1".into()), cx));
        pump(&runtime, cx);
        runtime.block_on(async {
            let RpcReply::Stream(mut events) = engine
                .handle(methods::WATCH_TURN_TERMINAL_EVENTS, json!({}))
                .await
                .unwrap()
            else {
                panic!("missing events")
            };
            engine_fixture::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
            engine_fixture::next_frame(&mut events).await;
        });
        pump(&runtime, cx);
        let snapshot = runtime.block_on(async {
            let RpcReply::Stream(mut stream) = engine
                .handle(methods::WATCH_CHAT_USAGE, json!({"chatId":"chat-1"}))
                .await
                .unwrap()
            else {
                panic!("missing usage watch")
            };
            engine_fixture::next_frame(&mut stream).await
        });
        pump(&runtime, cx);
        cx.read(|cx| {
            let usage = state.read(cx).chat_usage.as_ref().unwrap();
            // The UI holds exactly the engine's frame — it must not
            // recalculate occupancy from totals or the model picker — and
            // prints it with no re-derivation of its own.
            let engine_frame: ChatUsage = serde_json::from_value(snapshot.clone()).unwrap();
            assert_eq!(usage, &engine_frame);
            // Pinned end to end: gross 700 + 70 + 7 + 3, occupancy the main
            // run's own request input with both cache fields (700 + 7 + 3),
            // measured — and a chat with no stored model has no window to
            // divide by, so no percentage.
            assert_eq!(usage.gross, 780);
            assert_eq!(usage.occupancy.tokens, 710);
            // Run acceptance stores the request's model on the chat, so a
            // settled queue divides by that model's catalog window — 272k for
            // this one. The UI's job is to print what the frame carries; the
            // catalog number itself is the engine's to know.
            assert!(usage.occupancy.context_window.is_some_and(|n| n > 0));
            let shown = label(usage);
            assert!(
                shown.starts_with("Total 780 · Window 710 / ") && shown.ends_with('%'),
                "{shown}"
            );
            assert_eq!(details(usage)[0], "780 tokens · 1 records");
            assert_eq!(
                details(usage)[1],
                "Turns: input 700 · output 70 · cache read 7 · cache write 3"
            );
        });
    }
}
