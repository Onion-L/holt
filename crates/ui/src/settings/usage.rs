//! The Usage settings page (usage-overview spec, tickets 02+): the
//! device-level usage aggregate from one unary `UsageStats` call, read
//! fresh on every entry. The page reads top-down: the header's count
//! line and range switcher; the Total tokens hero with the model
//! toggle chips beside it; the Daily usage stacked bar chart; the six
//! metric tiles; the year-long Activity heatmap; and the By model /
//! By project Breakdown donut, which folds its list to the top four
//! rows plus an "Others" row — the row itself, not a switch beside it,
//! opens the tail it groups.
//! Everything on the page is read-only: the only controls that touch
//! the engine reload the same aggregate, and the legibility state —
//! the chart legend's visibility toggles and fold, the Breakdown's
//! fold — is ephemeral page state that a reload resets. Reloads never
//! blank the page: a range switch or refresh dims the stale view under
//! a header spinner until the fresh reply lands; only a first load
//! drops to skeletons.
//!
//! The chart is gpui self-drawn over the shared day axis: one bar per
//! day, the visible models stacked bottom-to-top in rank order with a
//! rounded cap on the stack's topmost segment, over one hairline
//! gridline per Y rung (the git graph's palette, so legend chips and
//! bar segments share one color per rank). Hovering a day washes its
//! column and pins the day readout inside the plot over that day's
//! own slot — instant, and it never escapes over the gutter or the
//! content below. The heatmap is the same readout idea over a DOM
//! grid of cells: every day is its own hover target.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Datelike;
use gpui::{
    AnyElement, Bounds, Context, Entity, Hsla, IntoElement, PathBuilder, Render, SharedString,
    Task, Window, canvas, div, point, prelude::*, px, size,
};
use holt_proto::UsageStatsReply;
use holt_rpc::methods;

use crate::{
    chat_usage::percent,
    icons, loaders,
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
/// Tall enough that a stacked day reads as a column, not a splinter — the
/// reference chart's proportions.
const CHART_HEIGHT: f32 = 260.0;
/// Day tooltips appear faster than the 350ms hover cards: scanning across
/// thirty columns should read each day without slowing to a crawl.
const DAY_TOOLTIP_DELAY: Duration = Duration::from_millis(150);
/// The pinned day readout's width: air for a model id without reading as
/// a panel pasted over the chart.
const READOUT_WIDTH: f32 = 210.0;

/// A `YYYY-MM-DD` bucket key as the axis prints it: "Sep 16". An
/// unparsable date (never the engine's shape) prints as-is.
fn short_date(date: &str) -> String {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|parsed| parsed.format("%b %-d").to_string())
        .unwrap_or_else(|_| date.to_string())
}

/// The header's count line, verbatim spec shape: "N chats · last N days".
fn header_count_text(chat_count: u64, days: u32) -> String {
    format!("{chat_count} chats · last {days} days")
}

