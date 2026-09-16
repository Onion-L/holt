//! The Usage settings page (usage-overview spec, tickets 02+): the
//! device-level usage aggregate from one unary `UsageStats` call, read
//! fresh on every entry. Ticket 02 built the page shell — the four
//! Loadable states plus the header bar; ticket 03 added the summary area:
//! the Total and per-model legend, the stacked daily area chart, and the
//! six metric tiles; ticket 04 adds the year-long Activity heatmap.
//! Everything on the page is read-only: the only controls reload the same
//! aggregate, and the legend's curve visibility is ephemeral page state —
//! a reload resets it.
//!
//! The chart is gpui self-drawn: one canvas painting each visible model's
//! stacked area as a filled polygon over the shared day axis (the git
//! graph's palette, so legend dots and layers share one color per rank),
//! with the day readout riding the existing tooltip infra — one hover
//! column per day, each opening the same card the usage ring's hover
//! opens. The heatmap is the same tooltip infra over a DOM grid of cells:
//! every day is its own hover target.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Datelike;
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

// ---------------------------------------------------------------------------
// The Activity heatmap (ticket 04)
// ---------------------------------------------------------------------------

/// The heatmap cell geometry: 11px squares on a 3px gap — the GitHub
/// grid's density at desktop size.
const HEAT_CELL: f32 = 11.0;
const HEAT_GAP: f32 = 3.0;
/// The weekday-label gutter left of the grid; the month-label row indents
/// by the same amount so labels sit over their columns.
const HEAT_GUTTER: f32 = 30.0;
/// A Breakdown table's numeric column: wide enough for exact counts into
/// the hundreds of millions without reflowing the identity column.
const BREAKDOWN_COL: f32 = 76.0;

/// One placed heatmap cell: a day's tokens plus where it sits in the
/// 7-row grid — columns are weeks, rows are weekdays, Sunday first.
struct HeatCell {
    date: chrono::NaiveDate,
    tokens: u64,
    col: usize,
    row: usize,
}

/// The placed grid: the engine's fixed 365 local-day buckets ending
/// today, wrapped onto whole weeks. `cols` is 53 for every weekday the
/// year can start on.
struct HeatmapGrid {
    cells: Vec<HeatCell>,
    cols: usize,
}

/// Wrap the reply's buckets onto the Sunday-first grid. The buckets
/// arrive oldest-first ending today, so the first day's weekday sets the
/// leading offset; an unparsable date (never the engine's shape) drops
/// its cell rather than failing the whole grid.
fn fold_heatmap(reply: &UsageStatsReply) -> HeatmapGrid {
    // One unparsable date would shift every later cell onto the wrong
    // weekday row, so a single bad bucket empties the grid instead of
    // quietly corrupting it (never the engine's shape).
    let Some(mut days): Option<Vec<(chrono::NaiveDate, u64)>> = reply
        .heatmap
        .iter()
        .map(|day| {
            Some((
                chrono::NaiveDate::parse_from_str(&day.date, "%Y-%m-%d").ok()?,
                day.tokens,
            ))
        })
        .collect()
    else {
        return HeatmapGrid {
            cells: Vec::new(),
            cols: 0,
        };
    };
    let offset = days
        .first()
        .map(|(date, _)| date.weekday().num_days_from_sunday() as usize)
        .unwrap_or(0);
    let cells: Vec<HeatCell> = days
        .drain(..)
        .enumerate()
        .map(|(index, (date, tokens))| {
            let spot = offset + index;
            HeatCell {
                date,
                tokens,
                col: spot / 7,
                row: spot % 7,
            }
        })
        .collect();
    let cols = cells.last().map_or(0, |last| last.col + 1);
    HeatmapGrid { cells, cols }
}

/// The color level 0..=4: the empty shade until a day has tokens, then
/// quarters of the year's peak (ceiling, so any token beats the empty
/// shade and the peak itself reaches the top).
fn heatmap_level(tokens: u64, peak: u64) -> usize {
    if tokens == 0 || peak == 0 {
        0
    } else {
        ((4.0 * tokens as f64 / peak as f64).ceil() as usize).clamp(1, 4)
    }
}

