//! The Usage settings page (usage-overview spec, tickets 02+): the
//! device-level usage aggregate from one unary `UsageStats` call, read
//! fresh on every entry. Ticket 02 built the page shell — the four
//! Loadable states plus the header bar; ticket 03 adds the summary area:
//! the Total and per-model legend, the stacked daily area chart, and the
//! six metric tiles. Everything on the page is read-only: the only
//! controls reload the same aggregate, and the legend's curve visibility
//! is ephemeral page state — a reload resets it.
//!
//! The chart is gpui self-drawn: one canvas painting each visible model's
//! stacked area as a filled polygon over the shared day axis (the git
//! graph's palette, so legend dots and layers share one color per rank),
//! with the day readout riding the existing tooltip infra — one hover
//! column per day, each opening the same card the usage ring's hover
//! opens.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, Hsla, IntoElement, PathBuilder, Render, SharedString, Task,
    Window, canvas, div, point, prelude::*, px,
};
use holt_proto::UsageStatsReply;
use holt_rpc::methods;

use crate::{
    chat_usage::percent,
    icons,
    popover::{self, Loadable},
    state::AppState,
    theme::Theme,
    token_display::compact_tokens,
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

/// The chart block's fixed height; the day columns and the canvas share it.
const CHART_HEIGHT: f32 = 220.0;
/// The summary's left column: legend rows with air for a model id.
const LEGEND_WIDTH: f32 = 260.0;
/// Day tooltips appear faster than the 350ms hover cards: scanning across
/// thirty columns should read each day without slowing to a crawl.
const DAY_TOOLTIP_DELAY: Duration = Duration::from_millis(150);

/// The header's count line, verbatim spec shape: "N chats · last N days".
fn header_count_text(chat_count: u64, days: u32) -> String {
    format!("{chat_count} chats · last {days} days")
}

// ---------------------------------------------------------------------------
// The summary model: the reply folded into what the summary area prints
// ---------------------------------------------------------------------------

/// One model's chart-ready series. `id` is the legend's key, `tokens` the
/// range total, `per_day` the gross tokens per day — aligned with
/// [`Summary::dates`] by construction (padded or truncated there, so a
/// lagging series can never skew the stack).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChartSeries {
    id: String,
    tokens: u64,
    per_day: Vec<u64>,
}

/// The summary area's whole input, folded once per render: the grand
/// total, the shared day axis, and the series in the reply's own order —
/// total descending, which is both the legend's order and the stack's
/// bottom-to-top order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Summary {
    grand_total: u64,
    dates: Vec<String>,
    series: Vec<ChartSeries>,
}

impl Summary {
    /// The visible series with their reply ranks — the rank is what ties a
    /// legend row and its area layer to one color.
    fn visible(&self, hidden: &BTreeSet<String>) -> Vec<(usize, &ChartSeries)> {
        self.series
            .iter()
            .enumerate()
            .filter(|(_, series)| !hidden.contains(&series.id))
            .collect()
    }

    /// One day's visible total — the number the tooltip's Total row and the
    /// stack height both print, so the readout always matches the picture.
    fn day_total(&self, hidden: &BTreeSet<String>, day: usize) -> u64 {
        self.visible(hidden)
            .iter()
            .map(|(_, series)| series.per_day.get(day).copied().unwrap_or(0))
            .sum()
    }

    /// The Y scale: the tallest visible day total, floored at 1 so an
    /// all-zero window still divides.
    fn peak_day_total(&self, hidden: &BTreeSet<String>) -> u64 {
        (0..self.dates.len())
            .map(|day| self.day_total(hidden, day))
            .max()
            .unwrap_or(0)
            .max(1)
    }

    /// The tooltip's rows for one day: visible models with their day tokens,
    /// in stack order.
    fn day_rows(&self, hidden: &BTreeSet<String>, day: usize) -> Vec<(usize, String, u64)> {
        self.visible(hidden)
            .into_iter()
            .map(|(rank, series)| {
                (
                    rank,
                    series.id.clone(),
                    series.per_day.get(day).copied().unwrap_or(0),
                )
            })
            .collect()
    }
}

