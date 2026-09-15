//! The chat's token numbers, as composer chrome: a radial ring beside the
//! branch chip whose arc is the window occupancy, and the card its hover
//! opens — context occupancy on top, cumulative usage below, each drawn as a
//! bar instead of a line of text.
//!
//! The engine's `ChatUsage` frame is the only input: the totals, the
//! per-source split, and the occupancy all arrive decided, so this module
//! re-derives nothing. It only decides how what the frame carries is printed
//! — which is also why the two numbers stay in two labeled sections: the
//! occupancy is this request's claim on the window, the usage total is what
//! every request so far has spent, and adding them together is meaningless.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, Context, Div, Hsla, IntoElement, PathBuilder, Render, SharedString, Task, Window,
    canvas, div, point, prelude::*, px,
};

use crate::{
    motion,
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

/// Diameter of the ring, and of the square button that carries it — the
/// composer footer's chips are 20px tall, so the trigger matches the row.
const RING_DIAMETER: f32 = 14.0;
const RING_BUTTON: f32 = 20.0;
/// Stroke width of the ring, and the polyline resolution of its two arcs.
const RING_STROKE: f32 = 2.0;
const RING_SEGMENTS: f32 = 64.0;
/// Card width: one kind row (dot, name, count, share) with air to spare.
const CARD_WIDTH: f32 = 300.0;
/// The hover delay before the card appears — the usage tooltip's own delay
/// before this change.
const CARD_DELAY: Duration = Duration::from_millis(350);

/// How full the window the chat runs next would be, plus where the number
/// came from — the ring's arc and the card's first section.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Occupancy {
    pub(crate) tokens: u64,
    /// `None` (or zero) is an unknown window: a custom model, or a model no
    /// catalog row covers. The ring then has no fraction to draw.
    pub(crate) window: Option<u64>,
    /// The number is the History estimate rather than a provider report.
    pub(crate) estimated: bool,
}

impl Occupancy {
    /// The filled fraction of the track — 0.0 when the window is unknown,
    /// since an unknown window has no occupancy to claim.
    fn fraction(&self) -> f32 {
        match self.window.filter(|window| *window > 0) {
            Some(window) => (self.tokens as f64 / window as f64).clamp(0.0, 1.0) as f32,
            None => 0.0,
        }
    }
}

/// One source's share of the chat's gross usage.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct KindShare {
    /// The frame's kind key (`turn`, `subagent`, …) — the row's stable id.
    pub(crate) kind: String,
    pub(crate) label: String,
    pub(crate) tokens: u64,
    /// 0..=1 of the gross total; the rows and the stacked bar share it.
    pub(crate) share: f32,
}

/// The prompt side of the chat's spend: what the provider had to read, and
/// how much of it came back out of its cache. A cache write is prompt the
/// provider processed (and stored) for later — never a hit; an uncached input
/// token is a miss.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CacheStats {
    pub(crate) input: u64,
    pub(crate) read: u64,
    pub(crate) written: u64,
}

impl CacheStats {
    /// Every prompt token the chat's records report.
    fn prompt(&self) -> u64 {
        self.input + self.read + self.written
    }

    /// The share of the prompt served from cache. `None` when the chat has no
    /// prompt tokens at all — a rate over zero is not a number to print.
    fn hit_rate(&self) -> Option<f32> {
        let prompt = self.prompt();
        (prompt > 0).then(|| self.read as f32 / prompt as f32)
    }
}

/// Everything the ring and its card print, folded out of one frame.
pub(crate) struct CardModel {
    pub(crate) gross: u64,
    pub(crate) records: u64,
    pub(crate) occupancy: Occupancy,
    /// Sources by descending tokens — the bar's segment order and the rows'
    /// order are the same list.
    pub(crate) kinds: Vec<KindShare>,
    /// The prompt side, over the same records: what the cache did for this
    /// chat. A subset of `gross` (which also carries output), never a segment
    /// of the usage bar.
    pub(crate) cache: CacheStats,
}