/// The five shades of the scale, empty → peak — and of the Less→More
/// legend, which shows them in the same order.
fn heatmap_cell_color(level: usize, theme: &Theme) -> Hsla {
    match level {
        0 => theme.ink(0.06),
        _ => theme.accent.opacity([0.28, 0.55, 0.8, 1.0][level - 1]),
    }
}

/// A cell's hover copy, spec shape: "X tokens on <full date>".
fn heatmap_tooltip(cell: &HeatCell) -> String {
    format!(
        "{} tokens on {}",
        cell.tokens,
        cell.date.format("%B %-d, %Y")
    )
}

/// Month labels over the grid, GitHub's rule: the column containing a
/// month's 1st carries that month's abbreviation. A day-of-month 1 appears
/// exactly once per month, so the labels are naturally unique; the
/// leading partial column gets one only when it truly contains a 1st.
fn heatmap_month_labels(grid: &HeatmapGrid) -> Vec<(usize, String)> {
    let mut labels = Vec::new();
    for cell in &grid.cells {
        if cell.date.day() == 1 {
            labels.push((cell.col, cell.date.format("%b").to_string()));
        }
    }
    labels
}

/// Which detail table the Breakdown block shows. By model is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum BreakdownTab {
    #[default]
    Models,
    Projects,
}

pub struct UsagePage {
    state: Entity<AppState>,
    days: u32,
    stats: Loadable<UsageStatsReply>,
    task: Option<Task<()>>,
    /// Hidden models' series ids — the legend's click-to-hide state. Page
    /// -local and ephemeral: every reload clears it.
    hidden: BTreeSet<String>,
    /// The Breakdown block's table and its expanded group. Unlike the
    /// legend's visibility these survive a reload: the tab is a view
    /// preference, and flipping back to By model on every refresh would
    /// fight the reader.
    breakdown_tab: BreakdownTab,
    deleted_expanded: bool,
}