/// Fold the reply once: the day axis comes off the first series (the
/// engine zero-fills every series to the same length), each series' total
/// is its own per-day sum, and the grand total sums the visible-by-default
/// set.
fn fold_summary(reply: &UsageStatsReply) -> Summary {
    let width = reply
        .models
        .first()
        .map(|series| series.days.len())
        .unwrap_or(0);
    let dates = reply
        .models
        .first()
        .map(|series| {
            series
                .days
                .iter()
                .map(|day| day.date.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let series: Vec<ChartSeries> = reply
        .models
        .iter()
        .map(|model| {
            let mut per_day: Vec<u64> = model.days.iter().map(|day| day.tokens).collect();
            per_day.resize(width, 0);
            ChartSeries {
                tokens: per_day.iter().sum(),
                id: format!("{}/{}", model.provider, model.model),
                per_day,
            }
        })
        .collect();
    let grand_total = series.iter().map(|series| series.tokens).sum();
    Summary {
        grand_total,
        dates,
        series,
    }
}

/// A series color: the git graph's hue set (accent, success, warning,
/// danger, muted), at full saturation — area fills read at a different
/// weight than that chart's thin lanes, so history's desaturation stays
/// there. `busy` is left out: it is accent-derived and collapses into the
/// first slot in accent-themed builds. The legend dot and the area layer
/// take the same color, which is what ties them together; past the
/// palette the hues cycle.
fn series_color(rank: usize, theme: &Theme) -> Hsla {
    const PALETTE: [fn(&Theme) -> Hsla; 5] = [
        |theme| theme.accent,
        |theme| theme.success,
        |theme| theme.warning,
        |theme| theme.danger,
        |theme| theme.text_muted,
    ];
    PALETTE[rank % PALETTE.len()](theme)
}

/// Hide or restore one model's curve. The last visible model refuses to
/// hide: a chart of nothing is not a view of the data.
fn toggle_hidden(hidden: &mut BTreeSet<String>, summary: &Summary, id: &str) {
    if !hidden.contains(id) {
        if summary.visible(hidden).len() <= 1 {
            return;
        }
        hidden.insert(id.to_string());
    } else {
        hidden.remove(id);
    }
}

// ---------------------------------------------------------------------------
// The metric tiles
// ---------------------------------------------------------------------------

/// One tile's printed state: the compact value on the tile, the exact
/// counts its hover tooltip shows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MetricTile {
    label: &'static str,
    value: String,
    detail: String,
}

/// The six range metrics. Token tiles print [`compact_tokens`] and spell
/// the exact count on hover; Cache hit prints the engine's own rate — the
/// one computed with cache writes out of the denominator — and Active
/// days names its range.
fn metric_tiles(reply: &UsageStatsReply) -> Vec<MetricTile> {
    let totals = &reply.totals;
    let token_tile = |label: &'static str, tokens: u64| MetricTile {
        label,
        value: compact_tokens(tokens),
        detail: format!("{tokens} tokens"),
    };
    let mut tiles = vec![
        token_tile("Input", totals.input),
        token_tile("Output", totals.output),
        token_tile("Cache read", totals.cache_read),
        token_tile("Cache write", totals.cache_write),
    ];
    tiles.push(MetricTile {
        label: "Cache hit",
        value: totals.cache_hit.map_or_else(|| "—".to_string(), percent),
        detail: format!(
            "{} of {} prompt tokens served from cache",
            totals.cache_read,
            totals.input + totals.cache_read
        ),
    });
    tiles.push(MetricTile {
        label: "Active days",
        value: totals.active_days.to_string(),
        detail: format!(
            "{} of {} days in range had usage",
            totals.active_days, reply.days
        ),
    });
    tiles
}

pub struct UsagePage {
    state: Entity<AppState>,
    days: u32,
    stats: Loadable<UsageStatsReply>,
    task: Option<Task<()>>,
    /// Hidden models' series ids — the legend's click-to-hide state. Page
    /// -local and ephemeral: every reload clears it.
    hidden: BTreeSet<String>,
}

impl UsagePage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            days: DEFAULT_RANGE,
            stats: Loadable::Idle,
            task: None,
            hidden: BTreeSet::new(),
        };
        page.load(cx);
        page
    }

    /// One fresh `UsageStats` call for the current range. Every reload —
    /// entry, range switch, refresh — runs through here, drops the
    /// previous call, and lands the page back on skeletons until the reply
    /// arrives. Curve visibility resets with it: the new reply is a new
    /// view, not a filter over the old one.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.hidden.clear();
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

    /// Hide or restore one legend row's curve. The last visible model
    /// refuses: [`toggle_hidden`] is the policy, the page just redraws.
    fn toggle_model(&mut self, id: String, cx: &mut Context<Self>) {
        let summary = self.stats.ready().map(fold_summary).unwrap_or_default();
        toggle_hidden(&mut self.hidden, &summary, &id);
        cx.notify();
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

    /// The summary area: Total and legend on the left, the stacked daily
    /// area chart on the right.
    fn render_summary(
        &self,
        reply: &UsageStatsReply,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = Theme::of(cx).clone();
        let summary = Arc::new(fold_summary(reply));
        let hidden = Arc::new(self.hidden.clone());
        div()
            .id("usage-summary")
            .debug_selector(|| "usage-summary".into())
            .mt(px(16.0))
            .flex()
            .flex_row()
            .items_start()
            .gap(px(24.0))
            .child(self.render_legend(&summary, &theme, cx))
            .child(self.render_chart(&summary, &hidden, &theme))
    }

    /// The legend: the "Total tokens" headline over one row per model —
    /// dot, id, compact count, and a share bar. Clicking a row toggles its
    /// curve; a hidden row reads dimmed with a hollow dot.
    fn render_legend(
        &self,
        summary: &Arc<Summary>,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let total = summary.grand_total.max(1);
        div()
            .id("usage-legend")
            .debug_selector(|| "usage-legend".into())
            .flex_none()
            .w(px(LEGEND_WIDTH))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_muted)
                            .child("Total tokens"),
                    )
                    .child(
                        div()
                            .debug_selector(|| "usage-total".into())
                            .text_size(crate::typography::ui_rems(22.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(SharedString::from(compact_tokens(summary.grand_total))),
                    ),
            )
            .children(
                summary
                    .series
                    .iter()
                    .enumerate()
                    .map(|(rank, series)| self.legend_row(series, rank, total, theme, cx)),
            )
    }

    fn legend_row(
        &self,
        series: &ChartSeries,
        rank: usize,
        total: u64,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let color = series_color(rank, theme);
        let is_hidden = self.hidden.contains(&series.id);
        let id = series.id.clone();
        div()
            .id(SharedString::from(format!("usage-legend-{rank}")))
            .debug_selector(move || format!("usage-legend-{rank}"))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .cursor_pointer()
            .on_click(cx.listener(move |page, _, _, cx| page.toggle_model(id.clone(), cx)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(if is_hidden {
                        div()
                            .flex_none()
                            .size(px(6.0))
                            .rounded_full()
                            .border_1()
                            .border_color(color)
                    } else {
                        div().flex_none().size(px(6.0)).rounded_full().bg(color)
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.5))
                            .text_color(if is_hidden {
                                theme.text_faint
                            } else {
                                theme.text_muted
                            })
                            .child(SharedString::from(series.id.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(crate::typography::ui_rems(11.5))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(compact_tokens(series.tokens))),
                    ),
            )
            .child(
                // The share bar: the row's fraction of the grand total —
                // the usage card's track geometry at legend size.
                div()
                    .debug_selector(move || format!("usage-legend-{rank}-track"))
                    .h(px(4.0))
                    .w_full()
                    .rounded(px(2.0))
                    .overflow_hidden()
                    .bg(theme.ink(0.10))
                    .child(
                        div()
                            .debug_selector(move || format!("usage-legend-{rank}-fill"))
                            .h_full()
                            .w(gpui::relative(
                                (series.tokens as f32 / total as f32).clamp(0.0, 1.0),
                            ))
                            .rounded(px(2.0))
                            .bg(if is_hidden { color.opacity(0.3) } else { color }),
                    ),
            )
            .when(is_hidden, |row| row.opacity(0.75))
    }

    /// The stacked area chart: the canvas plus one hover column per day,
    /// each opening the day readout, over a shared Y gutter.
    fn render_chart(
        &self,
        summary: &Arc<Summary>,
        hidden: &Arc<BTreeSet<String>>,
        theme: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        let peak = summary.peak_day_total(hidden);
        let mid = compact_tokens(peak / 2);
        let days = summary.dates.len();
        div()
            .id("usage-chart")
            .debug_selector(|| "usage-chart".into())
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(6.0))
                    // Y gutter: the scale's top, middle, and zero.
                    .child(
                        div()
                            .flex_none()
                            .w(px(34.0))
                            .h(px(CHART_HEIGHT))
                            .flex()
                            .flex_col()
                            .items_end()
                            .justify_between()
                            .text_size(crate::typography::ui_rems(10.0))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(compact_tokens(peak)))
                            .child(SharedString::from(mid))
                            .child("0"),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .h(px(CHART_HEIGHT))
                            .child(area_chart(summary, hidden, peak, theme))
                            .child(div().absolute().inset_0().flex().children(
                                (0..days).map(|day| self.day_column(summary, hidden, day, theme)),
                            )),
                    ),
            )
            .child(
                // X labels: the range's start, middle, and end dates, under
                // the chart body (past the Y gutter).
                div()
                    .flex()
                    .flex_row()
                    .gap(px(6.0))
                    .child(div().flex_none().w(px(34.0)))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .text_size(crate::typography::ui_rems(10.0))
                            .text_color(theme.text_faint)
                            .children(
                                [0, days.saturating_sub(1) / 2, days.saturating_sub(1)]
                                    .into_iter()
                                    .filter_map(|ix| summary.dates.get(ix).cloned())
                                    .map(SharedString::from),
                            ),
                    ),
            )
    }

    /// One day's hover column — the full-height strip over that day's
    /// slice of the axis. The strip itself stays invisible; its tooltip is
    /// the day readout.
    fn day_column(
        &self,
        summary: &Arc<Summary>,
        hidden: &Arc<BTreeSet<String>>,
        day: usize,
        theme: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        let card = DayCard {
            date: summary.dates.get(day).cloned().unwrap_or_default(),
            rows: Arc::new(
                summary
                    .day_rows(hidden, day)
                    .into_iter()
                    .map(|(rank, id, tokens)| DayRow {
                        color: series_color(rank, theme),
                        id,
                        tokens,
                    })
                    .collect(),
            ),
            total: summary.day_total(hidden, day),
        };
        div()
            .id(SharedString::from(format!("usage-day-{day}")))
            .debug_selector(move || format!("usage-day-{day}"))
            .flex_1()
            .h_full()
            .cursor_default()
            .tooltip(move |_, cx| cx.new(|_| card.clone()).into())
            .tooltip_show_delay(DAY_TOOLTIP_DELAY)
    }

    /// The six metric tiles: compact values on the tiles, exact counts on
    /// hover.
    fn render_metrics(
        &self,
        reply: &UsageStatsReply,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = Theme::of(cx).clone();
        div()
            .id("usage-metrics")
            .debug_selector(|| "usage-metrics".into())
            .mt(px(20.0))
            .flex()
            .flex_row()
            .gap(px(8.0))
            .children(metric_tiles(reply).into_iter().map(|tile| {
                let slug = tile.label.to_lowercase().replace(' ', "-");
                div()
                    .id(SharedString::from(format!("usage-metric-{slug}")))
                    .debug_selector(move || format!("usage-metric-{slug}"))
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .px(px(12.0))
                    .py(px(10.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(theme.border)
                    .cursor_default()
                    .tooltip(move |_, cx| {
                        cx.new(|_| crate::image_viewer::ViewerTooltip(tile.detail.clone().into()))
                            .into()
                    })
                    .tooltip_show_delay(DAY_TOOLTIP_DELAY)
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_muted)
                            .child(tile.label),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(15.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(SharedString::from(tile.value)),
                    )
            }))
    }
}