/// Fold the frame into what the card prints. The share arithmetic is the only
/// thing this module computes: the tokens themselves are the engine's.
pub(crate) fn card_model(usage: &ChatUsage) -> CardModel {
    let mut kinds: Vec<KindShare> = usage
        .by_kind
        .iter()
        .map(|(kind, sum)| {
            let tokens = sum.input + sum.output + sum.cache_read + sum.cache_write;
            KindShare {
                kind: kind.clone(),
                label: kind_label(kind),
                tokens,
                share: if usage.gross == 0 {
                    0.0
                } else {
                    tokens as f32 / usage.gross as f32
                },
            }
        })
        .collect();
    // Descending by size, ties broken by name so the order is stable across
    // renders (BTreeMap order alone would put the smallest first).
    kinds.sort_by(|a, b| b.tokens.cmp(&a.tokens).then_with(|| a.label.cmp(&b.label)));
    CardModel {
        gross: usage.gross,
        records: usage.record_count,
        occupancy: Occupancy {
            tokens: usage.occupancy.tokens,
            window: usage.occupancy.context_window,
            estimated: usage.occupancy.estimated,
        },
        cache: CacheStats {
            input: usage.by_kind.values().map(|sum| sum.input).sum(),
            read: usage.by_kind.values().map(|sum| sum.cache_read).sum(),
            written: usage.by_kind.values().map(|sum| sum.cache_write).sum(),
        },
        kinds,
    }
}

/// A source kind as the card names it. An unknown key — a newer engine's new
/// source — prints as the key itself, never dropped.
fn kind_label(kind: &str) -> String {
    match kind {
        "turn" => "Turns".to_string(),
        "subagent" => "Subagents".to_string(),
        "compaction" => "Compaction".to_string(),
        "auto-review" => "Auto-review".to_string(),
        "title" => "Titles".to_string(),
        other => other.to_string(),
    }
}

/// The occupancy as the card's first section prints it: `7.7k / 272k · 2.8%`,
/// `≈`-prefixed per number while the figure is the History estimate, and
/// absolute tokens when the window is unknown (no percentage then — a guess
/// divided by an unknown is not a number to print).
pub(crate) fn occupancy_value(occupancy: &Occupancy) -> String {
    let approx = if occupancy.estimated { "≈" } else { "" };
    match occupancy.window.filter(|window| *window > 0) {
        Some(window) => format!(
            "{approx}{} / {} · {approx}{}",
            compact_tokens(occupancy.tokens),
            compact_tokens(window),
            percent(occupancy.tokens as f64 / window as f64),
        ),
        None => format!(
            "{approx}{} · window unknown",
            compact_tokens(occupancy.tokens)
        ),
    }
}

/// The usage section's value: the gross total and the records behind it.
fn usage_value(model: &CardModel) -> String {
    format!(
        "{} · {} {}",
        compact_tokens(model.gross),
        model.records,
        if model.records == 1 {
            "record"
        } else {
            "records"
        }
    )
}

/// A fraction as a percentage with one decimal, a trailing `.0` dropped —
/// the convention [`compact_tokens`] already follows, so a clean 4 reads
/// `4%`, never `4.0%`.
fn percent(fraction: f64) -> String {
    let value = format!("{:.1}", fraction * 100.0);
    format!("{}%", value.trim_end_matches(".0"))
}

/// The one line of prose the card keeps: where the occupancy number came
/// from, and what is missing when the window is unknown.
fn source_note(model: &CardModel) -> Option<&'static str> {
    match (model.occupancy.estimated, model.occupancy.window) {
        (true, None) => Some("Estimated from history; the window is unknown."),
        (true, Some(_)) => Some("Estimated from history until the next reply reports usage."),
        (false, None) => Some("Context window size is unknown."),
        (false, Some(_)) => None,
    }
}