impl UsagePage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            days: DEFAULT_RANGE,
            stats: Loadable::Idle,
            task: None,
            hidden: BTreeSet::new(),
            breakdown_tab: BreakdownTab::default(),
            deleted_expanded: false,
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

    /// Switch the Breakdown table.
    fn set_breakdown_tab(&mut self, tab: BreakdownTab, cx: &mut Context<Self>) {
        if self.breakdown_tab != tab {
            self.breakdown_tab = tab;
            cx.notify();
        }
    }

    /// Expand or collapse the Deleted chats group.
    fn toggle_deleted(&mut self, cx: &mut Context<Self>) {
        self.deleted_expanded = !self.deleted_expanded;
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

    /// The Activity block: the year-long token heatmap. Fixed 365-day
    /// window — the range switcher above never touches it, because the
    /// fold reads only the reply's heatmap buckets.
    fn render_heatmap(
        &self,
        reply: &UsageStatsReply,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = Theme::of(cx).clone();
        let grid = fold_heatmap(reply);
        let peak = grid.cells.iter().map(|cell| cell.tokens).max().unwrap_or(0);
        let month_labels = heatmap_month_labels(&grid);
        let mut columns: Vec<Vec<Option<&HeatCell>>> =
            (0..grid.cols).map(|_| vec![None; 7]).collect();
        for cell in &grid.cells {
            columns[cell.col][cell.row] = Some(cell);
        }
        div()
            .id("usage-heatmap")
            .debug_selector(|| "usage-heatmap".into())
            .mt(px(28.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(widgets::section_label(&theme, "Activity"))
            // Month labels, indented past the weekday gutter so each one
            // sits over its week column (the label may overflow the slot).
            .child(
                div()
                    .flex()
                    .flex_row()
                    // Same gap as the grid row below, or every label lands
                    // 6px left of its column.
                    .gap(px(6.0))
                    .child(div().flex_none().w(px(HEAT_GUTTER)))
                    .child(
                        div()
                            .debug_selector(|| "usage-heatmap-months".into())
                            .flex()
                            .flex_row()
                            .gap(px(HEAT_GAP))
                            .children((0..grid.cols).map(|col| {
                                let label = month_labels
                                    .iter()
                                    .find(|(label_col, _)| *label_col == col)
                                    .map(|(_, label)| label.clone());
                                div()
                                    .flex_none()
                                    .w(px(HEAT_CELL))
                                    .text_size(crate::typography::ui_rems(10.0))
                                    .text_color(theme.text_faint)
                                    .children(label.map(SharedString::from))
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(6.0))
                    .child(
                        // Weekday gutter: Mon/Wed/Fri at their rows.
                        div()
                            .flex_none()
                            .w(px(HEAT_GUTTER))
                            .flex()
                            .flex_col()
                            .gap(px(HEAT_GAP))
                            .children(
                                ["", "Mon", "", "Wed", "", "Fri", ""]
                                    .into_iter()
                                    .enumerate()
                                    .map(|(row, label)| {
                                        div()
                                            .flex_none()
                                            .h(px(HEAT_CELL))
                                            .flex()
                                            .items_center()
                                            .text_size(crate::typography::ui_rems(10.0))
                                            .text_color(theme.text_faint)
                                            .when(!label.is_empty(), |slot| {
                                                slot.debug_selector(move || {
                                                    format!("usage-heat-wd-{row}")
                                                })
                                                .child(label)
                                            })
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .id("usage-heatmap-grid")
                            .debug_selector(|| "usage-heatmap-grid".into())
                            .flex()
                            .flex_row()
                            .gap(px(HEAT_GAP))
                            .children(columns.into_iter().map(|rows| {
                                div().flex().flex_col().gap(px(HEAT_GAP)).children(
                                    rows.into_iter().map(|cell| match cell {
                                        Some(cell) => {
                                            self.heat_cell(cell, peak, &theme).into_any_element()
                                        }
                                        None => {
                                            div().flex_none().size(px(HEAT_CELL)).into_any_element()
                                        }
                                    }),
                                )
                            })),
                    ),
            )
            .child(
                // Less→More legend: the five shades in scale order.
                div()
                    .id("usage-heatmap-legend")
                    .debug_selector(|| "usage-heatmap-legend".into())
                    .flex()
                    .flex_row()
                    .justify_end()
                    .items_center()
                    .gap(px(4.0))
                    .text_size(crate::typography::ui_rems(10.0))
                    .text_color(theme.text_faint)
                    .child("Less")
                    .children((0..=4).map(|level| {
                        div()
                            .flex_none()
                            .size(px(HEAT_CELL))
                            .rounded(px(2.0))
                            .bg(heatmap_cell_color(level, &theme))
                    }))
                    .child("More"),
            )
    }

    /// One day cell: the 11px hover target carrying the day readout.
    fn heat_cell(&self, cell: &HeatCell, peak: u64, theme: &Theme) -> gpui::Stateful<gpui::Div> {
        let level = heatmap_level(cell.tokens, peak);
        let tooltip = SharedString::from(heatmap_tooltip(cell));
        div()
            .id(SharedString::from(format!(
                "usage-heat-{}-{}",
                cell.col, cell.row
            )))
            .debug_selector(move || format!("usage-heat-{}-{}", cell.col, cell.row))
            .flex_none()
            .size(px(HEAT_CELL))
            .rounded(px(2.0))
            .bg(heatmap_cell_color(level, theme))
            .cursor_default()
            .tooltip(move |_, cx| {
                cx.new(|_| crate::image_viewer::ViewerTooltip(tooltip.clone()))
                    .into()
            })
            .tooltip_show_delay(DAY_TOOLTIP_DELAY)
    }

    /// The Breakdown block: two switchable detail tables over the reply's
    /// own by-model / by-project rows — the engine hands them total-
    /// descending, the table prints them in that order. The four numeric
    /// columns are exact counts (Total is the four token fields summed;
    /// there is no cache-write column — the tiles above carry it).
    fn render_breakdown(
        &self,
        reply: &UsageStatsReply,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = Theme::of(cx).clone();
        div()
            .id("usage-breakdown")
            .debug_selector(|| "usage-breakdown".into())
            .mt(px(28.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(
                        widgets::section_label(&theme, "Breakdown")
                            .flex_1()
                            .min_w_0(),
                    )
                    .child(self.breakdown_chip(BreakdownTab::Models, &theme, cx))
                    .child(self.breakdown_chip(BreakdownTab::Projects, &theme, cx)),
            )
            .child(self.breakdown_header(&theme))
            .children(match self.breakdown_tab {
                BreakdownTab::Models => reply
                    .by_model
                    .iter()
                    .enumerate()
                    .map(|(index, row)| {
                        self.breakdown_row(
                            &format!("{}/{}", row.provider, row.model),
                            (row.input, row.output, row.cache_read, row.total),
                            format!("usage-bd-model-row-{index}"),
                            false,
                            &theme,
                        )
                    })
                    .collect::<Vec<_>>(),
                BreakdownTab::Projects => reply
                    .by_project
                    .iter()
                    .enumerate()
                    .flat_map(|(index, group)| {
                        let mut rows = vec![match &group.path {
                            Some(path) => self.breakdown_row(
                                path,
                                (group.input, group.output, group.cache_read, group.total),
                                format!("usage-bd-project-row-{index}"),
                                false,
                                &theme,
                            ),
                            None => self.deleted_group_row(group, &theme, cx),
                        }];
                        if group.path.is_none() && self.deleted_expanded {
                            rows.extend(group.chats.iter().enumerate().map(|(ix, chat)| {
                                self.breakdown_row(
                                    &chat.chat_id,
                                    (chat.input, chat.output, chat.cache_read, chat.total),
                                    format!("usage-bd-deleted-sub-{ix}"),
                                    true,
                                    &theme,
                                )
                            }));
                        }
                        rows
                    })
                    .collect::<Vec<_>>(),
            })
    }

    /// One of the two section tabs — the range chips' shape.
    fn breakdown_chip(
        &self,
        tab: BreakdownTab,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let (tag, label) = match tab {
            BreakdownTab::Models => ("model", "By model"),
            BreakdownTab::Projects => ("project", "By project"),
        };
        let active = self.breakdown_tab == tab;
        let mut chip = div()
            .id(SharedString::from(format!("usage-bd-tab-{tag}")))
            .debug_selector(move || format!("usage-bd-tab-{tag}"))
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
                .on_click(cx.listener(move |page, _, _, cx| page.set_breakdown_tab(tab, cx)));
        }
        chip.child(label).into_any_element()
    }

    /// The table's header: the identity column is named by the active tab,
    /// the four numeric columns are fixed.
    fn breakdown_header(&self, theme: &Theme) -> gpui::Stateful<gpui::Div> {
        let name = match self.breakdown_tab {
            BreakdownTab::Models => "Model",
            BreakdownTab::Projects => "Project",
        };
        div()
            .id("usage-bd-head")
            .debug_selector(|| "usage-bd-head".into())
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .py(px(6.0))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_muted)
                    .child(name),
            )
            .child(
                div()
                    .debug_selector(|| "usage-bd-head-input".into())
                    .flex_none()
                    .flex()
                    .flex_row()
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_muted)
                    .children(["Input", "Output", "Cache read", "Total"].map(|label| {
                        div()
                            .flex_none()
                            .w(px(BREAKDOWN_COL))
                            .text_right()
                            .child(label)
                    })),
            )
    }

    /// One table row: the identity cell, then the four exact counts.
    /// `indent` drops the identity cell toward the Deleted chats'
    /// sub-rows.
    fn breakdown_row(
        &self,
        name: &str,
        numbers: (u64, u64, u64, u64),
        selector: String,
        indent: bool,
        theme: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .py(px(6.0))
            .when(indent, |row| row.pl(px(20.0)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(name.to_string())),
            )
            .child(self.breakdown_numbers(numbers, theme))
    }

    /// The four exact-count cells — the same fixed widths every table row
    /// and the header share.
    fn breakdown_numbers(
        &self,
        (input, output, cache_read, total): (u64, u64, u64, u64),
        theme: &Theme,
    ) -> gpui::Div {
        div()
            .flex_none()
            .flex()
            .flex_row()
            .text_size(crate::typography::ui_rems(11.5))
            .text_color(theme.text)
            .children(
                [
                    (input, false),
                    (output, false),
                    (cache_read, false),
                    (total, true),
                ]
                .map(|(tokens, is_total)| {
                    let mut cell = div()
                        .flex_none()
                        .w(px(BREAKDOWN_COL))
                        .text_right()
                        .child(SharedString::from(tokens.to_string()));
                    if is_total {
                        cell = cell.font_weight(gpui::FontWeight::MEDIUM);
                    }
                    cell
                }),
            )
    }

    /// The Deleted chats group row: the expandable catch-all for records
    /// whose chat resolves to no working directory.
    fn deleted_group_row(
        &self,
        group: &holt_proto::UsageProjectGroup,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let caret = if self.deleted_expanded {
            icons::ALT_ARROW_DOWN
        } else {
            icons::ALT_ARROW_RIGHT
        };
        div()
            .id("usage-bd-deleted-row")
            .debug_selector(|| "usage-bd-deleted-row".into())
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .py(px(6.0))
            .cursor_pointer()
            .hover(|state| state.bg(crate::theme::wash(0.04)))
            .on_click(cx.listener(|page, _, _, cx| page.toggle_deleted(cx)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        icons::icon(caret)
                            .size(px(12.0))
                            .text_color(theme.text_faint),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child("Deleted chats"),
                    ),
            )
            .child(self.breakdown_numbers(
                (group.input, group.output, group.cache_read, group.total),
                theme,
            ))
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
                        .child(self.render_heatmap(reply, cx))
                        .child(self.render_breakdown(reply, cx))
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

    // -------------------------------------------------------------------
    // Ticket 04 — the Activity heatmap
    // -------------------------------------------------------------------

    fn day_json(date: &str, tokens: u64) -> serde_json::Value {
        serde_json::json!({ "date": date, "tokens": tokens })
    }

    /// A real 365-day heatmap ending `today` — the engine's fixed
    /// window — with three peaks (500, 1000, 200) so the scale has shape.
    fn heatmap_reply_ending(today: chrono::NaiveDate) -> Scripted {
        let tokens = |i: usize| match i {
            100 => 500,
            200 => 1000,
            364 => 200,
            _ => 0,
        };
        let heatmap: Vec<serde_json::Value> = (0..365)
            .map(|i| {
                day_json(
                    &(today - chrono::Duration::days(364 - i as i64))
                        .format("%Y-%m-%d")
                        .to_string(),
                    tokens(i),
                )
            })
            .collect();
        Scripted::Ok(serde_json::json!({
            "chatCount": 1,
            "days": 30,
            "totals": {
                "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0,
                "cacheHit": null, "activeDays": 3,
            },
            "models": [], "byModel": [], "byProject": [],
            "heatmap": heatmap,
        }))
    }

    /// A two-week window ending Wednesday 2026-09-16: it starts Thursday
    /// 2026-09-03, so the Sunday-first grid needs a 4-cell leading offset.
    fn two_week_reply() -> serde_json::Value {
        let end = chrono::NaiveDate::from_ymd_opt(2026, 9, 16).unwrap();
        let heatmap: Vec<serde_json::Value> = (0..14)
            .map(|i| {
                day_json(
                    &(end - chrono::Duration::days(13 - i as i64))
                        .format("%Y-%m-%d")
                        .to_string(),
                    (i * 10) as u64,
                )
            })
            .collect();
        serde_json::json!({
            "chatCount": 1, "days": 30,
            "totals": { "input": 0, "output": 0, "cacheRead": 0,
                        "cacheWrite": 0, "cacheHit": null, "activeDays": 14 },
            "models": [], "byModel": [], "byProject": [],
            "heatmap": heatmap,
        })
    }

    fn cell(grid: &HeatmapGrid, col: usize, row: usize) -> &HeatCell {
        grid.cells
            .iter()
            .find(|cell| cell.col == col && cell.row == row)
            .unwrap_or_else(|| panic!("no cell at {col},{row}"))
    }

    #[test]
    fn the_heatmap_wraps_onto_sunday_first_weeks() {
        let grid = fold_heatmap(&decode_reply(two_week_reply()));

        // Thursday start: 4 leading offsets, 14 days, three week columns.
        assert_eq!(grid.cols, 3);
        let first = cell(&grid, 0, 4);
        assert_eq!(first.date.to_string(), "2026-09-03");
        // The days flow down each column: Friday 09-04 lands under it.
        assert_eq!(cell(&grid, 0, 5).date.to_string(), "2026-09-04");
        // The last day (Wednesday) sits at column 2, row 3 — aligned to
        // its own weekday, not packed to the top.
        let last = cell(&grid, 2, 3);
        assert_eq!(last.date.to_string(), "2026-09-16");
    }

    #[test]
    fn heatmap_levels_quarter_the_peak_and_empty_days_stay_lowest() {
        let peak = 100;
        assert_eq!(heatmap_level(0, peak), 0);
        assert_eq!(heatmap_level(1, peak), 1, "any token beats the empty shade");
        assert_eq!(heatmap_level(25, peak), 1);
        assert_eq!(heatmap_level(26, peak), 2);
        assert_eq!(heatmap_level(75, peak), 3);
        assert_eq!(
            heatmap_level(100, peak),
            4,
            "the peak reaches the top level"
        );
        // A year with no usage at all has no scale to climb.
        assert_eq!(heatmap_level(10, 0), 0);
    }

    #[test]
    fn the_heatmap_hover_names_the_full_date() {
        let grid = fold_heatmap(&decode_reply(two_week_reply()));
        let wednesday = cell(&grid, 2, 3);
        assert_eq!(
            heatmap_tooltip(wednesday),
            "130 tokens on September 16, 2026"
        );
    }

    #[test]
    fn month_labels_mark_where_a_month_first_appears() {
        // A 33-day window spanning one month boundary: Aug 15 (Sat) 2026
        // to Sep 16. GitHub's rule labels the column containing the 1st:
        // Sep 1 lands in column 3 (6 leading offsets + 17 days), August
        // itself never gets one (its 1st is before the window starts).
        let end = chrono::NaiveDate::from_ymd_opt(2026, 9, 16).unwrap();
        let heatmap: Vec<serde_json::Value> = (0..33)
            .map(|i| {
                day_json(
                    &(end - chrono::Duration::days(32 - i as i64))
                        .format("%Y-%m-%d")
                        .to_string(),
                    0,
                )
            })
            .collect();
        let reply = serde_json::json!({
            "chatCount": 1, "days": 30,
            "totals": { "input": 0, "output": 0, "cacheRead": 0,
                        "cacheWrite": 0, "cacheHit": null, "activeDays": 0 },
            "models": [], "byModel": [], "byProject": [],
            "heatmap": heatmap,
        });
        let grid = fold_heatmap(&decode_reply(reply));
        let labels = heatmap_month_labels(&grid);
        assert_eq!(labels, vec![(3, "Sep".to_string())], "{labels:?}");
    }

    #[gpui::test]
    fn the_heatmap_grid_aligns_to_today_and_stays_hittable(cx: &mut gpui::TestAppContext) {
        // The clock is read once: fixture and assertions share one today,
        // so a midnight rollover mid-test cannot desync them.
        let today = chrono::Local::now().date_naive();
        let mut harness = harness(cx, vec![], heatmap_reply_ending(today));

        assert!(harness.present("usage-heatmap"));
        assert!(harness.present("usage-heatmap-grid"));
        assert!(harness.present("usage-heatmap-legend"));

        // Today is the last bucket: its column is the 53rd (index 52) and
        // its row is today's own weekday, Sunday-first.
        let today = chrono::Local::now().date_naive();
        let offset = today.weekday().num_days_from_sunday() as usize;
        let spot = offset + 364;
        let (col, row) = (spot / 7, spot % 7);
        let today_cell = harness.bounds(Box::leak(
            format!("usage-heat-{col}-{row}").into_boxed_str(),
        ));
        assert_eq!(
            (today_cell.size.width, today_cell.size.height),
            (px(HEAT_CELL), px(HEAT_CELL)),
            "the small hover target is laid out at cell size"
        );

        // The grid hugs today: the cell after today in its own column
        // does not exist, and the leading column starts at today's
        // weekday offset.
        if row < 6 {
            let next = Box::leak(format!("usage-heat-{col}-{}", row + 1).into_boxed_str());
            assert!(!harness.present(next), "no cell exists past today");
        }
        assert_eq!(
            harness.present("usage-heat-0-0"),
            offset == 0,
            "the leading column starts at today's weekday offset"
        );

        // Weekday gutter: exactly Mon/Wed/Fri carry labels.
        assert!(harness.present("usage-heat-wd-1"));
        assert!(harness.present("usage-heat-wd-3"));
        assert!(harness.present("usage-heat-wd-5"));
        assert!(!harness.present("usage-heat-wd-0"));

        // The heatmap is range-independent: the reply's buckets are the
        // same fixed year whatever the switcher does.
        harness.click("usage-range-7");
        harness.pump();
        assert!(
            harness.present(Box::leak(
                format!("usage-heat-{col}-{row}").into_boxed_str()
            )),
            "the heatmap survives a range switch unchanged"
        );
    }

    // -------------------------------------------------------------------
    // Ticket 05 — the Breakdown tables
    // -------------------------------------------------------------------

    /// Two model rows, three project groups (same-basename directories,
    /// plus the Deleted chats catch-all whose per-chat subtotals sum to
    /// the group), and one path long enough to test truncation.
    fn breakdown_reply_json() -> serde_json::Value {
        let long_path = format!("/very/deep/{}", "nested/".repeat(24));
        serde_json::json!({
            "chatCount": 3,
            "days": 30,
            "totals": {
                "input": 480, "output": 140, "cacheRead": 90, "cacheWrite": 0,
                "cacheHit": 0.158, "activeDays": 5,
            },
            "models": [], "heatmap": [],
            "byModel": [
                {"provider": "openai", "model": "gpt-5.4",
                 "input": 200, "output": 60, "cacheRead": 40, "total": 300},
                {"provider": "anthropic", "model": "claude-opus",
                 "input": 70, "output": 20, "cacheRead": 10, "total": 100},
                {"provider": "openai", "model": "gpt-5-mini",
                 "input": 30, "output": 15, "cacheRead": 5, "total": 50},
            ],
            "byProject": [
                {"path": "/work/api",
                 "input": 170, "output": 50, "cacheRead": 30, "total": 250, "chats": []},
                {"path": "/other/api",
                 "input": 60, "output": 20, "cacheRead": 10, "total": 90, "chats": []},
                {"path": serde_json::Value::Null,
                 "input": 40, "output": 10, "cacheRead": 10, "total": 60, "chats": [
                     {"chatId": "chat-gone-1",
                      "input": 30, "output": 6, "cacheRead": 4, "total": 40},
                     {"chatId": "chat-gone-2",
                      "input": 10, "output": 4, "cacheRead": 6, "total": 20},
                 ]},
                {"path": long_path,
                 "input": 1, "output": 2, "cacheRead": 0, "total": 3, "chats": []},
            ],
        })
    }

    fn breakdown_reply() -> Scripted {
        Scripted::Ok(breakdown_reply_json())
    }

    #[test]
    fn deleted_chat_subtotals_sum_to_the_group_row() {
        let reply = decode_reply(breakdown_reply_json());
        let deleted = reply
            .by_project
            .iter()
            .find(|group| group.path.is_none())
            .expect("the deleted-chats group");
        // The engine's four-field totals: the expandable row's number is
        // exactly the sum of what unfolds beneath it.
        let sum: u64 = deleted.chats.iter().map(|chat| chat.total).sum();
        assert_eq!(sum, deleted.total);
        assert_eq!(deleted.chats.len(), 2);
        // The input column alone reconciles too — the same arithmetic the
        // table prints column-wise.
        let input_sum: u64 = deleted.chats.iter().map(|chat| chat.input).sum();
        assert_eq!(input_sum, deleted.input);
    }

    #[gpui::test]
    fn the_breakdown_opens_on_by_model_in_engine_order(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, vec![], breakdown_reply());

        // Default tab: By model, rows in the engine's total-descending
        // order (300 then 100), no project rows.
        assert!(harness.present("usage-bd-model-row-0"));
        assert!(harness.present("usage-bd-model-row-1"));
        let first = harness.bounds("usage-bd-model-row-0");
        let second = harness.bounds("usage-bd-model-row-1");
        assert!(first.origin.y < second.origin.y, "largest total first");
        // Many models just repeat the row shape: the third sits below the
        // second, nothing reflows.
        let third = harness.bounds("usage-bd-model-row-2");
        assert!(second.origin.y < third.origin.y);
        assert_eq!(third.size.height, first.size.height);
        assert!(!harness.present("usage-bd-project-row-0"));

        // Switching tabs swaps the table.
        harness.click("usage-bd-tab-project");
        harness.pump();
        assert!(harness.present("usage-bd-project-row-0"));
        assert!(harness.present("usage-bd-project-row-1"));
        assert!(harness.present("usage-bd-deleted-row"));
        assert!(!harness.present("usage-bd-model-row-0"), "model rows gone");
    }

    #[gpui::test]
    fn the_deleted_chats_group_expands_and_collapses(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, vec![], breakdown_reply());
        harness.click("usage-bd-tab-project");
        harness.pump();

        // Collapsed by default: the group row is there, its chats are not.
        assert!(harness.present("usage-bd-deleted-row"));
        assert!(!harness.present("usage-bd-deleted-sub-0"));

        // Expanding reveals the per-chat rows, keyed by chat id.
        harness.click("usage-bd-deleted-row");
        harness.pump();
        assert!(harness.present("usage-bd-deleted-sub-0"));
        assert!(harness.present("usage-bd-deleted-sub-1"));
        let group = harness.bounds("usage-bd-deleted-row");
        let sub = harness.bounds("usage-bd-deleted-sub-0");
        assert!(
            sub.origin.y > group.origin.y,
            "chats unfold under the group"
        );

        // Collapsing hides them again — and the state survives a reload.
        harness.click("usage-bd-deleted-row");
        harness.pump();
        assert!(!harness.present("usage-bd-deleted-sub-0"));
        harness.click("usage-bd-deleted-row");
        harness.pump();
        harness.click("usage-range-7");
        harness.pump();
        assert!(
            harness.present("usage-bd-deleted-sub-0"),
            "the expansion survives a reload — it is a view preference"
        );
    }

    #[gpui::test]
    fn a_long_path_truncates_instead_of_breaking_the_table(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, vec![], breakdown_reply());
        harness.click("usage-bd-tab-project");
        harness.pump();

        // The 300-character path stays on one line at the table's own
        // width: the identity cell truncates, the numeric columns keep
        // their alignment, and the page never grows a horizontal scroll.
        let head = harness.bounds("usage-bd-head");
        let normal = harness.bounds("usage-bd-project-row-0");
        let long_row = harness.bounds("usage-bd-project-row-3");
        assert_eq!(head.size.width, long_row.size.width, "same table width");
        assert_eq!(
            long_row.size.height, normal.size.height,
            "a long path truncates, never wraps"
        );
        // The two directory rows sit above the long-path one,
        // total-descending; the deleted group between them carries its
        // own selector.
        let first = harness.bounds("usage-bd-project-row-0");
        let second = harness.bounds("usage-bd-project-row-1");
        assert!(first.origin.y < second.origin.y);
        assert!(second.origin.y < long_row.origin.y);
    }
}