/// The chart's paint pass: three faint gridlines, then each visible
/// model's area as a filled polygon between the cumulative stack below it
/// and above it — the reply's own order stacks the biggest model at the
/// bottom. `peak` is the tallest visible stacked day total, the very
/// number the Y gutter prints: one scale, derived once in
/// [`Summary::peak_day_total`], so the picture can never disagree with
/// its labels (a stacked top touches the peak line at most).
fn area_chart(
    summary: &Arc<Summary>,
    hidden: &Arc<BTreeSet<String>>,
    peak: u64,
    theme: &Theme,
) -> AnyElement {
    let grid = theme.ink(0.06);
    let visible: Vec<(Hsla, Vec<u64>)> = summary
        .visible(hidden)
        .into_iter()
        .map(|(rank, series)| (series_color(rank, theme), series.per_day.clone()))
        .collect();
    let days = summary.dates.len();
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            if days < 2 {
                return;
            }
            let max = peak.max(1) as f32;
            let height = bounds.size.height;
            let width = bounds.size.width;
            for fraction in [0.25, 0.5, 0.75] {
                let y = bounds.origin.y + height * fraction;
                let mut builder = PathBuilder::stroke(px(1.0));
                builder.move_to(point(bounds.origin.x, y));
                builder.line_to(point(bounds.origin.x + width, y));
                if let Ok(path) = builder.build() {
                    window.paint_path(path, grid);
                }
            }
            let x_at = |day: usize| bounds.origin.x + width * (day as f32 / (days - 1) as f32);
            let y_at = |tokens: u64| bounds.origin.y + height * (1.0 - tokens as f32 / max);
            let mut below = vec![0u64; days];
            for (color, per_day) in &visible {
                let mut polygon = Vec::with_capacity(days * 2);
                for (day, spot) in below.iter_mut().enumerate() {
                    *spot += per_day.get(day).copied().unwrap_or(0);
                    polygon.push(point(x_at(day), y_at(*spot)));
                }
                for day in (0..days).rev() {
                    polygon.push(point(x_at(day), y_at(below[day])));
                }
                let mut builder = PathBuilder::fill();
                builder.add_polygon(&polygon, true);
                if let Ok(path) = builder.build() {
                    window.paint_path(path, color.opacity(0.85));
                }
            }
        },
    )
    .absolute()
    .inset_0()
    .into_any_element()
}