/// The hero Total's caption: the exact count, comma-separated, so the
/// headline's precision is on the page (the compact value never shows
/// past three significant figures) instead of a hover away.
fn exact_tokens(tokens: u64) -> String {
    let digits = tokens.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// The Y scale's rung step: the smallest of 1/2/2.5/5 × 10^k that clears
/// a quarter of the peak — the reference chart's clean round rung values
/// (0 · 50M · 100M · 150M · 200M), never a raw "136.1M" mid-rung.
fn nice_step(peak: u64) -> f64 {
    let target = peak as f64 / 4.0;
    let magnitude = 10f64.powf(target.max(1.0).log10().floor());
    for mantissa in [1.0, 2.0, 2.5, 5.0, 10.0] {
        let step = mantissa * magnitude;
        // 2.5 only while it stays a whole token count — a sub-token rung
        // would print a fraction.
        if step >= target && (mantissa != 2.5 || magnitude >= 10.0) {
            return step;
        }
    }
    magnitude * 10.0
}

/// The Y rungs bottom-up: zero, the step, … a top rung at or above the
/// peak. The last value is the chart's ceiling — bars and gridlines both
/// divide by it, so the top rung is always the highest gridline.
fn scale_rungs(peak: u64) -> Vec<u64> {
    let step = nice_step(peak);
    let count = ((peak as f64 / step).ceil() as u64).max(1);
    (0..=count)
        .map(|rung| (rung as f64 * step).round() as u64)
        .collect()
}

/// X labels print under their own bar slot; a label needs a few character
/// widths of slot, so dense ranges label every Nth day — anchored to the
/// range's last day, so today never goes unlabeled.
fn label_stride(days: usize) -> usize {
    if days <= 14 {
        1
    } else {
        (days as f32 / 12.0).ceil() as usize
    }
}

/// The title row: the chart icon, the page name at headline size, and the
/// description riding the same line — the reference design's masthead.
fn title_row(theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .child(
            icons::icon(icons::CHART_COLUMN)
                .size(px(17.0))
                .text_color(theme.text_muted),
        )
        .child(
            div()
                .text_size(crate::typography::ui_rems(20.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(PAGE_TITLE),
        )
        .child(
            div()
                .text_size(crate::typography::ui_rems(13.0))
                .text_color(theme.text_muted)
                .child(PAGE_DESCRIPTION),
        )
}

/// A bordered pill holding switch segments — the range switcher and the
/// Breakdown tabs share this one shape.
fn segmented_control(children: Vec<AnyElement>, theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .h(px(26.0))
        .px(px(3.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
        .rounded(px(7.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.ink(0.03))
        .children(children)
}

/// One segment inside a [`segmented_control`]: the active option carries
/// a wash fill and full-weight text, the rest are quiet switches. The
/// caller attaches `.on_click` to the inactive ones.
fn segment(
    id: SharedString,
    selector: String,
    label: SharedString,
    active: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let hover_text = theme.text;
    let mut chip = div()
        .id(id)
        .debug_selector(move || selector)
        .flex_none()
        .h(px(20.0))
        .px(px(8.0))
        .flex()
        .items_center()
        .rounded(px(5.0))
        .text_size(crate::typography::ui_rems(11.5))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(if active { theme.text } else { theme.text_muted });
    if active {
        chip = chip.bg(theme.ink(0.12));
    } else {
        chip = chip
            .cursor_pointer()
            .hover(move |state| state.text_color(hover_text));
    }
    chip.child(label)
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

    /// One day's visible total — the number the readout's Total row and
    /// the stack height both print, so the readout always matches the
    /// picture.
    fn day_total(&self, hidden: &BTreeSet<String>, day: usize) -> u64 {
        self.visible(hidden)
            .iter()
            .map(|(_, series)| series.per_day.get(day).copied().unwrap_or(0))
            .sum()
    }

    /// The Y scale's input: the tallest visible stacked day total.
    /// Floored at 1 so an all-zero window still divides.
    fn peak_day(&self, hidden: &BTreeSet<String>) -> u64 {
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
/// danger, muted), at full saturation — bar fills read at a different
/// weight than that chart's thin lanes, so history's desaturation stays
/// there. `busy` is left out: it is accent-derived and collapses into the
/// first slot in accent-themed builds. The legend checkbox and the bar
/// segments take the same color, which is what ties them together; past
/// the palette the hues cycle.
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

/// The legend's fold threshold: past this many models the rest hide
/// behind a "+N more" chip — about two rows of chips, so the hero row
/// keeps its height no matter how many models the range saw.
const LEGEND_VISIBLE: usize = 6;

/// Hide or restore one model's bars. The last visible model refuses to
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

/// The heatmap cell geometry: 13px squares, the columns spreading to
/// fill the content width (justify-between absorbs the slack into the
/// gaps, so the grid hugs both page edges like the reference).
const HEAT_CELL: f32 = 13.0;
const HEAT_GAP: f32 = 3.0;
/// The weekday-label gutter left of the grid; the month-label row indents
/// by the same amount so labels sit over their columns.
const HEAT_GUTTER: f32 = 30.0;
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

/// A cell's hover copy: compact count + short date, the chart readout's
/// voice — a raw digit soup ("114891") is exactly what hover should
/// spare the reader.
fn heatmap_tooltip(cell: &HeatCell) -> String {
    format!(
        "{} tokens on {}",
        compact_tokens(cell.tokens),
        cell.date.format("%b %-d, %Y")
    )
}

/// Month labels over the grid, GitHub's rule: a column is labeled when
/// the month of its days changes, reading each column top-down — so the
/// leading partial month labels column 0, and a month starting mid-week
/// labels the column containing its 1st.
fn heatmap_month_labels(grid: &HeatmapGrid) -> Vec<(usize, String)> {
    let mut labels = Vec::new();
    let mut current: Option<(i32, u32)> = None;
    for col in 0..grid.cols {
        for cell in grid.cells.iter().filter(|cell| cell.col == col) {
            let month = (cell.date.year(), cell.date.month());
            if Some(month) != current {
                labels.push((col, cell.date.format("%b").to_string()));
                current = Some(month);
                break;
            }
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

/// The Breakdown donut's geometry: a 224px ring drawn thick like the
/// reference (thickness ≈ 0.22 of the size), with a 2px margin between
/// the ring's outer edge and the canvas bounds so anti-aliasing never
/// clips. Segments run clockwise from 12 o'clock in engine order,
/// separated by a constant-width gap (in px, so each seam's edges stay
/// parallel instead of converging on the hole); a hairline minimum
/// keeps tiny shares visible, like the reference chart's slivers.
const DONUT_SIZE: f32 = 224.0;
const DONUT_THICKNESS: f32 = 50.0;
const DONUT_MARGIN: f32 = 2.0;
const DONUT_GAP: f32 = 4.0;
const DONUT_MIN_SLIVER: f32 = 0.02;

/// One legend/donut entry of the Breakdown card: the engine's
/// total-descending order, colors from the chart's rank palette.
#[derive(Debug, Clone)]
struct BreakdownEntry {
    color: Hsla,
    name: String,
    tokens: u64,
}

/// The Breakdown's fold threshold: past this many entries the tail
/// collapses into one "Others" row, so a long model list never stretches
/// the card.
const BREAKDOWN_VISIBLE: usize = 4;
/// The folded tail's fixed name — one synthetic entry standing in for
/// every row past [`BREAKDOWN_VISIBLE`].
const BREAKDOWN_OTHERS: &str = "Others";

/// How the card is drawn from the entries the active tab folded: the
/// donut's slices and the tail the Others slice groups.
#[derive(Debug, Clone, Default)]
struct BreakdownFold {
    /// What the donut draws and the legend lists: the top
    /// [`BREAKDOWN_VISIBLE`] entries, plus one Others entry standing in
    /// for the tail when there is one.
    slices: Vec<BreakdownEntry>,
    /// The entries the Others slice groups, in engine order. Empty when
    /// the reply is short enough to print whole — then the last slice is
    /// a real entry and no row folds anything.
    tail: Vec<BreakdownEntry>,
}

impl BreakdownFold {
    /// The Others slice's index, when the fold has a tail to group.
    fn others(&self) -> Option<usize> {
        (!self.tail.is_empty()).then(|| self.slices.len() - 1)
    }
}

/// How one legend row reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BreakdownRowKind {
    /// A slice of the donut, in its own color.
    Slice,
    /// The fold's disclosure: the Others slice's row, which folds and
    /// unfolds the tail under itself.
    Others,
    /// A tail entry itemized under the Others row: indented, in the
    /// group's own gray.
    Child,
}

impl BreakdownRowKind {
    fn is_child(self) -> bool {
        matches!(self, Self::Child)
    }
}

/// One legend row's place in the card: the id it is addressed by, the
/// donut slice it speaks for, and the shape it reads in.
struct BreakdownRow {
    id: String,
    slice: usize,
    kind: BreakdownRowKind,
}

/// Fold the active tab's rows once per render. Zero-total rows drop out —
/// a slice and a legend row for nothing would both read as a glitch.
/// Model entries reuse the chart's per-rank colors (looked up through the
/// summary's series ranks) so a model reads as the same hue in both
/// charts; projects have no chart counterpart and take the palette in
/// order.
fn fold_breakdown(
    reply: &UsageStatsReply,
    summary: &Summary,
    tab: BreakdownTab,
    theme: &Theme,
) -> Vec<BreakdownEntry> {
    match tab {
        BreakdownTab::Models => reply
            .by_model
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                if row.total == 0 {
                    return None;
                }
                let name = format!("{}/{}", row.provider, row.model);
                let rank = summary
                    .series
                    .iter()
                    .position(|series| series.id == name)
                    .unwrap_or(index);
                Some(BreakdownEntry {
                    color: series_color(rank, theme),
                    name,
                    tokens: row.total,
                })
            })
            .collect(),
        BreakdownTab::Projects => reply
            .by_project
            .iter()
            .enumerate()
            .filter_map(|(index, group)| {
                if group.total == 0 {
                    return None;
                }
                Some(BreakdownEntry {
                    color: series_color(index, theme),
                    name: group
                        .path
                        .clone()
                        .unwrap_or_else(|| "Deleted chats".to_string()),
                    tokens: group.total,
                })
            })
            .collect(),
    }
}

/// The rows the card draws: the top [`BREAKDOWN_VISIBLE`] entries as-is,
/// with the rest summed into one muted "Others" slice and kept beside it
/// as the tail that slice groups. The donut and the legend share the
/// slices, so a hovered row index means the same slice in both, and the
/// ring's shares stay true — the fold groups slices, it never drops them,
/// and it never changes the ring's shape: opening the fold itemizes the
/// tail in the legend, the donut draws the same slices either way.
fn breakdown_fold(entries: &[BreakdownEntry], theme: &Theme) -> BreakdownFold {
    if entries.len() <= BREAKDOWN_VISIBLE {
        return BreakdownFold {
            slices: entries.to_vec(),
            tail: Vec::new(),
        };
    }
    let mut slices = entries[..BREAKDOWN_VISIBLE].to_vec();
    // The tail keeps the group's own color: an itemized row is part of the
    // Others slice, and a rank hue there would point at a slice that
    // doesn't exist.
    let color = theme.text_faint;
    let tail: Vec<BreakdownEntry> = entries[BREAKDOWN_VISIBLE..]
        .iter()
        .cloned()
        .map(|mut entry| {
            entry.color = color;
            entry
        })
        .collect();
    slices.push(BreakdownEntry {
        color,
        name: BREAKDOWN_OTHERS.to_string(),
        tokens: tail.iter().map(|entry| entry.tokens).sum(),
    });
    BreakdownFold { slices, tail }
}

/// The legend's integer share, rounded like the reference chart — tiny
/// shares read as "0%", never a decimal.
fn breakdown_percent(tokens: u64, grand: u64) -> u64 {
    if grand == 0 {
        0
    } else {
        ((tokens as f64 / grand as f64) * 100.0).round() as u64
    }
}

pub struct UsagePage {
    state: Entity<AppState>,
    days: u32,
    stats: Loadable<UsageStatsReply>,
    task: Option<Task<()>>,
    /// Hidden models' series ids — the legend's click-to-hide state. Page
    /// -local and ephemeral: every reload clears it.
    hidden: BTreeSet<String>,
    /// The legend's "+N more" expansion. Ephemeral like the visibility
    /// state: a reload re-folds the legend.
    legend_expanded: bool,
    /// The Breakdown block's tab. Unlike the legend's visibility this
    /// survives a reload: the tab is a view preference, and flipping back
    /// to By model on every refresh would fight the reader.
    breakdown_tab: BreakdownTab,
    /// The Breakdown legend's fold: closed prints the top
    /// [`BREAKDOWN_VISIBLE`] rows plus the Others row, open itemizes the
    /// tail under that row. Ephemeral like the chart legend's fold — a
    /// reload re-folds it.
    breakdown_expanded: bool,
    /// The hovered Breakdown legend row — dims the donut's other slices
    /// and swaps the center readout to that entry. Cleared on tab switch
    /// (the index means a different entry there).
    hover_slice: Option<usize>,
    /// The hovered day column — pins the readout card over its slot and
    /// washes the column. Ephemeral; a reload can leave it past the axis,
    /// so render filters it against the day count.
    hover_day: Option<usize>,
    /// True while a reload is in flight over existing data: the stale
    /// reply stays on screen dimmed with a spinner in the header, instead
    /// of the page blanking to skeletons.
    reloading: bool,
    /// The range the on-screen reply actually answers — the count line
    /// names this, not the pending selection, so the text never claims a
    /// window the numbers don't cover.
    loaded_days: u32,
}

impl UsagePage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            days: DEFAULT_RANGE,
            stats: Loadable::Idle,
            task: None,
            hidden: BTreeSet::new(),
            legend_expanded: false,
            breakdown_tab: BreakdownTab::default(),
            breakdown_expanded: false,
            hover_slice: None,
            hover_day: None,
            reloading: false,
            loaded_days: DEFAULT_RANGE,
        };
        page.load(cx);
        page
    }

    /// One fresh `UsageStats` call for the current range. Every reload —
    /// entry, range switch, refresh — runs through here and drops the
    /// previous call. A reload over existing data keeps that data on
    /// screen dimmed until the fresh reply lands — only a first load (or
    /// a retry off an error) drops to skeletons. Series visibility resets
    /// when the reply lands: the new reply is a new view, not a filter
    /// over the old one.
    fn load(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.stats, Loadable::Ready(_)) {
            self.stats = Loadable::Loading;
        }
        self.reloading = true;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.stats = Loadable::Error("Engine not connected".into());
            self.reloading = false;
            return;
        };
        let days = self.days;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::USAGE_STATS, serde_json::json!({ "days": days }))
                .await;
            this.update(cx, |page, cx| {
                page.reloading = false;
                page.loaded_days = days;
                page.hidden.clear();
                page.legend_expanded = false;
                page.breakdown_expanded = false;
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

    /// Switch the range: a fresh RPC for the new window, never a re-filter
    /// of the old reply. Selecting the range already shown is a no-op.
    fn set_days(&mut self, days: u32, cx: &mut Context<Self>) {
        if days == self.days {
            return;
        }
        self.days = days;
        self.load(cx);
    }

    /// The refresh button: re-run the same range through a fresh reload.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load(cx);
    }

    /// Hide or restore one legend row's bars. The last visible model
    /// refuses: [`toggle_hidden`] is the policy, the page just redraws.
    fn toggle_model(&mut self, id: String, cx: &mut Context<Self>) {
        let summary = self.stats.ready().map(fold_summary).unwrap_or_default();
        toggle_hidden(&mut self.hidden, &summary, &id);
        cx.notify();
    }

    /// Switch the Breakdown card.
    fn set_breakdown_tab(&mut self, tab: BreakdownTab, cx: &mut Context<Self>) {
        if self.breakdown_tab != tab {
            self.breakdown_tab = tab;
            self.hover_slice = None;
            cx.notify();
        }
    }

    /// The normal state's top bar: the count line on the left, the range
    /// switcher and refresh on the right. The count line names the range
    /// the on-screen reply answers — while a reload is in flight it keeps
    /// naming the stale window, never the pending one. A reload swaps the
    /// refresh icon for the working spinner; the button stays clickable.
    fn render_header(
        &self,
        reply: &UsageStatsReply,
        cx: &mut Context<Self>,
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
                    .id("usage-count-line")
                    .debug_selector(|| "usage-count-line".into())
                    .flex_1()
                    .min_w_0()
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(header_count_text(
                        reply.chat_count,
                        self.loaded_days,
                    ))),
            )
            .child(segmented_control(
                RANGES
                    .map(|days| self.range_chip(&theme, days, cx))
                    .into_iter()
                    .collect(),
                &theme,
            ))
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
                    .child(if self.reloading {
                        div()
                            .debug_selector(|| "usage-refresh-spinner".into())
                            .child(loaders::gradient_spinner(
                                "usage-refresh-spinner",
                                &theme,
                                3.0,
                                cx.entity_id(),
                                cx,
                            ))
                            .into_any_element()
                    } else {
                        icons::icon(icons::REFRESH)
                            .size(px(14.0))
                            .into_any_element()
                    }),
            )
    }

    /// One range segment inside the header's switcher. The active range
    /// carries the fill; the others are quiet switches.
    fn range_chip(&self, theme: &Theme, days: u32, cx: &Context<Self>) -> gpui::AnyElement {
        let active = self.days == days;
        let mut chip = segment(
            SharedString::from(format!("usage-range-{days}")),
            format!("usage-range-{days}"),
            SharedString::from(format!("{days}d")),
            active,
            theme,
        );
        if !active {
            chip = chip.on_click(cx.listener(move |page, _, _, cx| page.set_days(days, cx)));
        }
        chip.into_any_element()
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

    /// The Daily usage section under the hero: the section label over
    /// the stacked bar chart. The model toggle chips live in the hero
    /// beside the Total.
    fn render_summary(
        &self,
        reply: &UsageStatsReply,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = Theme::of(cx).clone();
        let summary = Arc::new(fold_summary(reply));
        let hidden = Arc::new(self.hidden.clone());
        let rungs = Arc::new(scale_rungs(summary.peak_day(&hidden)));
        div()
            .id("usage-summary")
            .debug_selector(|| "usage-summary".into())
            .mt(px(20.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(widgets::section_label(&theme, "Daily usage"))
            .child(self.render_chart(&summary, &hidden, rungs, &theme, cx))
    }

    /// The chart's legend, the hero row's right side: one toggle chip
    /// per model — checkbox filled with the series color while its
    /// bars show, hollow when hidden — plus the model's range total.
    /// Chips wrap beside the Total; past [`LEGEND_VISIBLE`] the rest
    /// fold behind a "+N more" chip so a long model list never
    /// stretches the hero row. A hidden chip reads dimmed.
    fn render_legend(
        &self,
        summary: &Arc<Summary>,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let total = summary.series.len();
        let collapsed = !self.legend_expanded && total > LEGEND_VISIBLE;
        let shown = if collapsed { LEGEND_VISIBLE } else { total };
        div()
            .id("usage-legend")
            .debug_selector(|| "usage-legend".into())
            .flex_1()
            .min_w_0()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_x(px(24.0))
            .gap_y(px(8.0))
            .children(
                summary
                    .series
                    .iter()
                    .take(shown)
                    .enumerate()
                    .map(|(rank, series)| self.legend_item(series, rank, theme, cx)),
            )
            .when(total > LEGEND_VISIBLE, |legend| {
                legend.child(self.legend_fold_chip(collapsed, total - LEGEND_VISIBLE, theme, cx))
            })
    }

    /// The legend's fold disclosure: "+N more" when collapsed, "Show
    /// less" once expanded — a quiet text switch next to the chips.
    fn legend_fold_chip(
        &self,
        collapsed: bool,
        folded: usize,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (tag, label) = if collapsed {
            ("more", format!("+{folded} more"))
        } else {
            ("less", "Show less".to_string())
        };
        div()
            .id(SharedString::from(format!("usage-legend-{tag}")))
            .debug_selector(move || format!("usage-legend-{tag}"))
            .flex_none()
            .h(px(20.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .rounded(px(5.0))
            .text_size(crate::typography::ui_rems(11.5))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|chip| chip.text_color(theme.text).bg(theme.ink(0.05)))
            .on_click(cx.listener(|page, _, _, cx| {
                page.legend_expanded = !page.legend_expanded;
                cx.notify();
            }))
            .child(label)
    }

    fn legend_item(
        &self,
        series: &ChartSeries,
        rank: usize,
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
            .flex_row()
            .items_center()
            .gap(px(6.0))
            // A real hover target: the wash rides a slightly padded pill
            // whose negative margins cancel the padding, so the item's text
            // stays on the line's grid while the hover has breathing room.
            .rounded(px(6.0))
            .px(px(6.0))
            .py(px(2.0))
            .mx(px(-6.0))
            .my(px(-2.0))
            .cursor_pointer()
            .hover(|item| item.bg(theme.ink(0.05)))
            .when(is_hidden, |item| item.opacity(0.55))
            .on_click(cx.listener(move |page, _, _, cx| page.toggle_model(id.clone(), cx)))
            .child(if is_hidden {
                div()
                    .flex_none()
                    .size(px(14.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(theme.border_strong)
                    .bg(theme.ink(0.02))
            } else {
                div()
                    .flex_none()
                    .size(px(14.0))
                    .rounded(px(4.0))
                    .bg(color)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        icons::icon(icons::CHECK)
                            .size(px(10.0))
                            .text_color(theme.on_solid),
                    )
            })
            .child(
                div()
                    .max_w(px(160.0))
                    .truncate()
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(series.id.clone())),
            )
            .child(
                div()
                    .text_size(crate::typography::ui_rems(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from(compact_tokens(series.tokens))),
            )
    }

    /// The stacked bar chart: the canvas, one hover column per day, and
    /// the hovered day's pinned readout, over a shared Y gutter, with one
    /// date label under its own day slot. A range with no usage anywhere
    /// renders as one quiet placeholder instead of an empty grid.
    fn render_chart(
        &self,
        summary: &Arc<Summary>,
        hidden: &Arc<BTreeSet<String>>,
        rungs: Arc<Vec<u64>>,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let days = summary.dates.len();
        let stride = label_stride(days);
        // No usage in the whole reply: an empty grid with "0 / 1" rungs
        // reads as a glitch, so the plot collapses to one quiet line.
        if summary.grand_total == 0 {
            return div()
                .id("usage-chart")
                .debug_selector(|| "usage-chart".into())
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .debug_selector(|| "usage-chart-empty".into())
                        .h(px(CHART_HEIGHT))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_faint)
                        .child("No usage in this range"),
                );
        }
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
                    // Y gutter: one label per rung, the top rung first.
                    .child(
                        div()
                            .flex_none()
                            .w(px(HEAT_GUTTER))
                            .h(px(CHART_HEIGHT))
                            .flex()
                            .flex_col()
                            .items_end()
                            .justify_between()
                            .text_size(crate::typography::ui_rems(10.0))
                            .text_color(theme.text_faint)
                            .children(
                                rungs
                                    .iter()
                                    .rev()
                                    .map(|rung| SharedString::from(compact_tokens(*rung))),
                            ),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .h(px(CHART_HEIGHT))
                            .child(bar_chart(summary, hidden, rungs, theme))
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .children((0..days).map(|day| self.day_column(day, theme, cx))),
                            )
                            // The pinned readout rides ABOVE the hover
                            // columns, but carries no interaction handlers,
                            // so it never steals their pointer.
                            .children(self.day_readout_overlay(summary, hidden, days, theme)),
                    ),
            )
            .child(
                // X labels: one slot per day, matching the bar slots; dense
                // ranges label every stride-th day, anchored to the last.
                div()
                    .flex()
                    .flex_row()
                    .gap(px(6.0))
                    .child(div().flex_none().w(px(HEAT_GUTTER)))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_row()
                            .text_size(crate::typography::ui_rems(10.0))
                            .text_color(theme.text_faint)
                            .children((0..days).map(|day| {
                                let labeled = (days - 1 - day).is_multiple_of(stride);
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .justify_center()
                                    // The label overflows its slot on ONE
                                    // line — a narrow slot must never wrap
                                    // it into stacked characters.
                                    .whitespace_nowrap()
                                    .when(self.hover_day == Some(day), |slot| {
                                        slot.text_color(theme.text)
                                    })
                                    .when(labeled, |slot| {
                                        let date =
                                            summary.dates.get(day).cloned().unwrap_or_default();
                                        slot.child(SharedString::from(
                                            short_date(&date).to_uppercase(),
                                        ))
                                    })
                            })),
                    ),
            )
    }

    /// One day's hover column — the full-height strip over that day's
    /// slice of the axis. Hovering washes the column and pins the day
    /// readout over the slot; leaving clears it only if it still owns the
    /// readout (adjacent columns' enter/leave can land out of order).
    fn day_column(
        &self,
        day: usize,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(SharedString::from(format!("usage-day-{day}")))
            .debug_selector(move || format!("usage-day-{day}"))
            .flex_1()
            .h_full()
            .cursor_default()
            .when(self.hover_day == Some(day), |col| col.bg(theme.ink(0.05)))
            .on_hover(cx.listener(move |page, hovered: &bool, _, cx| {
                if *hovered {
                    page.hover_day = Some(day);
                } else if page.hover_day == Some(day) {
                    page.hover_day = None;
                }
                cx.notify();
            }))
    }

    /// The hovered day's readout, pinned inside the plot over that day's
    /// own slot — centered on it, clamped to the plot's edges so it never
    /// escapes over the Y gutter or onto the content below.
    fn day_readout_overlay(
        &self,
        summary: &Arc<Summary>,
        hidden: &Arc<BTreeSet<String>>,
        days: usize,
        theme: &Theme,
    ) -> Option<AnyElement> {
        let day = self.hover_day.filter(|day| *day < days)?;
        let mut slot = div()
            .absolute()
            .top_0()
            .bottom_0()
            .left(gpui::relative(day as f32 / days as f32))
            .right(gpui::relative(1.0 - (day + 1) as f32 / days as f32))
            .flex()
            .items_start()
            .child(self.day_readout(summary, hidden, day, theme));
        slot = if day == 0 {
            slot.justify_start()
        } else if day == days - 1 {
            slot.justify_end()
        } else {
            slot.justify_center()
        };
        Some(slot.into_any_element())
    }

    /// The pinned day readout: the date, one row per model with usage that
    /// day (zero days drop out), and the day's Total — the same series
    /// colors as the bars beneath. Compact counts, unlike the tooltip this
    /// replaces: a raw "72659522" reads as noise at a glance.
    fn day_readout(
        &self,
        summary: &Arc<Summary>,
        hidden: &Arc<BTreeSet<String>>,
        day: usize,
        theme: &Theme,
    ) -> AnyElement {
        let date = summary.dates.get(day).cloned().unwrap_or_default();
        // flex_none is load-bearing: the card is a flex child of a slot as
        // narrow as one day, and without it flex-shrink crushes the card
        // into a vertical sliver of wrapped characters.
        let mut card = popover::popover_card(theme)
            .flex_none()
            .w(px(READOUT_WIDTH))
            .p(px(8.0))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .text_size(crate::typography::ui_rems(11.5))
            .debug_selector(|| "usage-day-readout".into())
            .child(
                div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(SharedString::from(short_date(&date))),
            );
        let rows = summary.day_rows(hidden, day);
        for (rank, id, tokens) in rows.iter().filter(|(_, _, tokens)| *tokens > 0) {
            card = card.child(
                div()
                    .debug_selector(move || format!("usage-dayrow-{id}"))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex_none()
                            .size(px(6.0))
                            .rounded_full()
                            .bg(series_color(*rank, theme)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.text_muted)
                            .child(SharedString::from(id.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.text)
                            .child(SharedString::from(compact_tokens(*tokens))),
                    ),
            );
        }
        if rows.is_empty() {
            card = card.child(div().text_color(theme.text_faint).child("No usage"));
        } else {
            card = card.child(div().h(px(1.0)).bg(theme.ink(0.08))).child(
                div()
                    .debug_selector(|| "usage-day-total".into())
                    .flex()
                    .flex_row()
                    .justify_between()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child("Total")
                    .child(SharedString::from(compact_tokens(
                        summary.day_total(hidden, day),
                    ))),
            );
        }
        crate::frost::frosted(popover::CARD_RADIUS, crate::frost::MENU_BLUR, card)
            .into_any_element()
    }

    /// The hero block above the chart: Total tokens as the big number
    /// over the exact comma-separated count on the left, the model
    /// toggle chips wrapping on the right. The numbers are range
    /// facts: the chips' visibility toggles never touch them.
    fn render_total_hero(
        &self,
        reply: &UsageStatsReply,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = Theme::of(cx).clone();
        let totals = &reply.totals;
        let total = totals.input + totals.output + totals.cache_read + totals.cache_write;
        let summary = Arc::new(fold_summary(reply));
        div()
            .id("usage-hero")
            .debug_selector(|| "usage-hero".into())
            .mt(px(20.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(40.0))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child("Total tokens"),
                    )
                    .child(
                        div()
                            .debug_selector(|| "usage-total".into())
                            .text_size(crate::typography::ui_rems(32.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(SharedString::from(compact_tokens(total))),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(format!(
                                "{} tokens",
                                exact_tokens(total)
                            ))),
                    ),
            )
            .child(self.render_legend(&summary, &theme, cx))
    }

    /// The six metric tiles below the chart: one card split into six
    /// cells by hairline dividers — compact values on the cells, exact
    /// counts on hover.
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
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .children(
                metric_tiles(reply)
                    .iter()
                    .enumerate()
                    .map(|(index, tile)| self.metric_cell(tile, index > 0, &theme)),
            )
    }

    /// One metric tile: the muted label over the compact value, the
    /// exact counts on hover. `divided` draws the hairline separating
    /// the cell from the one on its left.
    fn metric_cell(
        &self,
        tile: &MetricTile,
        divided: bool,
        theme: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        let slug = tile.label.to_lowercase().replace(' ', "-");
        let detail = tile.detail.clone();
        div()
            .id(SharedString::from(format!("usage-metric-{slug}")))
            .debug_selector(move || format!("usage-metric-{slug}"))
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .px(px(14.0))
            .py(px(12.0))
            .when(divided, |cell| cell.border_l_1().border_color(theme.border))
            .cursor_default()
            .hover(|cell| cell.bg(theme.ink(0.03)))
            .tooltip(move |_, cx| {
                cx.new(|_| crate::image_viewer::ViewerTooltip(detail.clone().into()))
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
                    .text_size(crate::typography::ui_rems(16.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from(tile.value.clone())),
            )
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
                                    // The label overflows its week-column
                                    // slot on ONE line — never wraps into
                                    // vertical letters; the next 1st-of-
                                    // month is weeks away, so nothing
                                    // collides.
                                    .whitespace_nowrap()
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

    /// The Breakdown block: a donut + legend card over the reply's own
    /// by-model / by-project rows — the engine hands them total-
    /// descending, the donut draws them clockwise from 12 o'clock and the
    /// legend lists the same order with integer share percentages and
    /// compact counts. Model entries carry the same color dot as their
    /// chart counterparts. Past [`BREAKDOWN_VISIBLE`] rows the tail folds
    /// into the Others slice, whose own legend row is the disclosure —
    /// closed by default.
    fn render_breakdown(
        &self,
        reply: &UsageStatsReply,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = Theme::of(cx).clone();
        let summary = fold_summary(reply);
        let entries = fold_breakdown(reply, &summary, self.breakdown_tab, &theme);
        let fold = breakdown_fold(&entries, &theme);
        let grand: u64 = entries.iter().map(|entry| entry.tokens).sum();
        let body = if grand == 0 {
            div()
                .debug_selector(|| "usage-breakdown-empty".into())
                .mt(px(10.0))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_faint)
                .child("No usage in this range")
                .into_any_element()
        } else {
            div()
                .debug_selector(|| "usage-breakdown-card".into())
                .mt(px(10.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(56.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.border)
                .p(px(24.0))
                .child(self.donut_block(
                    &fold.slices,
                    grand,
                    self.hover_slice.filter(|hover| *hover < fold.slices.len()),
                    &theme,
                ))
                .child(self.breakdown_legend(&fold, grand, &theme, cx))
                .into_any_element()
        };
        div()
            .id("usage-breakdown")
            .debug_selector(|| "usage-breakdown".into())
            .mt(px(28.0))
            .flex()
            .flex_col()
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
                    .child(segmented_control(
                        vec![
                            self.breakdown_chip(BreakdownTab::Models, &theme, cx),
                            self.breakdown_chip(BreakdownTab::Projects, &theme, cx),
                        ],
                        &theme,
                    )),
            )
            .child(body)
    }

    /// The donut: a fixed-size square canvas with the readout riding the
    /// hole — the grand total over the muted "tokens" unit at rest, the
    /// hovered entry's compact count over its truncated name on hover.
    fn donut_block(
        &self,
        entries: &[BreakdownEntry],
        grand: u64,
        hover: Option<usize>,
        theme: &Theme,
    ) -> gpui::Div {
        let (value, label) = match hover.and_then(|index| entries.get(index)) {
            Some(entry) => (compact_tokens(entry.tokens), Some(entry.name.clone())),
            None => (compact_tokens(grand), None),
        };
        div()
            .debug_selector(|| "usage-breakdown-donut".into())
            .flex_none()
            .size(px(DONUT_SIZE))
            .relative()
            .child(donut_chart(entries, grand, hover))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(4.0))
                    .child(
                        div()
                            .debug_selector(|| "usage-breakdown-total".into())
                            .text_size(crate::typography::ui_rems(18.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(SharedString::from(value)),
                    )
                    .child(
                        div()
                            // A name rides the hole on hover: clamped to
                            // the inner diameter, truncated, never pushing
                            // the hole's text wider than the ring.
                            .max_w(px(100.0))
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(
                                label.unwrap_or_else(|| "tokens".to_string()),
                            )),
                    ),
            )
    }

    /// The legend: one row per entry — dot, name, integer share on the
    /// first line; the compact count indented under the name on the
    /// second; a hairline between rows, none after the last. A tail ends
    /// the list with the Others row, whose own rows itemize under it
    /// while the fold is open. Hovering a row washes it, dims the donut
    /// down to its slice, and swaps the center readout to that entry.
    fn breakdown_legend(
        &self,
        fold: &BreakdownFold,
        grand: u64,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let others = fold.others();
        let mut legend = div().flex_1().min_w_0().flex().flex_col();
        for (index, entry) in fold.slices.iter().enumerate() {
            let kind = if others == Some(index) {
                BreakdownRowKind::Others
            } else {
                BreakdownRowKind::Slice
            };
            // The Others row keeps its hairline while its tail is
            // itemized below it; every other row's rule separates it from
            // the row under it, and the list's own last row carries none.
            let rule = index + 1 < fold.slices.len()
                || (kind == BreakdownRowKind::Others && self.breakdown_expanded);
            let row = BreakdownRow {
                id: format!("usage-breakdown-row-{index}"),
                slice: index,
                kind,
            };
            legend = legend.child(self.ruled(
                self.breakdown_row(&row, entry, grand, theme, cx),
                rule,
                theme,
            ));
        }
        if self.breakdown_expanded {
            for (index, entry) in fold.tail.iter().enumerate() {
                let row = BreakdownRow {
                    id: format!("usage-breakdown-child-{index}"),
                    slice: others.unwrap_or_default(),
                    kind: BreakdownRowKind::Child,
                };
                legend = legend.child(self.ruled(
                    self.breakdown_row(&row, entry, grand, theme, cx),
                    index + 1 < fold.tail.len(),
                    theme,
                ));
            }
        }
        legend
    }

    /// Hang the row's hairline under it: every row but the list's last
    /// carries one.
    fn ruled(
        &self,
        row: gpui::Stateful<gpui::Div>,
        rule: bool,
        theme: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        if rule {
            row.border_b_1().border_color(theme.border)
        } else {
            row
        }
    }

    /// One legend row: dot, name, integer share on the first line; the
    /// compact count indented under the name on the second. Hovering
    /// washes the row, dims the donut down to the row's slice and swaps
    /// the center readout to that entry. The Others row is also the
    /// fold's control: it carries the caret and takes the click, and the
    /// rows it folds ([`BreakdownRowKind::Child`]) are indented under it.
    fn breakdown_row(
        &self,
        row: &BreakdownRow,
        entry: &BreakdownEntry,
        grand: u64,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id = row.id.clone();
        let slice = row.slice;
        let is_others = row.kind == BreakdownRowKind::Others;
        let indent = if row.kind.is_child() { 19.0 } else { 0.0 };
        let caret = if self.breakdown_expanded {
            icons::ALT_ARROW_DOWN
        } else {
            icons::ALT_ARROW_RIGHT
        };
        let debug_id = id.clone();
        let row = div()
            .id(SharedString::from(id))
            .debug_selector(move || debug_id)
            .flex()
            .flex_col()
            .gap(px(4.0))
            .py(px(12.0))
            // The hover wash rides a slightly padded pill whose negative
            // margins cancel the padding, keeping the hairline grid and
            // text alignment untouched.
            .rounded(px(6.0))
            .px(px(8.0))
            .mx(px(-8.0))
            .cursor_default()
            .hover(|row| row.bg(theme.ink(0.04)))
            .on_hover(cx.listener(move |page, hovered: &bool, _, cx| {
                if *hovered {
                    page.hover_slice = Some(slice);
                } else if page.hover_slice == Some(slice) {
                    page.hover_slice = None;
                }
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .pl(px(indent))
                    .child(
                        div()
                            .flex_none()
                            .size(px(9.0))
                            .rounded_full()
                            .bg(entry.color),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(crate::typography::ui_rems(12.5))
                                    .text_color(theme.text)
                                    .child(SharedString::from(entry.name.clone())),
                            )
                            // The Others row's fold caret rides after the
                            // name, so the dot and share columns stay on
                            // the other rows' grid.
                            .when(is_others, |name| {
                                name.child(
                                    icons::icon(caret)
                                        .flex_none()
                                        .size(px(11.0))
                                        .text_color(theme.text_faint),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(crate::typography::ui_rems(12.5))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!(
                                "{}%",
                                breakdown_percent(entry.tokens, grand)
                            ))),
                    ),
            )
            .child(
                div()
                    .pl(px(19.0 + indent))
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format!(
                        "{} tokens",
                        compact_tokens(entry.tokens)
                    ))),
            );
        if !is_others {
            return row;
        }
        row.cursor_pointer().on_click(cx.listener(|page, _, _, cx| {
            page.breakdown_expanded = !page.breakdown_expanded;
            cx.notify();
        }))
    }

    /// One of the two section segments — the range switcher's shape.
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
        let mut chip = segment(
            SharedString::from(format!("usage-bd-tab-{tag}")),
            format!("usage-bd-tab-{tag}"),
            SharedString::from(label),
            active,
            theme,
        );
        if !active {
            chip =
                chip.on_click(cx.listener(move |page, _, _, cx| page.set_breakdown_tab(tab, cx)));
        }
        chip.into_any_element()
    }
}

/// The Breakdown donut's paint pass: filled annular sectors (gpui paths
/// have no arc primitive), one per entry, clockwise from 12 o'clock in
/// engine order. Each segment gives up its share of a constant-width
/// gap so slices read as slices, clamped to a hairline minimum so tiny
/// shares stay visible; a single-entry
/// donut closes its circle without a gap. A two-slice donut instead
/// centers its smaller slice at 12 o'clock, so the two seams mirror
/// across the vertical axis — one axis-aligned seam beside a data-angle
/// seam reads as crooked, a mirrored pair reads as balanced.
fn donut_chart(entries: &[BreakdownEntry], grand: u64, hover: Option<usize>) -> AnyElement {
    let grand = grand.max(1) as f32;
    let gap = if entries.len() > 1 { DONUT_GAP } else { 0.0 };
    let mut start = -std::f32::consts::FRAC_PI_2;
    if entries.len() == 2 {
        // Entries are total-descending, so slice 1 is the smaller one;
        // its center rides at start + sweep0 + sweep1/2.
        let sweep0 = entries[0].tokens as f32 / grand * std::f32::consts::TAU;
        start -= sweep0 + (std::f32::consts::TAU - sweep0) / 2.0;
    }
    // Segments carry their nominal boundaries; the gap trim happens
    // per radius in the paint pass.
    let mut segments: Vec<(Hsla, f32, f32)> = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let sweep = entry.tokens as f32 / grand * std::f32::consts::TAU;
        // Hover dims the ring down to the hovered slice.
        let color = match hover {
            Some(hovered) if hovered != index => entry.color.opacity(0.3),
            _ => entry.color,
        };
        segments.push((color, start, start + sweep));
        start += sweep;
    }
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let center = bounds.center();
            // The margin keeps the ring's outer edge clear of the canvas
            // bounds — an edge touching them clips its anti-aliasing.
            let outer = DONUT_SIZE / 2.0 - DONUT_MARGIN;
            let inner = outer - DONUT_THICKNESS;
            for (color, start, end) in &segments {
                let sweep = end - start;
                let mid = (start + end) / 2.0;
                // Each seam is a constant-width strip, not an angular
                // wedge: a slice edge is the chord parallel to the seam's
                // radial line at half the gap from it, so the trim angle
                // grows toward the hole — asin((gap/2) / r) — and the
                // slit's two edges stay parallel.
                let edge = |radius: f32| {
                    let trim = if gap == 0.0 {
                        0.0
                    } else {
                        (gap / 2.0 / radius).asin()
                    };
                    let drawn = (sweep - 2.0 * trim).max(DONUT_MIN_SLIVER);
                    (mid - drawn / 2.0, mid + drawn / 2.0)
                };
                let (outer_start, outer_end) = edge(outer);
                let (inner_start, inner_end) = edge(inner);
                // 96 chords per full circle: the chord error at ring size
                // is a fraction of a pixel — the polyline reads as an arc.
                let span = (outer_end - outer_start).max(inner_end - inner_start);
                let steps = ((span / std::f32::consts::TAU) * 96.0).ceil().max(2.0) as usize;
                let at = |radius: f32, from: f32, to: f32, i: usize| {
                    let theta = from + (to - from) * (i as f32 / steps as f32);
                    point(
                        center.x + px(radius * theta.cos()),
                        center.y + px(radius * theta.sin()),
                    )
                };
                // A filled annular sector — outer arc, inner arc, close —
                // not a stroked arc: a stroke's butt cap cuts perpendicular
                // to the last chord, which reads as a slanted wedge at ring
                // thickness, while a fill's cut follows the seam's edge.
                let mut builder = PathBuilder::fill();
                builder.move_to(at(outer, outer_start, outer_end, 0));
                for i in 1..=steps {
                    builder.line_to(at(outer, outer_start, outer_end, i));
                }
                for i in (0..=steps).rev() {
                    builder.line_to(at(inner, inner_start, inner_end, i));
                }
                if let Ok(path) = builder.build() {
                    window.paint_path(path, *color);
                }
            }
        },
    )
    .absolute()
    .inset_0()
    .into_any_element()
}