/// The ring beside the branch chip, and the card its hover opens. `None`
/// until the chat's frame arrives — an unselected chat, or an engine that
/// never billed it.
pub(crate) fn ring(usage: Option<&ChatUsage>, theme: &Theme) -> Option<AnyElement> {
    let model = Arc::new(card_model(usage?));
    let card = Arc::clone(&model);
    Some(
        div()
            .id("chat-usage-ring")
            .debug_selector(|| "chat-usage-ring".into())
            .flex_none()
            .size(px(RING_BUTTON))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(motion::hover_blend(
                "chat-usage-ring",
                gpui::transparent_black(),
                theme.element_hover,
            ))
            .on_hover(motion::hover_listener("chat-usage-ring"))
            .tooltip(move |_, cx| cx.new(|_| UsageCard(Arc::clone(&card))).into())
            .tooltip_show_delay(CARD_DELAY)
            .child(occupancy_ring(model.occupancy.fraction(), theme))
            .into_any_element(),
    )
}

/// The ring itself: a faint track plus the occupied arc growing clockwise
/// from 12 o'clock. gpui paths have no arc primitive, so both are stroked
/// polylines — the same technique as `loaders::upload_progress_ring`.
fn occupancy_ring(fraction: f32, theme: &Theme) -> AnyElement {
    let track = theme.ink(0.15);
    let arc = theme.accent;
    let ring = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let center = bounds.center();
            let radius = RING_DIAMETER / 2.0 - RING_STROKE;
            let mut paint_arc = |sweep: f32, color: Hsla| {
                if sweep <= 0.0 {
                    return;
                }
                let steps = ((RING_SEGMENTS * sweep).ceil() as usize).max(2);
                let at = |i: usize| {
                    let theta = -std::f32::consts::FRAC_PI_2
                        + std::f32::consts::TAU * sweep * (i as f32 / steps as f32);
                    point(
                        center.x + px(radius * theta.cos()),
                        center.y + px(radius * theta.sin()),
                    )
                };
                let mut builder = PathBuilder::stroke(px(RING_STROKE));
                builder.move_to(at(0));
                for i in 1..=steps {
                    builder.line_to(at(i));
                }
                if let Ok(path) = builder.build() {
                    window.paint_path(path, color);
                }
            };
            paint_arc(1.0, track);
            paint_arc(fraction.clamp(0.0, 1.0), arc);
        },
    )
    .absolute()
    .inset_0();
    div()
        .relative()
        .size(px(RING_DIAMETER))
        .child(ring)
        .into_any_element()
}

/// The hover card: two labeled sections, each a header plus a bar, and — for
/// usage — one row per source.
struct UsageCard(Arc<CardModel>);

impl Render for UsageCard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let model = self.0.as_ref();
        let mut card = crate::popover::popover_card(&theme)
            .debug_selector(|| "chat-usage-card".into())
            .w(px(CARD_WIDTH))
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(7.0))
            .text_size(crate::typography::ui_rems(12.0))
            .child(section_header(
                "Context",
                occupancy_value(&model.occupancy),
                &theme,
            ))
            .child(bar(
                "context",
                model.occupancy.fraction(),
                theme.accent,
                &theme,
            ));
        if !model.kinds.is_empty() {
            card = card
                .child(div().h(px(2.0)))
                .child(section_header("Usage", usage_value(model), &theme))
                .child(usage_bar(model, &theme))
                .children(
                    model
                        .kinds
                        .iter()
                        .enumerate()
                        .map(|(rank, share)| kind_row(rank, share, &theme)),
                );
        }
        if let Some(hit_rate) = model.cache.hit_rate() {
            card = card
                .child(div().h(px(2.0)))
                .child(section_header(
                    "Cache hit",
                    percent(hit_rate as f64),
                    &theme,
                ))
                .child(bar("cache", hit_rate, theme.accent, &theme))
                .child(footnote(cache_note(&model.cache), &theme));
        }
        if let Some(note) = source_note(model) {
            card = card.child(footnote(note.to_string(), &theme));
        }
        crate::frost::frosted(crate::popover::CARD_RADIUS, crate::frost::MENU_BLUR, card)
    }
}

/// A section's label (left, bright) and its value (right, muted).
fn section_header(label: &str, value: String, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .child(
            div()
                .flex_none()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(SharedString::from(label.to_string())),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_color(theme.text_muted)
                .child(SharedString::from(value)),
        )
}