/// One model's line in the day readout.
#[derive(Clone)]
struct DayRow {
    color: Hsla,
    id: String,
    tokens: u64,
}

/// The day readout a hover column opens: the date, one row per visible
/// model in stack order — same dot colors as the chart — and the day's
/// Total. The same card the usage ring's hover opens, fed by the reply.
#[derive(Clone)]
struct DayCard {
    date: String,
    rows: Arc<Vec<DayRow>>,
    total: u64,
}

impl Render for DayCard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let mut card = popover::popover_card(&theme)
            .w(px(200.0))
            .p(px(8.0))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .text_size(crate::typography::ui_rems(11.5))
            .child(
                div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(SharedString::from(self.date.clone())),
            );
        for row in self.rows.iter() {
            card = card.child(
                div()
                    .debug_selector(move || format!("usage-dayrow-{}", row.id))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(div().flex_none().size(px(6.0)).rounded_full().bg(row.color))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.text_muted)
                            .child(SharedString::from(row.id.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.text)
                            .child(SharedString::from(row.tokens.to_string())),
                    ),
            );
        }
        if !self.rows.is_empty() {
            card = card.child(div().h(px(1.0)).bg(theme.ink(0.08))).child(
                div()
                    .debug_selector(|| "usage-day-total".into())
                    .flex()
                    .flex_row()
                    .justify_between()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child("Total")
                    .child(SharedString::from(compact_tokens(self.total))),
            );
        }
        crate::frost::frosted(popover::CARD_RADIUS, crate::frost::MENU_BLUR, card)
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
                    div()
                        .flex()
                        .flex_col()
                        .child(self.render_header(reply, cx))
                        .child(self.render_summary(reply, cx))
                        .child(self.render_metrics(reply, cx))
                        .into_any_element()
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

        /// An element's measured bounds — the geometry-assertion lookup.
        fn bounds(&mut self, selector: &'static str) -> gpui::Bounds<gpui::Pixels> {
            self.visual
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} renders"))
        }
    }

    /// A fraction of a measured width, within a pixel of layout rounding.
    fn assert_close(measured: f32, expected: f32, what: &str) {
        assert!(
            (measured - expected).abs() < 0.02,
            "{what}: {measured} != {expected}"
        );
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

    // -------------------------------------------------------------------
    // Ticket 03 — the summary area: Total, legend, chart, metric tiles
    // -------------------------------------------------------------------

    /// A two-model reply over a three-day axis. Openai carries 300 tokens
    /// (75%: 200/50/50), anthropic 100 (25%: 0/60/40); day totals are
    /// 200/110/90 with a 200 peak.
    fn summary_reply_json() -> serde_json::Value {
        serde_json::json!({
            "chatCount": 2,
            "days": 30,
            "totals": {
                "input": 500, "output": 50, "cacheRead": 40, "cacheWrite": 10,
                "cacheHit": 0.074, "activeDays": 3,
            },
            "models": [
                {"provider": "openai", "model": "gpt-5.4", "days": [
                    {"date": "2026-09-14", "tokens": 200},
                    {"date": "2026-09-15", "tokens": 50},
                    {"date": "2026-09-16", "tokens": 50},
                ]},
                {"provider": "anthropic", "model": "claude-opus", "days": [
                    {"date": "2026-09-14", "tokens": 0},
                    {"date": "2026-09-15", "tokens": 60},
                    {"date": "2026-09-16", "tokens": 40},
                ]},
            ],
            "byModel": [], "byProject": [], "heatmap": [],
        })
    }

    fn summary_reply() -> Scripted {
        Scripted::Ok(summary_reply_json())
    }

    fn decode_reply(value: serde_json::Value) -> UsageStatsReply {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn the_fold_aligns_series_on_the_shared_day_axis() {
        let summary = fold_summary(&decode_reply(summary_reply_json()));
        assert_eq!(summary.series.len(), 2);
        assert_eq!(summary.dates, ["2026-09-14", "2026-09-15", "2026-09-16"]);
        // Series keep the reply's order (total descending) and sum their
        // own days into the legend's counts.
        assert_eq!(summary.series[0].id, "openai/gpt-5.4");
        assert_eq!(summary.series[0].tokens, 300);
        assert_eq!(summary.series[0].per_day, vec![200, 50, 50]);
        assert_eq!(summary.series[1].id, "anthropic/claude-opus");
        assert_eq!(summary.series[1].tokens, 100);
        assert_eq!(summary.grand_total, 400);

        // A series shorter than the axis is padded with zeroes, so it can
        // never skew the stack or the alignment.
        let mut short = decode_reply(summary_reply_json());
        short.models[1].days.pop();
        let summary = fold_summary(&short);
        assert_eq!(summary.series[1].per_day, vec![0, 60, 0]);
        assert_eq!(summary.series[1].tokens, 60);
    }

    #[test]
    fn hidden_models_leave_the_day_readout_and_the_scale() {
        let summary = fold_summary(&decode_reply(summary_reply_json()));
        let mut hidden = BTreeSet::new();

        // All visible: the tooltip rows list both models, the day Total is
        // their sum.
        assert_eq!(summary.day_rows(&hidden, 1).len(), 2);
        assert_eq!(summary.day_total(&hidden, 1), 110);
        assert_eq!(summary.peak_day_total(&hidden), 200);

        // Hiding openai drops its tokens from the day total, the rows, and
        // the Y scale — the readout always matches the picture.
        hidden.insert("openai/gpt-5.4".into());
        assert_eq!(summary.day_total(&hidden, 1), 60);
        assert_eq!(summary.day_total(&hidden, 0), 0);
        assert_eq!(
            summary.peak_day_total(&hidden),
            60,
            "the scale follows the visible stack"
        );
        let rows = summary.day_rows(&hidden, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "anthropic/claude-opus");
        assert_eq!(rows[0].2, 60);
    }

    #[test]
    fn the_last_visible_model_refuses_to_hide() {
        let summary = fold_summary(&decode_reply(summary_reply_json()));
        let mut hidden = BTreeSet::new();

        toggle_hidden(&mut hidden, &summary, "anthropic/claude-opus");
        assert_eq!(hidden.len(), 1);
        // One visible model left: hiding it is refused.
        toggle_hidden(&mut hidden, &summary, "openai/gpt-5.4");
        assert!(!hidden.contains("openai/gpt-5.4"));
        // And the hidden one comes back on a second click.
        toggle_hidden(&mut hidden, &summary, "anthropic/claude-opus");
        assert!(hidden.is_empty());
    }

    #[test]
    fn metric_tiles_print_compact_and_hover_exact() {
        let tiles = metric_tiles(&decode_reply(summary_reply_json()));
        let by_label = |name: &str| {
            tiles
                .iter()
                .find(|tile| tile.label == name)
                .unwrap_or_else(|| panic!("no {name} tile"))
        };
        assert_eq!(tiles.len(), 6);
        assert_eq!(by_label("Input").value, "500");
        assert_eq!(by_label("Input").detail, "500 tokens");
        assert_eq!(by_label("Cache read").value, "40");
        assert_eq!(by_label("Cache write").value, "10");
        // The hit tile prints the engine's own rate as a percentage — the
        // one with cache writes out of the denominator — and spells the
        // exact fraction on hover.
        assert_eq!(by_label("Cache hit").value, "7.4%");
        assert_eq!(
            by_label("Cache hit").detail,
            "40 of 540 prompt tokens served from cache"
        );
        assert_eq!(by_label("Active days").value, "3");
        assert_eq!(
            by_label("Active days").detail,
            "3 of 30 days in range had usage"
        );

        // A range with no prompt tokens has no rate to print.
        let mut cold = decode_reply(summary_reply_json());
        cold.totals.cache_hit = None;
        cold.totals.input = 0;
        cold.totals.cache_read = 0;
        assert_eq!(
            metric_tiles(&cold)
                .iter()
                .find(|t| t.label == "Cache hit")
                .unwrap()
                .value,
            "—"
        );
    }

    /// The legend dot, the area layer, and the day-readout dot all take
    /// `series_color(rank)` — the one-to-one tie the ticket requires. The
    /// palette is distinct across the first ranks and cycles past itself,
    /// which is what keeps the tie unambiguous for small model counts.
    #[test]
    fn series_colors_are_distinct_per_rank_until_the_palette_cycles() {
        let theme = Theme::default();
        let ranks: Vec<Hsla> = (0..5).map(|rank| series_color(rank, &theme)).collect();
        for (i, color) in ranks.iter().enumerate() {
            for other in ranks.iter().skip(i + 1) {
                assert_ne!(color, other, "ranks {i} and beyond share a color");
            }
        }
        assert_eq!(series_color(5, &theme), series_color(0, &theme));
        assert_eq!(series_color(0, &theme), theme.accent);
        assert_eq!(series_color(1, &theme), theme.success);
    }

    #[gpui::test]
    fn the_summary_paints_legend_bars_in_share_proportions(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, vec![], summary_reply());
        assert!(harness.present("usage-summary"));
        assert!(harness.present("usage-total"));

        // The chart body paints at its fixed height, one hover column per
        // day of the axis — three here, each the full chart height (the
        // chart root adds the X-label row below).
        let column = harness.bounds("usage-day-1");
        assert_eq!(column.size.height, px(CHART_HEIGHT));
        assert!(harness.present("usage-day-0"));
        assert!(harness.present("usage-day-2"));
        assert!(!harness.present("usage-day-3"));

        // Legend share bars carry each model's fraction of the grand total
        // — 75% and 25% of a 400-token month.
        let track = harness.bounds("usage-legend-0-track");
        let fill = harness.bounds("usage-legend-0-fill");
        assert_close(fill.size.width / track.size.width, 0.75, "openai share");
        let track = harness.bounds("usage-legend-1-track");
        let fill = harness.bounds("usage-legend-1-fill");
        assert_close(fill.size.width / track.size.width, 0.25, "anthropic share");
    }

    #[gpui::test]
    fn legend_clicks_toggle_curves_and_a_reload_resets_them(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, vec![], summary_reply());
        let shows = |harness: &Harness<'_>, id: &str| {
            harness
                .visual
                .read(|cx| !harness.page.read(cx).hidden.contains(id))
        };
        assert!(shows(&harness, "anthropic/claude-opus"));

        // A click hides one curve — its day readout and its stack layer.
        harness.click("usage-legend-1");
        assert!(!shows(&harness, "anthropic/claude-opus"));
        // The last visible model refuses to hide.
        harness.click("usage-legend-0");
        assert!(shows(&harness, "openai/gpt-5.4"), "the last curve stays");
        // And the hidden one restores on a second click.
        harness.click("usage-legend-1");
        assert!(shows(&harness, "anthropic/claude-opus"));

        // A reload resets visibility: the new reply is a new view.
        harness.click("usage-legend-1");
        assert!(!shows(&harness, "anthropic/claude-opus"));
        harness.click("usage-range-7");
        harness.pump();
        assert!(
            shows(&harness, "anthropic/claude-opus"),
            "a reload restores every curve"
        );
        assert!(harness.present("usage-day-0"), "back on the ready state");
    }
}