/// The chart's paint pass: one hairline per Y rung, then one bar per day —
/// the visible models stacked bottom-to-top in rank order, the stack's
/// topmost nonzero segment carrying the rounded cap (the reference
/// chart's pill-tipped bars). `rungs` is the shared Y scale bottom-up;
/// its last value is the ceiling every bar and gridline divides by.
fn bar_chart(
    summary: &Arc<Summary>,
    hidden: &Arc<BTreeSet<String>>,
    rungs: Arc<Vec<u64>>,
    theme: &Theme,
) -> AnyElement {
    let grid = theme.ink(0.08);
    let visible: Vec<(Hsla, Vec<u64>)> = summary
        .visible(hidden)
        .into_iter()
        .map(|(rank, series)| (series_color(rank, theme), series.per_day.clone()))
        .collect();
    let days = summary.dates.len();
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let height = f32::from(bounds.size.height);
            let width = f32::from(bounds.size.width);
            let left = f32::from(bounds.origin.x);
            let baseline = f32::from(bounds.origin.y) + height;
            let top = rungs.last().copied().unwrap_or(1).max(1) as f32;
            // The Y rungs the gutter labels — thin quads, the same
            // primitive as the bars.
            for rung in rungs.iter() {
                let y = baseline - height * (*rung as f32 / top);
                window.paint_quad(gpui::fill(
                    Bounds::new(point(px(left), px(y - 0.5)), size(px(width), px(1.0))),
                    grid,
                ));
            }
            if days == 0 {
                return;
            }
            // One slot per day; the bar takes the reference's share of its
            // slot, capped so a short range paints bars, not slabs.
            let slot = width / days as f32;
            let bar_w = (slot * 0.55).min(28.0);
            for day in 0..days {
                let segments: Vec<(Hsla, f32)> = visible
                    .iter()
                    .map(|(color, per_day)| (*color, per_day.get(day).copied().unwrap_or(0) as f32))
                    .collect();
                // The stack's topmost nonzero segment owns the rounded cap.
                let cap = segments.iter().rposition(|(_, tokens)| *tokens > 0.0);
                let x0 = left + slot * (day as f32 + 0.5) - bar_w / 2.0;
                let mut below = 0.0f32;
                for (index, (color, tokens)) in segments.iter().enumerate() {
                    if *tokens <= 0.0 {
                        continue;
                    }
                    let seg_h = height * (tokens / top);
                    let y = baseline - below - seg_h;
                    let mut quad = gpui::fill(
                        Bounds::new(point(px(x0), px(y)), size(px(bar_w), px(seg_h))),
                        *color,
                    );
                    if Some(index) == cap {
                        // The cap's radius yields to the segment it rounds:
                        // a corner taller than half its side paints sharp.
                        let radius = 5.0f32.min(seg_h * 0.5).min(bar_w * 0.5);
                        quad = quad.corner_radii(gpui::Corners {
                            top_left: px(radius),
                            top_right: px(radius),
                            bottom_right: px(0.0),
                            bottom_left: px(0.0),
                        });
                    }
                    window.paint_quad(quad);
                    below += seg_h;
                }
            }
        },
    )
    .absolute()
    .inset_0()
    .into_any_element()
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
                        // Stale-while-revalidate: a reload dims the page in
                        // place — the header spinner carries the activity —
                        // instead of blanking to skeletons.
                        .when(self.reloading, |page| page.opacity(0.6))
                        .child(self.render_header(reply, cx))
                        .child(self.render_total_hero(reply, cx))
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
            .child(widgets::page_column().child(title_row(&theme)).child(body))
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

        fn reloading(&self) -> bool {
            self.visual.read(|cx| self.page.read(cx).reloading)
        }

        fn loaded_days(&self) -> u32 {
            self.visual.read(|cx| self.page.read(cx).loaded_days)
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
    fn switching_range_and_refreshing_rerun_the_rpc_stale_visible(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, vec![], reply(5));
        assert_eq!(harness.calls(), vec![30]);

        // The range switch re-queries with the new days — but the stale
        // page stays on screen (dimmed, spinner in the header) until the
        // fresh reply lands; a switch never blanks the page to skeletons.
        harness.click("usage-range-7");
        assert!(!harness.loading(), "ready data never drops to skeletons");
        assert!(harness.reloading());
        assert_eq!(harness.days(), 7);
        assert_eq!(
            harness.loaded_days(),
            30,
            "the count line still names the window on screen"
        );
        harness.repaint();
        assert!(
            harness.present("usage-header"),
            "stale content stays visible"
        );
        assert!(harness.present("usage-refresh-spinner"));
        harness.pump();
        assert!(!harness.reloading());
        assert_eq!(harness.loaded_days(), 7);
        assert!(
            !harness.present("usage-refresh-spinner"),
            "the spinner retires with the reload"
        );
        assert_eq!(harness.calls(), vec![30, 7]);

        // Refresh re-runs the current range through the same veil.
        harness.click("usage-refresh");
        assert!(harness.reloading());
        harness.pump();
        assert!(!harness.reloading());
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
        // their sum, and the Y scale tops at the tallest stacked day.
        assert_eq!(summary.day_rows(&hidden, 1).len(), 2);
        assert_eq!(summary.day_total(&hidden, 1), 110);
        assert_eq!(summary.peak_day(&hidden), 200);

        // Hiding openai drops its tokens from the day total, the rows, and
        // the Y scale — the readout always matches the picture.
        hidden.insert("openai/gpt-5.4".into());
        assert_eq!(summary.day_total(&hidden, 1), 60);
        assert_eq!(summary.day_total(&hidden, 0), 0);
        assert_eq!(
            summary.peak_day(&hidden),
            60,
            "the scale follows the visible bars"
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
    fn the_hero_caption_prints_the_exact_count() {
        assert_eq!(exact_tokens(0), "0");
        assert_eq!(exact_tokens(999), "999");
        assert_eq!(exact_tokens(1_000), "1,000");
        assert_eq!(exact_tokens(858_392_112), "858,392,112");
    }

    #[gpui::test]
    fn the_hero_sits_above_the_chart(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, vec![], summary_reply());

        // The hero block rides above the Daily usage chart, the six
        // tiles in the strip below the chart.
        assert!(harness.present("usage-hero"));
        assert!(harness.present("usage-total"), "the hero total renders");
        assert!(harness.present("usage-metrics"));
        let hero = harness.bounds("usage-hero");
        let chart = harness.bounds("usage-summary");
        assert!(
            hero.bottom() <= chart.origin.y,
            "the hero rides above the chart: {hero:?} vs {chart:?}"
        );
        // The model toggle chips ride inside the hero, right of the
        // Total block.
        let total = harness.bounds("usage-total");
        let legend = harness.bounds("usage-legend");
        assert!(
            legend.left() >= total.right(),
            "the legend sits right of the total: {legend:?} vs {total:?}"
        );
        assert!(
            legend.origin.y >= hero.origin.y && legend.bottom() <= hero.bottom(),
            "the legend stays inside the hero row: {legend:?} vs {hero:?}"
        );
        let metrics = harness.bounds("usage-metrics");
        assert!(
            metrics.origin.y >= chart.bottom(),
            "the metrics strip sits below the chart"
        );
        for slug in [
            "input",
            "output",
            "cache-read",
            "cache-write",
            "cache-hit",
            "active-days",
        ] {
            assert!(
                harness.present(Box::leak(format!("usage-metric-{slug}").into_boxed_str())),
                "the {slug} tile renders"
            );
        }

        // The hero is a range fact: hiding a model from the chart leaves
        // it untouched.
        harness.click("usage-legend-1");
        assert!(harness.present("usage-total"));
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
    fn the_summary_paints_the_legend_line_and_day_columns(cx: &mut gpui::TestAppContext) {
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

        // The legend line carries one checkbox item per model.
        assert!(harness.present("usage-legend-0"));
        assert!(harness.present("usage-legend-1"));
    }

    #[gpui::test]
    fn legend_clicks_toggle_bars_and_a_reload_resets_them(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, vec![], summary_reply());
        let shows = |harness: &Harness<'_>, id: &str| {
            harness
                .visual
                .read(|cx| !harness.page.read(cx).hidden.contains(id))
        };
        assert!(shows(&harness, "anthropic/claude-opus"));

        // A click hides one model's bars — its day readout and its stack
        // layer.
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
            "a reload restores every series"
        );
        assert!(harness.present("usage-day-0"), "back on the ready state");
    }

    #[gpui::test]
    fn the_legend_folds_extra_models_behind_a_more_chip(cx: &mut gpui::TestAppContext) {
        // Eight models: the first six show, the rest fold behind a
        // "+2 more" chip, so the hero row keeps its height.
        let models: Vec<serde_json::Value> = (0..8)
            .map(|index| {
                serde_json::json!({
                    "provider": "p",
                    "model": format!("m{index}"),
                    "days": [{"date": "2026-09-16", "tokens": (8 - index) * 10}],
                })
            })
            .collect();
        let reply = Scripted::Ok(serde_json::json!({
            "chatCount": 1, "days": 30,
            "totals": { "input": 10, "output": 10, "cacheRead": 0,
                        "cacheWrite": 0, "cacheHit": null, "activeDays": 1 },
            "models": models, "byModel": [], "byProject": [], "heatmap": [],
        }));
        let mut harness = harness(cx, vec![], reply);

        assert!(harness.present("usage-legend-5"));
        assert!(
            !harness.present("usage-legend-6"),
            "the seventh model folds away"
        );
        assert!(harness.present("usage-legend-more"));

        harness.click("usage-legend-more");
        harness.repaint();
        assert!(harness.present("usage-legend-7"));
        assert!(harness.present("usage-legend-less"));
        assert!(!harness.present("usage-legend-more"));

        harness.click("usage-legend-less");
        harness.repaint();
        assert!(!harness.present("usage-legend-6"));

        // A reload re-folds the legend, like the visibility toggles.
        harness.click("usage-legend-more");
        harness.repaint();
        harness.click("usage-refresh");
        harness.pump();
        assert!(harness.present("usage-legend-more"));
        assert!(!harness.present("usage-legend-6"));
    }

    #[gpui::test]
    fn an_empty_range_shows_a_quiet_placeholder(cx: &mut gpui::TestAppContext) {
        // Chats exist, but the range's model series is empty (usage lives
        // outside the window, or none at all): the chart collapses to one
        // quiet line instead of an empty grid with a "0 / 1" axis.
        let mut harness = harness(cx, vec![], reply(3));

        assert!(harness.present("usage-header"), "the page still renders");
        assert!(harness.present("usage-chart-empty"));
        assert!(
            !harness.present("usage-day-0"),
            "no hover columns without data"
        );
        assert!(!harness.present("usage-legend-0"));
    }

    #[gpui::test]
    fn the_readout_keeps_its_width_over_a_narrow_slot(cx: &mut gpui::TestAppContext) {
        // Thirty days of one model: each day slot shrinks far below the
        // readout's fixed width — the card must ride its flex_none and
        // overflow the slot, not crush into a vertical sliver.
        let today = chrono::Local::now().date_naive();
        let days: Vec<serde_json::Value> = (0..30)
            .map(|i| {
                day_json(
                    &(today - chrono::Duration::days(29 - i as i64))
                        .format("%Y-%m-%d")
                        .to_string(),
                    (i + 1) as u64,
                )
            })
            .collect();
        let reply = Scripted::Ok(serde_json::json!({
            "chatCount": 1, "days": 30,
            "totals": { "input": 10, "output": 10, "cacheRead": 0,
                        "cacheWrite": 0, "cacheHit": null, "activeDays": 30 },
            "models": [{"provider": "p", "model": "m", "days": days}],
            "byModel": [], "byProject": [], "heatmap": [],
        }));
        let mut harness = harness(cx, vec![], reply);

        let column = harness.bounds("usage-day-15");
        assert!(
            column.size.width < px(READOUT_WIDTH),
            "the fixture must produce slots narrower than the readout"
        );
        harness
            .visual
            .simulate_mouse_move(column.center(), None, Default::default());
        harness.repaint();

        let readout = harness.bounds("usage-day-readout");
        assert!(
            (f32::from(readout.size.width) - READOUT_WIDTH).abs() < 1.0,
            "the readout keeps its fixed width, got {:?}",
            readout.size.width
        );
    }

    #[gpui::test]
    fn a_hovered_day_pins_its_readout_inside_the_plot(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, vec![], summary_reply());
        assert!(
            !harness.present("usage-day-readout"),
            "no readout until a column is hovered"
        );

        // Hover the middle day: the readout pins inside the chart body —
        // never over the Y gutter or outside the plot.
        let column = harness.bounds("usage-day-1");
        harness
            .visual
            .simulate_mouse_move(column.center(), None, Default::default());
        harness.repaint();
        assert!(harness.present("usage-day-readout"));
        assert!(harness.present("usage-dayrow-anthropic/claude-opus"));
        let readout = harness.bounds("usage-day-readout");
        let plot = harness.bounds("usage-day-0");
        let plot_right = harness.bounds("usage-day-2").right();
        assert!(
            readout.left() >= plot.left() && readout.right() <= plot_right,
            "the readout stays between the plot's edges: {readout:?}"
        );

        // Moving off the chart clears it.
        let header = harness.bounds("usage-refresh");
        harness
            .visual
            .simulate_mouse_move(header.center(), None, Default::default());
        harness.repaint();
        assert!(
            !harness.present("usage-day-readout"),
            "the readout follows the pointer off the columns"
        );
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
    fn the_scale_rounds_the_peak_to_clean_rungs() {
        // The reference chart's shape: rungs land on round numbers and the
        // top rung clears the peak.
        assert_eq!(
            scale_rungs(188_000_000),
            vec![0, 50_000_000, 100_000_000, 150_000_000, 200_000_000]
        );
        assert_eq!(
            scale_rungs(30_000_000),
            vec![0, 10_000_000, 20_000_000, 30_000_000]
        );
        assert_eq!(scale_rungs(110), vec![0, 50, 100, 150]);
        // Small counts stay whole-token rungs.
        assert_eq!(scale_rungs(9), vec![0, 5, 10]);
        assert_eq!(scale_rungs(1), vec![0, 1]);
    }

    #[test]
    fn x_labels_label_every_bar_until_dense() {
        assert_eq!(label_stride(7), 1);
        assert_eq!(label_stride(14), 1);
        assert_eq!(label_stride(30), 3);
        assert_eq!(label_stride(90), 8);
    }

    #[test]
    fn axis_dates_print_short() {
        assert_eq!(short_date("2026-09-16"), "Sep 16");
        assert_eq!(short_date("2026-08-01"), "Aug 1");
        assert_eq!(short_date("not a date"), "not a date");
    }

    #[test]
    fn the_heatmap_hover_prints_compact_and_short() {
        let grid = fold_heatmap(&decode_reply(two_week_reply()));
        let wednesday = cell(&grid, 2, 3);
        assert_eq!(heatmap_tooltip(wednesday), "130 tokens on Sep 16, 2026");
        // Big counts stay glanceable, never a digit soup.
        let big = HeatCell {
            date: wednesday.date,
            tokens: 114_891,
            col: 0,
            row: 0,
        };
        assert_eq!(heatmap_tooltip(&big), "114.9k tokens on Sep 16, 2026");
    }

    #[test]
    fn month_labels_mark_where_a_month_first_appears() {
        // A 33-day window spanning one month boundary: Aug 15 (Sat) 2026
        // to Sep 16. The column-month-change rule labels the leading
        // partial month at column 0, then Sep where its days first appear
        // (Sep 1 lands in column 3: 6 leading offsets + 17 days).
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
        assert_eq!(
            labels,
            vec![(0, "Aug".to_string()), (3, "Sep".to_string())],
            "{labels:?}"
        );
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
    // Ticket 05 — the Breakdown card (donut + legend)
    // -------------------------------------------------------------------

    /// Three model rows, three project groups (same-basename directories,
    /// plus the Deleted chats catch-all), one path long enough to test
    /// truncation, and one zero-total model that must never render.
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
                {"provider": "noop", "model": "none",
                 "input": 0, "output": 0, "cacheRead": 0, "total": 0},
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

    /// Seven model rows — past the four-row fold — over nothing else: the
    /// fold's own fixture. Totals run 700 down to 100, so the folded tail
    /// (the last three rows) sums to 600.
    fn folded_breakdown_reply_json() -> serde_json::Value {
        let models: Vec<serde_json::Value> = (1..=7)
            .map(|rank| {
                serde_json::json!({
                    "provider": "openai",
                    "model": format!("gpt-{rank}"),
                    "input": (8 - rank) * 100,
                    "output": 0,
                    "cacheRead": 0,
                    "total": (8 - rank) * 100,
                })
            })
            .collect();
        serde_json::json!({
            "chatCount": 7,
            "days": 30,
            "totals": {
                "input": 2800, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                "cacheHit": 0.0, "activeDays": 7,
            },
            "models": [], "heatmap": [],
            "byModel": models,
            "byProject": [],
        })
    }

    #[test]
    fn the_breakdown_folds_its_tail_into_one_others_slice() {
        let reply = decode_reply(folded_breakdown_reply_json());
        let summary = fold_summary(&reply);
        let theme = Theme::default();
        let entries = fold_breakdown(&reply, &summary, BreakdownTab::Models, &theme);
        assert_eq!(entries.len(), 7);

        // The top four in engine order, then one Others slice carrying the
        // tail — the ring keeps its true shares because nothing leaves the
        // list, and the tail's own gray is the slice's.
        let fold = breakdown_fold(&entries, &theme);
        let names: Vec<&str> = fold
            .slices
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "openai/gpt-1",
                "openai/gpt-2",
                "openai/gpt-3",
                "openai/gpt-4",
                BREAKDOWN_OTHERS
            ]
        );
        assert_eq!(fold.slices[4].tokens, 600, "the tail's own sum");
        assert_eq!(fold.others(), Some(4), "the Others row's own slice");
        assert_eq!(
            fold.tail
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["openai/gpt-5", "openai/gpt-6", "openai/gpt-7"],
            "the tail stays in engine order"
        );
        assert!(
            fold.tail
                .iter()
                .all(|entry| entry.color == fold.slices[4].color),
            "an itemized row wears the group's own gray, not a rank hue"
        );
        assert_eq!(
            fold.slices.iter().map(|entry| entry.tokens).sum::<u64>(),
            entries.iter().map(|entry| entry.tokens).sum::<u64>(),
            "the fold groups slices, it never drops them"
        );

        // A short reply prints whole: no Others row, no tail, so nothing
        // folds — the boundary is inclusive.
        let fits = breakdown_fold(&entries[..BREAKDOWN_VISIBLE], &theme);
        assert_eq!(fits.slices.len(), BREAKDOWN_VISIBLE);
        assert_eq!(fits.others(), None);
        assert!(fits.tail.is_empty());
    }

    #[test]
    fn deleted_chat_subtotals_sum_to_the_group_entry() {
        let reply = decode_reply(breakdown_reply_json());
        let deleted = reply
            .by_project
            .iter()
            .find(|group| group.path.is_none())
            .expect("the deleted-chats group");
        // The legend entry's number is exactly the sum of the chats it
        // aggregates — the engine's own four-field arithmetic.
        let sum: u64 = deleted.chats.iter().map(|chat| chat.total).sum();
        assert_eq!(sum, deleted.total);
        assert_eq!(deleted.chats.len(), 2);
    }

    #[test]
    fn the_breakdown_folds_engine_order_and_drops_zero_rows() {
        let reply = decode_reply(breakdown_reply_json());
        let summary = fold_summary(&reply);
        let theme = Theme::default();

        // By model: engine order (total-descending); the zero-total model
        // never becomes a row, so the shares stay meaningful.
        let models = fold_breakdown(&reply, &summary, BreakdownTab::Models, &theme);
        let names: Vec<&str> = models.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "openai/gpt-5.4",
                "anthropic/claude-opus",
                "openai/gpt-5-mini"
            ]
        );
        assert_eq!(models[0].tokens, 300);
        assert_eq!(models.iter().map(|e| e.tokens).sum::<u64>(), 450);
        // Distinct colors per rank, tied to the chart's palette.
        assert_ne!(models[0].color, models[1].color);

        // By project: the catch-all folds under its fixed name.
        let projects = fold_breakdown(&reply, &summary, BreakdownTab::Projects, &theme);
        assert_eq!(projects.len(), 4);
        assert_eq!(projects[2].name, "Deleted chats");
        assert_eq!(projects[2].tokens, 60);
    }

    #[test]
    fn breakdown_percent_rounds_to_integer_shares() {
        assert_eq!(breakdown_percent(300, 450), 67);
        assert_eq!(breakdown_percent(150, 450), 33);
        assert_eq!(breakdown_percent(1, 450), 0, "tiny shares read as 0%");
        assert_eq!(breakdown_percent(450, 450), 100);
        assert_eq!(
            breakdown_percent(0, 0),
            0,
            "an empty card divides by nothing"
        );
    }

    #[gpui::test]
    fn the_breakdown_opens_on_by_model_in_engine_order(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, vec![], breakdown_reply());

        // Default tab: By model — legend rows in the engine's total-
        // descending order (300 then 100 then 50), the zero-total model
        // dropped, the donut riding beside them with the total in its hole.
        assert!(harness.present("usage-breakdown-row-0"));
        assert!(harness.present("usage-breakdown-row-1"));
        assert!(harness.present("usage-breakdown-row-2"));
        assert!(!harness.present("usage-breakdown-row-3"));
        let first = harness.bounds("usage-breakdown-row-0");
        let second = harness.bounds("usage-breakdown-row-1");
        assert!(first.origin.y < second.origin.y, "largest total first");
        assert!(harness.present("usage-breakdown-donut"));
        assert!(harness.present("usage-breakdown-total"));

        // Switching tabs swaps the entries in place.
        harness.click("usage-bd-tab-project");
        harness.pump();
        assert!(harness.present("usage-breakdown-row-0"));
        assert!(
            harness.present("usage-breakdown-row-2"),
            "deleted chats row"
        );
        assert!(harness.present("usage-breakdown-row-3"), "long path row");
    }

    #[gpui::test]
    fn the_breakdown_opens_folded_under_its_others_row(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, vec![], Scripted::Ok(folded_breakdown_reply_json()));

        // Closed by default: the four biggest models, then the Others row
        // that groups the rest — the tail itself is not on the page, and
        // no card in the reply is short enough to need a fold.
        assert!(harness.present("usage-breakdown-row-3"));
        assert!(harness.present("usage-breakdown-row-4"), "the Others row");
        assert!(!harness.present("usage-breakdown-child-0"));

        // The Others row IS the disclosure: the switch sits past the test
        // window's fold (1080px tall), so the viewport grows before any
        // click, then the click itemizes the tail under it.
        harness
            .visual
            .simulate_resize(gpui::size(px(1920.0), px(1600.0)));
        harness.pump();
        harness.click("usage-breakdown-row-4");
        harness.pump();
        assert!(harness.present("usage-breakdown-child-0"));
        assert!(harness.present("usage-breakdown-child-2"));
        let others = harness.bounds("usage-breakdown-row-4");
        let child = harness.bounds("usage-breakdown-child-0");
        assert!(
            others.origin.y < child.origin.y,
            "the tail itemizes under the row that groups it"
        );
        assert_eq!(
            child.origin.x, others.origin.x,
            "a grouped row is the list's own row shape, not a nested panel"
        );

        // Clicking the row again folds the tail back away.
        harness.click("usage-breakdown-row-4");
        harness.pump();
        assert!(!harness.present("usage-breakdown-child-0"));

        // Grouped or not, every row of the group speaks for the Others
        // slice: hovering an itemized row dims the ring to it.
        harness.click("usage-breakdown-row-4");
        harness.pump();
        let child = harness.bounds("usage-breakdown-child-1");
        harness
            .visual
            .simulate_mouse_move(child.center(), None, Default::default());
        harness.repaint();
        assert_eq!(
            harness.visual.read(|cx| harness.page.read(cx).hover_slice),
            Some(4),
            "an itemized row points at the slice that groups it"
        );
    }

    #[gpui::test]
    fn hovering_a_legend_row_highlights_its_slice(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, vec![], breakdown_reply());
        let hovered =
            |harness: &Harness<'_>| harness.visual.read(|cx| harness.page.read(cx).hover_slice);
        assert_eq!(hovered(&harness), None);

        // Hover row 0: its slice is the highlighted one.
        let row = harness.bounds("usage-breakdown-row-0");
        harness
            .visual
            .simulate_mouse_move(row.center(), None, Default::default());
        harness.repaint();
        assert_eq!(hovered(&harness), Some(0));

        // Moving to another row moves the highlight, even if the leave
        // events land out of order.
        let row = harness.bounds("usage-breakdown-row-1");
        harness
            .visual
            .simulate_mouse_move(row.center(), None, Default::default());
        harness.repaint();
        assert_eq!(hovered(&harness), Some(1));

        // Off the legend, the highlight clears.
        let header = harness.bounds("usage-refresh");
        harness
            .visual
            .simulate_mouse_move(header.center(), None, Default::default());
        harness.repaint();
        assert_eq!(hovered(&harness), None);

        // A tab switch clears a stale highlight: the index means a
        // different entry there.
        let row = harness.bounds("usage-breakdown-row-0");
        harness
            .visual
            .simulate_mouse_move(row.center(), None, Default::default());
        harness.repaint();
        assert_eq!(hovered(&harness), Some(0));
        harness.click("usage-bd-tab-project");
        harness.pump();
        assert_eq!(hovered(&harness), None);
    }

    #[gpui::test]
    fn a_long_path_truncates_instead_of_breaking_the_card(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, vec![], breakdown_reply());
        harness.click("usage-bd-tab-project");
        harness.pump();

        // The 300-character path stays on one line at the legend's own
        // width: the row truncates, never wraps, and never reflows its
        // siblings.
        let normal = harness.bounds("usage-breakdown-row-0");
        let long_row = harness.bounds("usage-breakdown-row-3");
        assert_eq!(long_row.size.width, normal.size.width);
        // A hairline's width apart at most: the last row carries no bottom
        // border, everything else is the same single-line row — the path
        // truncates, never wraps.
        assert!(
            (f32::from(long_row.size.height) - f32::from(normal.size.height)).abs() <= 1.0,
            "a long path truncates, never wraps: {:?} vs {:?}",
            long_row.size.height,
            normal.size.height
        );
        let second = harness.bounds("usage-breakdown-row-1");
        assert!(
            second.origin.y < long_row.origin.y,
            "total-descending order"
        );
    }
}