/// The shared track: a rounded groove the fills ride in (the settings
/// previews' bar geometry).
fn track(theme: &Theme) -> Div {
    div()
        .h(px(5.0))
        .w_full()
        .rounded(px(3.0))
        .overflow_hidden()
        .bg(theme.ink(0.10))
}

/// A bar: one rounded track with a single fill. `name` keys the two debug
/// selectors the geometry test measures the fraction through.
fn bar(name: &'static str, fraction: f32, fill: Hsla, theme: &Theme) -> Div {
    track(theme)
        .debug_selector(move || format!("usage-{name}-track"))
        .child(
            div()
                .debug_selector(move || format!("usage-{name}-fill"))
                .h_full()
                .w(gpui::relative(fraction.clamp(0.0, 1.0)))
                .rounded(px(3.0))
                .bg(fill),
        )
}

/// The usage bar: one segment per source, in the rows' own order and colors.
/// The last segment takes the remainder so rounding never leaves a sliver of
/// bare track at the end.
fn usage_bar(model: &CardModel, theme: &Theme) -> Div {
    let last = model.kinds.len().saturating_sub(1);
    let mut bar = track(theme)
        .debug_selector(|| "usage-bar".into())
        .flex()
        .flex_row();
    for (rank, share) in model.kinds.iter().enumerate() {
        let kind = share.kind.clone();
        let segment = div()
            .debug_selector(move || format!("usage-segment-{kind}"))
            .h_full()
            .bg(kind_color(rank, theme));
        bar = bar.child(if rank == last {
            segment.flex_1()
        } else {
            segment.w(gpui::relative(share.share.clamp(0.0, 1.0)))
        });
    }
    bar
}

/// One source's row: dot, name, exact count, share of the total.
fn kind_row(rank: usize, share: &KindShare, theme: &Theme) -> Div {
    let kind = share.kind.clone();
    div()
        .debug_selector(move || format!("usage-kind-{kind}"))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .flex_none()
                .size(px(6.0))
                .rounded_full()
                .bg(kind_color(rank, theme)),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_color(theme.text_muted)
                .child(SharedString::from(share.label.clone())),
        )
        .child(div().flex_1())
        .child(
            div()
                .flex_none()
                .text_color(theme.text_faint)
                .child(SharedString::from(compact_tokens(share.tokens))),
        )
        .child(
            div()
                .flex_none()
                .w(px(44.0))
                .text_right()
                .text_color(theme.text)
                .child(SharedString::from(percent(share.share as f64))),
        )
}

/// A source's color: one hue dimmed by rank, so a five-source breakdown stays
/// legible without inventing a palette. Segment and dot take the same color,
/// which is what ties the bar to the rows.
fn kind_color(rank: usize, theme: &Theme) -> Hsla {
    theme.accent.opacity((1.0 - rank as f32 * 0.16).max(0.35))
}

/// The three prompt counts the hit rate divides, in the frame's own
/// vocabulary so the percentage is auditable against them. A zero write count
/// stays out: most providers never report cache writes, and a permanent
/// "0 written" is noise rather than data.
fn cache_note(cache: &CacheStats) -> String {
    let mut note = format!(
        "{} input · {} read",
        compact_tokens(cache.input),
        compact_tokens(cache.read)
    );
    if cache.written > 0 {
        note.push_str(&format!(" · {} written", compact_tokens(cache.written)));
    }
    note
}

