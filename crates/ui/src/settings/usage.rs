//! The Usage settings page (usage-overview spec, tickets 02+): the
//! device-level usage aggregate from one unary `UsageStats` call, read
//! fresh on every entry. This ticket builds the page shell — the four
//! Loadable states plus the header bar's chat count, range switcher, and
//! refresh — the charts, heatmap, and breakdown tabs land in tickets
//! 03–05. Everything on the page is read-only: the only controls reload
//! the same aggregate.

use gpui::{Context, Entity, IntoElement, Render, SharedString, Task, Window, div, prelude::*, px};
use holt_proto::UsageStatsReply;
use holt_rpc::methods;

use crate::{
    icons,
    popover::{self, Loadable},
    state::AppState,
    theme::Theme,
};

use super::widgets;

/// The page copy (settings pages carry no i18n — verbatim spec strings).
pub(crate) const PAGE_TITLE: &str = "Usage";
pub(crate) const PAGE_DESCRIPTION: &str = "Model token usage";
pub(crate) const ERROR_TITLE: &str = "Couldn't load model usage";
pub(crate) const EMPTY_TITLE: &str = "No model usage yet";
pub(crate) const EMPTY_DETAIL: &str = "Token usage is collected from the chats you run in \
     holt. Run a few chats and usage will appear here.";
pub(crate) const REFRESH_TOOLTIP: &str = "Refresh usage stats";

/// The offered ranges, and the range the page opens on.
pub(crate) const RANGES: [u32; 3] = [7, 30, 90];
pub(crate) const DEFAULT_RANGE: u32 = 30;

/// The version-skew shape (`UnknownMethod`): name the skew the way the
/// skills page does instead of echoing the raw error — and never a
/// "restart the app" instruction.
pub(crate) const VERSION_SKEW: &str =
    "Usage stats aren't available — the engine doesn't support them yet";

/// The header's count line, verbatim spec shape: "N chats · last N days".
fn header_count_text(chat_count: u64, days: u32) -> String {
    format!("{chat_count} chats · last {days} days")
}

pub struct UsagePage {
    state: Entity<AppState>,
    days: u32,
    stats: Loadable<UsageStatsReply>,
    task: Option<Task<()>>,
}