/// The card's small print: the cache counts and the occupancy's source.
fn footnote(text: String, theme: &Theme) -> Div {
    div()
        .text_size(crate::typography::ui_rems(10.0))
        .text_color(theme.text_faint)
        .child(SharedString::from(text))
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
    use std::sync::Mutex;

    fn frame(gross: u64, window: Option<u64>, estimated: bool) -> Value {
        json!({"gross":gross,"byKind":{"turn":{"input":40000,"output":2000,"cacheRead":3000,"cacheWrite":500}},
            "recordCount":3,"occupancy":{"tokens":40000,"contextWindow":window,"estimated":estimated}})
    }

    fn decode(value: Value) -> ChatUsage {
        WatchCoordinator::decode(value).unwrap()
    }

    /// One source's row, by the key the frame carries it under.
    fn row<'a>(model: &'a CardModel, kind: &str) -> &'a KindShare {
        model
            .kinds
            .iter()
            .find(|share| share.kind == kind)
            .unwrap_or_else(|| panic!("no {kind} row"))
    }

    #[test]
    fn the_card_splits_the_gross_by_kind_largest_first() {
        let model = card_model(&decode(json!({
            "gross": 1000,
            "byKind": {
                "title": {"input": 100, "output": 100},
                "turn": {"input": 400, "output": 100, "cacheRead": 300}
            },
            "recordCount": 3,
            "occupancy": {"tokens": 700, "contextWindow": 1000, "estimated": false}
        })));

        // The four token fields per kind, summed and shared over the gross —
        // the row's own count and the bar's segment width.
        assert_eq!(row(&model, "turn").tokens, 800);
        assert_eq!(row(&model, "turn").label, "Turns");
        assert!((row(&model, "turn").share - 0.8).abs() < 1e-6);
        assert_eq!(row(&model, "title").tokens, 200);
        assert_eq!(row(&model, "title").label, "Titles");
        // Largest first, regardless of the frame's key order.
        assert_eq!(model.kinds[0].kind, "turn");
        assert_eq!(model.kinds[1].kind, "title");
        // Prompt-side tokens: uncached input, reads, writes — the cache
        // section's three numbers and its rate.
        assert_eq!(model.cache.input, 500);
        assert_eq!(model.cache.read, 300);
        assert_eq!(model.cache.written, 0);
        assert_eq!(model.records, 3);
        assert_eq!(occupancy_value(&model.occupancy), "700 / 1k · 70%");
    }

    #[test]
    fn the_hit_rate_divides_reads_by_every_prompt_token() {
        // A write is prompt the provider processed and stored, so it widens
        // the denominator: 300 reads of 400 input + 300 read + 300 written.
        let cache = CacheStats {
            input: 400,
            read: 300,
            written: 300,
        };
        assert_eq!(cache.prompt(), 1000);
        assert_eq!(percent(cache.hit_rate().unwrap() as f64), "30%");
        assert_eq!(cache_note(&cache), "400 input · 300 read · 300 written");

        // A provider that never reports writes keeps them out of the print.
        let reads_only = CacheStats {
            input: 200,
            read: 800,
            written: 0,
        };
        assert_eq!(percent(reads_only.hit_rate().unwrap() as f64), "80%");
        assert_eq!(cache_note(&reads_only), "200 input · 800 read");

        // No prompt tokens at all: no rate to print, and no section.
        assert_eq!(CacheStats::default().hit_rate(), None);
    }

    #[test]
    fn the_frame_supplies_the_cache_numbers() {
        let model = card_model(&decode(json!({
            "gross": 1050,
            "byKind": {
                "turn": {"input": 400, "output": 100, "cacheRead": 300},
                "title": {"input": 100, "output": 100, "cacheWrite": 50}
            },
            "occupancy": {"tokens": 700, "contextWindow": 1000}
        })));
        assert_eq!(model.cache.input, 500);
        assert_eq!(model.cache.read, 300);
        assert_eq!(model.cache.written, 50);
        // Every kind contributes to the cache totals, not just the turns.
        assert_eq!(percent(model.cache.hit_rate().unwrap() as f64), "35.3%");
    }

    #[test]
    fn an_unknown_kind_prints_as_its_own_key() {
        let model = card_model(&decode(json!({
            "gross": 10,
            "byKind": {"goal-verifier": {"input": 10}},
            "occupancy": {"tokens": 10, "contextWindow": 100}
        })));
        assert_eq!(row(&model, "goal-verifier").label, "goal-verifier");
    }

    #[test]
    fn occupancy_names_its_source_and_a_withheld_window() {
        let measured = Occupancy {
            tokens: 7_651,
            window: Some(272_000),
            estimated: false,
        };
        assert_eq!(occupancy_value(&measured), "7.7k / 272k · 2.8%");
        assert!((measured.fraction() - 0.0281).abs() < 0.0001);

        // The estimate declares itself, per number.
        let estimated = Occupancy {
            estimated: true,
            ..measured
        };
        assert_eq!(occupancy_value(&estimated), "≈7.7k / 272k · ≈2.8%");

        // A custom model's window is withheld: absolute tokens, no share, and
        // an empty ring rather than a fabricated one.
        let unknown = Occupancy {
            window: None,
            ..measured
        };
        assert_eq!(occupancy_value(&unknown), "7.7k · window unknown");
        assert_eq!(unknown.fraction(), 0.0);
    }

    #[test]
    fn a_blank_frame_yields_no_shares_and_no_nan() {
        let model = card_model(&ChatUsage::default());
        assert_eq!(model.gross, 0);
        assert!(model.kinds.is_empty());
        assert_eq!(model.cache, CacheStats::default());
        assert_eq!(occupancy_value(&model.occupancy), "0 · window unknown");
    }

    #[test]
    fn the_source_note_covers_every_state_of_the_two_flags() {
        let of = |estimated, window| {
            source_note(&card_model(&ChatUsage {
                occupancy: holt_proto::ChatOccupancy {
                    tokens: 1,
                    context_window: window,
                    estimated,
                },
                ..Default::default()
            }))
        };
        assert_eq!(of(false, Some(1_000)), None);
        assert!(of(false, None).unwrap().contains("unknown"));
        assert!(of(true, Some(1_000)).unwrap().contains("Estimated"));
        assert!(of(true, None).unwrap().contains("Estimated"));
    }

    /// The ring and the card both paint, at the geometry the bars claim:
    /// the arc is stroked in a paint pass and the bars only exist on a layout
    /// pass, so this is where a bad fraction, an unset theme global, or a bar
    /// that never measured would surface. The two bars are the card's whole
    /// point, so their widths are checked against the shares they came from.
    #[gpui::test]
    fn the_ring_and_its_card_paint_their_bars(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        // Turn 800, Titles 200 of a 1000 gross, against a 1M window holding
        // 40k: a bar of 80% and one of 20%, and an occupancy fill of 4%.
        // 200 of the prompt's 920 tokens came from cache: a 21.7% hit rate.
        let usage = decode(json!({
            "gross": 1000,
            "byKind": {
                "turn": {"input": 560, "output": 40, "cacheRead": 200},
                "title": {"input": 160, "output": 40}
            },
            "recordCount": 3,
            "occupancy": {"tokens": 40000, "contextWindow": 1000000, "estimated": false}
        }));
        let window = cx.add_window(|_, _| UsagePreview(Some(usage)));
        let mut visual = gpui::VisualTestContext::from_window(*window, cx);

        let ring = visual.debug_bounds("chat-usage-ring").unwrap();
        assert_eq!((ring.size.width, ring.size.height), (px(20.0), px(20.0)));
        let card = visual.debug_bounds("chat-usage-card").unwrap();
        assert_eq!(card.size.width, px(CARD_WIDTH));

        let track = visual.debug_bounds("usage-context-track").unwrap();
        let fill = visual.debug_bounds("usage-context-fill").unwrap();
        assert_eq!(track.size.height, px(5.0));
        assert_close(fill.size.width / track.size.width, 0.04, "occupancy fill");

        let bar = visual.debug_bounds("usage-bar").unwrap();
        let turn = visual.debug_bounds("usage-segment-turn").unwrap();
        let title = visual.debug_bounds("usage-segment-title").unwrap();
        assert_close(turn.size.width / bar.size.width, 0.8, "turn segment");
        // The last segment takes the remainder: 20% here, and never a sliver
        // of bare track left over from float rounding.
        assert_close(title.size.width / bar.size.width, 0.2, "title segment");

        let cache_track = visual.debug_bounds("usage-cache-track").unwrap();
        let cache_fill = visual.debug_bounds("usage-cache-fill").unwrap();
        assert_close(
            cache_fill.size.width / cache_track.size.width,
            200.0 / 920.0,
            "cache hit fill",
        );

        assert!(visual.debug_bounds("usage-kind-turn").is_some());
        assert!(visual.debug_bounds("usage-kind-title").is_some());
    }

    /// A fraction of a measured width, within a pixel of layout rounding.
    fn assert_close(measured: f32, expected: f32, what: &str) {
        assert!(
            (measured - expected).abs() < 0.02,
            "{what}: {measured} != {expected}"
        );
    }

    /// A chat with no frame yet (a fresh chat, an older engine) renders no
    /// trigger at all — never an empty ring claiming zero occupancy.
    #[gpui::test]
    fn a_chat_without_a_frame_has_no_ring(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let window = cx.add_window(|_, _| UsagePreview(None));
        let mut visual = gpui::VisualTestContext::from_window(*window, cx);
        assert!(visual.debug_bounds("chat-usage-ring").is_none());
    }

    /// The ring beside the card, both mounted in a real window.
    struct UsagePreview(Option<ChatUsage>);

    impl Render for UsagePreview {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = Theme::of(cx).clone();
            let card = self
                .0
                .as_ref()
                .map(|usage| cx.new(|_| UsageCard(Arc::new(card_model(usage)))));
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .children(ring(self.0.as_ref(), &theme))
                .children(card)
        }
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

    fn displayed_occupancy(state: &Entity<AppState>, cx: &TestAppContext) -> Option<String> {
        cx.read(|cx| {
            state
                .read(cx)
                .chat_usage
                .as_ref()
                .map(|usage| occupancy_value(&card_model(usage).occupancy))
        })
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
            displayed_occupancy(&state, cx).as_deref(),
            Some("≈40k / 1M · ≈4%")
        );
        engine.watches.lock().unwrap()[0]
            .1
            .unbounded_send(frame(824000, None, false))
            .unwrap();
        pump(&runtime, cx);
        // The unknown window drops the percentage and the ring's arc with it.
        assert_eq!(
            displayed_occupancy(&state, cx).as_deref(),
            Some("40k · window unknown")
        );

        state.update(cx, |state, cx| state.select_chat(Some("b".into()), cx));
        assert!(displayed_occupancy(&state, cx).is_none());
        pump(&runtime, cx);
        assert!(engine.watches.lock().unwrap()[0].1.is_closed());
        assert_eq!(engine.watches.lock().unwrap()[1].0, "b");
        assert_eq!(
            displayed_occupancy(&state, cx).as_deref(),
            Some("≈40k / 1M · ≈4%")
        );

        state.update(cx, |state, cx| {
            state.select_chat(Some("old-engine".into()), cx)
        });
        pump(&runtime, cx);
        assert!(displayed_occupancy(&state, cx).is_none());
        state.update(cx, |state, cx| state.select_chat(None, cx));
        pump(&runtime, cx);
        assert!(displayed_occupancy(&state, cx).is_none());
    }

    #[gpui::test]
    fn scripted_engine_totals_reach_the_selected_ring(cx: &mut TestAppContext) {
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
            let model = card_model(usage);
            // Pinned end to end: gross 700 + 70 + 7 + 3 with one record, and
            // the occupancy the main run's own request input plus both cache
            // fields (700 + 7 + 3).
            assert_eq!(model.gross, 780);
            assert_eq!(model.records, 1);
            assert_eq!(model.occupancy.tokens, 710);
            // Prompt 700 + 7 read + 3 written; only the reads hit.
            assert_eq!(model.cache.prompt(), 710);
            assert_eq!(model.cache.read, 7);
            assert_eq!(model.cache.written, 3);
            let turn = row(&model, "turn");
            assert_eq!(turn.tokens, 780);
            assert!((turn.share - 1.0).abs() < 1e-6);
            // Run acceptance stores the request's model on the chat, so a
            // settled queue divides by that model's catalog window — 272k for
            // this one. The UI's job is to print what the frame carries; the
            // catalog number itself is the engine's to know.
            let window = model.occupancy.window.unwrap();
            assert!(window > 0);
            let shown = occupancy_value(&model.occupancy);
            assert!(
                shown.starts_with("710 / ") && shown.ends_with('%'),
                "{shown}"
            );
            assert_eq!(usage_value(&model), "780 · 1 record");
        });
    }
}