impl UsagePage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            days: DEFAULT_RANGE,
            stats: Loadable::Idle,
            task: None,
        };
        page.load(cx);
        page
    }

    /// One fresh `UsageStats` call for the current range. Every reload —
    /// entry, range switch, refresh — runs through here and lands the page
    /// back on skeletons until the reply arrives.
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.stats = Loadable::Error("Engine not connected".into());
            return;
        };
        self.stats = Loadable::Loading;
        let days = self.days;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::USAGE_STATS, serde_json::json!({ "days": days }))
                .await;
            this.update(cx, |page, cx| {
                page.stats = match result {
                    Ok(value) => serde_json::from_value(value)
                        .map(Loadable::Ready)
                        .unwrap_or_else(|error| Loadable::Error(error.to_string())),
                    Err(holt_rpc::RpcError::UnknownMethod(_)) => {
                        Loadable::Error(VERSION_SKEW.into())
                    }
                    Err(error) => Loadable::Error(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Switch the range: a full Loadable reload, not a re-filter of the old
    /// reply. Selecting the range already shown is a no-op.
    fn set_days(&mut self, days: u32, cx: &mut Context<Self>) {
        if days == self.days {
            return;
        }
        self.days = days;
        self.load(cx);
    }

    /// The refresh button: re-run the same range through a full reload.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load(cx);
    }

    /// The normal state's top bar: the count line on the left, the range
    /// switcher and refresh on the right. The count line spells the page's
    /// selected range: the reply it renders is always an answer to
    /// `self.days`, because every switch and refresh drops the in-flight
    /// task and re-enters the loading state before anything renders.
    fn render_header(
        &self,
        reply: &UsageStatsReply,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = Theme::of(cx).clone();
        let tooltip = SharedString::from(REFRESH_TOOLTIP);
        div()
            .id("usage-header")
            .debug_selector(|| "usage-header".into())
            .mt(px(20.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(header_count_text(
                        reply.chat_count,
                        self.days,
                    ))),
            )
            .children(RANGES.map(|days| self.range_chip(&theme, days, cx)))
            .child(
                div()
                    .id("usage-refresh")
                    .debug_selector(|| "usage-refresh".into())
                    .flex_none()
                    .h(px(24.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .rounded(px(6.0))
                    .text_color(theme.text_muted)
                    .cursor_pointer()
                    .hover(|state| state.bg(crate::theme::wash(0.06)).text_color(theme.text))
                    .on_click(cx.listener(|page, _, _, cx| page.refresh(cx)))
                    .tooltip(move |_, cx| {
                        cx.new(|_| crate::image_viewer::ViewerTooltip(tooltip.clone()))
                            .into()
                    })
                    .child(icons::icon(icons::REFRESH).size(px(14.0))),
            )
    }

    /// One range-switch chip — the git panel's tab chip: the active range
    /// carries a wash and full weight, the others are quiet switches.
    fn range_chip(&self, theme: &Theme, days: u32, cx: &Context<Self>) -> gpui::AnyElement {
        let active = self.days == days;
        let mut chip = div()
            .id(SharedString::from(format!("usage-range-{days}")))
            .debug_selector(move || format!("usage-range-{days}"))
            .flex_none()
            .h(px(24.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .rounded(px(6.0))
            .text_size(crate::typography::ui_rems(11.5))
            .font_weight(if active {
                gpui::FontWeight::SEMIBOLD
            } else {
                gpui::FontWeight::MEDIUM
            })
            .text_color(if active { theme.text } else { theme.text_muted });
        if active {
            chip = chip.bg(crate::theme::wash(0.06));
        } else {
            chip = chip
                .cursor_pointer()
                .hover(|state| state.bg(crate::theme::wash(0.05)).text_color(theme.text))
                .on_click(cx.listener(move |page, _, _, cx| page.set_days(days, cx)));
        }
        chip.child(SharedString::from(format!("{days}d")))
            .into_any_element()
    }

    /// The error state: the headline strip, the specific reason, and Retry —
    /// a full reload of the current range.
    fn render_error(&self, error: &str, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let theme = Theme::of(cx).clone();
        div()
            .id("usage-error")
            .debug_selector(|| "usage-error".into())
            .mt(px(16.0))
            .flex()
            .flex_col()
            .items_start()
            .gap(px(10.0))
            .child(widgets::error_strip(&theme, ERROR_TITLE))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(error.to_string())),
            )
            .child(
                widgets::ghost_action(&theme)
                    .id("usage-retry")
                    .debug_selector(|| "usage-retry".into())
                    .border_1()
                    .border_color(theme.border)
                    .hover(|style| widgets::ghost_hover(&theme, style))
                    .on_click(cx.listener(|page, _, _, cx| page.refresh(cx)))
                    .child("Retry"),
            )
    }

    /// The empty state (a fresh install): no usage anywhere on the device —
    /// normal, not an error.
    fn render_empty(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let theme = Theme::of(cx).clone();
        div()
            .id("usage-empty")
            .debug_selector(|| "usage-empty".into())
            .mt(px(24.0))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(12.5))
                    .text_color(theme.text)
                    .child(EMPTY_TITLE),
            )
            .child(
                div()
                    .text_size(crate::typography::ui_rems(12.5))
                    .text_color(theme.text_muted)
                    .child(EMPTY_DETAIL),
            )
    }
}

impl Render for UsagePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let body = match &self.stats {
            Loadable::Idle | Loadable::Loading => div()
                .debug_selector(|| "usage-skeleton".into())
                .child(popover::skeleton_rows(
                    "usage-skeleton",
                    &theme,
                    3,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            Loadable::Error(error) => self.render_error(error, cx).into_any_element(),
            Loadable::Ready(reply) => {
                if reply.chat_count == 0 {
                    self.render_empty(cx).into_any_element()
                } else {
                    self.render_header(reply, cx).into_any_element()
                }
            }
        };
        div()
            .id("usage-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, PAGE_TITLE, None))
                    .child(widgets::page_subtitle(&theme, PAGE_DESCRIPTION))
                    .child(body),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use holt_rpc::{RpcError, RpcReply, RpcService};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// What the fake engine answers the next `UsageStats` call with.
    #[derive(Clone)]
    enum Scripted {
        Ok(serde_json::Value),
        Failed(String),
        UnknownMethod,
    }

    /// A scripted `UsageStats` engine: pops the answer queue, falling back
    /// to `steady_state` once it runs dry, and records every `days` it was
    /// asked for.
    struct FakeEngine {
        answers: Mutex<VecDeque<Scripted>>,
        steady_state: Mutex<Scripted>,
        calls: Mutex<Vec<u32>>,
    }

    #[async_trait]
    impl RpcService for FakeEngine {
        async fn handle(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            if method != methods::USAGE_STATS {
                return Err(RpcError::UnknownMethod(method.to_string()));
            }
            let days = params
                .get("days")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as u32;
            self.calls.lock().unwrap().push(days);
            let answer = self
                .answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| self.steady_state.lock().unwrap().clone());
            match answer {
                Scripted::Ok(value) => RpcReply::value(&value),
                Scripted::Failed(message) => Err(RpcError::Failed(message)),
                Scripted::UnknownMethod => Err(RpcError::UnknownMethod(method.to_string())),
            }
        }
    }

    /// A ready reply the tolerant frame decodes — the aggregate's whole
    /// shape is ticket 01's contract, the page only reads the count.
    fn reply(chat_count: u64) -> Scripted {
        Scripted::Ok(serde_json::json!({
            "chatCount": chat_count,
            "days": 30,
            "totals": {
                "input": 100, "output": 10, "cacheRead": 3, "cacheWrite": 4,
                "cacheHit": 0.029, "activeDays": 2,
            },
            "models": [], "byModel": [], "byProject": [], "heatmap": [],
        }))
    }

    struct Harness<'a> {
        page: Entity<UsagePage>,
        visual: &'a mut gpui::VisualTestContext,
        engine: Arc<FakeEngine>,
        runtime: tokio::runtime::Runtime,
        _dir: tempfile::TempDir,
    }

    impl Harness<'_> {
        /// Drive RPC dispatch a few rounds (test thread), then the gpui
        /// foreground executor.
        fn pump(&mut self) {
            for _ in 0..6 {
                self.runtime
                    .block_on(async { tokio::task::yield_now().await });
                self.visual.run_until_parked();
            }
            // A window only repaints on notify, and debug_bounds reads the
            // last DRAWN frame — poke the page so the settled state is what
            // the bounds assertions see.
            self.page.update(&mut *self.visual, |_, cx| cx.notify());
            self.visual.run_until_parked();
        }

        fn present(&mut self, selector: &'static str) -> bool {
            self.visual.debug_bounds(selector).is_some()
        }

        /// Repaint without draining the RPC: `run_until_parked` alone never
        /// resolves the tokio-side call, so the page stays wherever the last
        /// state change put it.
        fn repaint(&mut self) {
            self.page.update(&mut *self.visual, |_, cx| cx.notify());
            self.visual.run_until_parked();
        }

        fn click(&mut self, selector: &'static str) {
            let bounds = self
                .visual
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} renders"));
            self.visual
                .simulate_click(bounds.center(), Default::default());
        }

        fn days(&self) -> u32 {
            self.visual.read(|cx| self.page.read(cx).days)
        }

        fn loading(&self) -> bool {
            self.visual.read(|cx| self.page.read(cx).stats.is_loading())
        }

        fn error(&self) -> Option<String> {
            self.visual
                .read(|cx| self.page.read(cx).stats.error().map(str::to_string))
        }

        fn calls(&self) -> Vec<u32> {
            self.engine.calls.lock().unwrap().clone()
        }
    }

    fn harness<'a>(
        cx: &'a mut gpui::TestAppContext,
        answers: Vec<Scripted>,
        steady_state: Scripted,
    ) -> Harness<'a> {
        let engine = Arc::new(FakeEngine {
            answers: Mutex::new(answers.into()),
            steady_state: Mutex::new(steady_state),
            calls: Mutex::new(Vec::new()),
        });
        // `memory_client` spawns its dispatch loop with `tokio::spawn`,
        // which needs a runtime context on this thread.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::default());
            crate::settings::init(crate::settings::UiSettings::default(), dir.path(), cx);
        });
        let app_state = cx.new(|_| AppState::new());
        let client = {
            let _guard = runtime.enter();
            holt_rpc::memory_client(engine.clone())
        };
        app_state.update(cx, |state, cx| state.attach_test_engine(client, cx));
        let (page, visual) =
            cx.add_window_view(|_window, cx| UsagePage::new(app_state.clone(), cx));
        let mut harness = Harness {
            page,
            visual,
            engine,
            runtime,
            _dir: dir,
        };
        harness.pump();
        harness
    }

    /// The count line and the rest of the page copy are verbatim spec
    /// strings — pinned here against accidental drift.
    #[test]
    fn the_copy_matches_the_spec_verbatim() {
        assert_eq!(header_count_text(3, 30), "3 chats · last 30 days");
        assert_eq!(header_count_text(12, 7), "12 chats · last 7 days");
        assert_eq!(header_count_text(1, 90), "1 chats · last 90 days");
        assert_eq!(PAGE_TITLE, "Usage");
        assert_eq!(PAGE_DESCRIPTION, "Model token usage");
        assert_eq!(ERROR_TITLE, "Couldn't load model usage");
        assert_eq!(EMPTY_TITLE, "No model usage yet");
        assert_eq!(
            EMPTY_DETAIL,
            "Token usage is collected from the chats you run in holt. Run \
             a few chats and usage will appear here."
        );
        assert_eq!(REFRESH_TOOLTIP, "Refresh usage stats");
        // Version skew names the skew, never a "restart the app" instruction.
        assert_eq!(
            VERSION_SKEW,
            "Usage stats aren't available — the engine doesn't support them yet"
        );
        assert!(!VERSION_SKEW.to_lowercase().contains("restart"));
    }

    #[gpui::test]
    fn the_ready_reply_renders_the_header_bar(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, vec![], reply(3));

        assert_eq!(harness.days(), 30, "the page opens on the 30d range");
        assert!(!harness.loading());
        assert_eq!(harness.error(), None);
        assert!(harness.present("usage-header"), "the normal state renders");
        assert!(harness.present("usage-range-7"));
        assert!(harness.present("usage-range-30"));
        assert!(harness.present("usage-range-90"));
        assert!(harness.present("usage-refresh"));
        assert!(
            !harness.present("usage-empty"),
            "a reply with chats never shows the empty state"
        );
        assert_eq!(harness.calls(), vec![30], "one call on entry");
    }

    #[gpui::test]
    fn an_engine_without_the_method_lands_in_version_skew_copy(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, vec![], Scripted::UnknownMethod);

        assert_eq!(
            harness.error().as_deref(),
            Some(VERSION_SKEW),
            "UnknownMethod reads as version skew, not the raw error"
        );
        assert!(
            harness.present("usage-retry"),
            "the error state offers Retry"
        );
        assert!(!harness.present("usage-header"));
    }

    #[gpui::test]
    fn a_failure_shows_the_headline_and_retry_recovers(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(
            cx,
            vec![Scripted::Failed("the engine fell over".into())],
            reply(2),
        );

        // First load failed: the headline strip, the specific reason, Retry.
        assert_eq!(
            harness.error().as_deref(),
            Some("the engine fell over"),
            "the error carries the specific reason"
        );
        assert!(harness.present("usage-retry"));

        // Retry is a full reload of the current range.
        harness.click("usage-retry");
        assert!(harness.loading(), "retry re-enters the loading state");
        harness.pump();
        assert_eq!(harness.error(), None);
        assert!(harness.present("usage-header"));
        assert_eq!(harness.calls(), vec![30, 30], "retry re-sent the RPC");
    }

    #[gpui::test]
    fn a_fresh_install_shows_the_empty_copy(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, vec![], reply(0));

        assert!(harness.present("usage-empty"));
        assert!(!harness.present("usage-header"), "no header without usage");
        assert_eq!(harness.error(), None, "empty is not an error");
    }

    #[gpui::test]
    fn switching_range_and_refreshing_rerun_the_rpc_through_loading(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, vec![], reply(5));
        assert_eq!(harness.calls(), vec![30]);

        // The range switch re-queries with the new days, through the loading
        // state — not a re-filter of the old reply.
        harness.click("usage-range-7");
        assert!(harness.loading(), "the switch lands the page on skeletons");
        assert_eq!(harness.days(), 7);
        harness.repaint();
        assert!(
            harness.present("usage-skeleton"),
            "the loading state renders skeleton rows"
        );
        assert!(
            !harness.present("usage-header"),
            "no stale header while the reload is in flight"
        );
        harness.pump();
        assert!(harness.present("usage-header"));
        assert_eq!(harness.calls(), vec![30, 7]);

        // Refresh re-runs the current range.
        harness.click("usage-refresh");
        assert!(harness.loading());
        harness.pump();
        assert!(harness.present("usage-header"));
        assert_eq!(harness.days(), 7);
        assert_eq!(harness.calls(), vec![30, 7, 7]);

        // Back to 90d.
        harness.click("usage-range-90");
        harness.pump();
        assert_eq!(harness.days(), 90);
        assert_eq!(harness.calls(), vec![30, 7, 7, 90]);
    }
}
