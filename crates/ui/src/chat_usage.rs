//! The chat's token numbers, as composer chrome: a radial ring beside the
//! branch chip whose arc is the window occupancy, and the card its click
//! pins above the composer — occupancy on top, the context breakdown as a
//! stacked bar and one row per category below.
//!
//! The card's rows open the right-pane report: the latest request's full
//! context (system prompt, rules, tools, skills, MCP servers, the
//! conversation grouped by turns), fed by the same frame's context
//! snapshot. The engine's `ChatUsage` frame is the only input: the headline
//! numbers — gross, per-kind sums, occupancy, per-entry estimates — are the
//! engine's, never recomputed here. The one re-derivation is the report's
//! rule rows: the frame carries the System Prompt as one entry, so each
//! rule section's share is re-estimated from that same text to split what
//! the engine reports whole.

use std::collections::HashSet;
use std::sync::Arc;

use gpui::{
    AnyElement, Context, Div, Entity, Hsla, IntoElement, PathBuilder, Render, ScrollHandle,
    SharedString, Subscription, Task, Window, canvas, div, point, prelude::*, px,
};

use crate::{
    i18n::t,
    motion,
    pickers::Pickers,
    state::{AppState, EngineHandle},
    theme::Theme,
    token_display::compact_tokens,
    watch_coordinator::WatchCoordinator,
};
use holt_proto::ChatUsage;
use holt_rpc::{RpcError, methods};

#[cfg(test)]
#[path = "../../engine/tests/all/common/mod.rs"]
mod engine_fixture;

/// Diameter of the ring, and of the square button that carries it — the
/// composer footer's chips are 20px tall, so the trigger matches the row.
const RING_DIAMETER: f32 = 14.0;
const RING_BUTTON: f32 = 20.0;
/// Stroke width of the ring, and the polyline resolution of its two arcs.
const RING_STROKE: f32 = 2.0;
const RING_SEGMENTS: f32 = 64.0;
/// Card width: one context-category row (dot, name, tokens) with air to
/// spare; the composer feeds the popover its pill width and falls back to
/// this until the first width pass.
pub(crate) const CARD_WIDTH: f32 = 340.0;

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

/// The frame's occupancy, folded for the ring and the card's header — the
/// only arithmetic this module does; the tokens themselves are the
/// engine's.
pub(crate) fn occupancy_of(usage: &ChatUsage) -> Occupancy {
    Occupancy {
        tokens: usage.occupancy.tokens,
        window: usage.occupancy.context_window,
        estimated: usage.occupancy.estimated,
    }
}

/// A fraction as a percentage with one decimal, a trailing `.0` dropped —
/// the convention [`compact_tokens`] already follows, so a clean 4 reads
/// `4%`, never `4.0%`.
pub(crate) fn percent(fraction: f64) -> String {
    let value = format!("{:.1}", fraction * 100.0);
    format!("{}%", value.trim_end_matches(".0"))
}

/// The one line of prose the card keeps: where the occupancy number came
/// from, and what is missing when the window is unknown.
fn source_note(occupancy: &Occupancy) -> Option<std::borrow::Cow<'static, str>> {
    match (occupancy.estimated, occupancy.window) {
        (true, None) => Some(t!("conversation_transcript.usage.note.estimated_unknown")),
        (true, Some(_)) => Some(t!("conversation_transcript.usage.note.estimated")),
        (false, None) => Some(t!("conversation_transcript.usage.note.window_unknown")),
        (false, Some(_)) => None,
    }
}

/// The ring beside the branch chip. A click pins the card above the composer;
/// it is deliberately not a hover tooltip, so the report action and the
/// numbers remain reachable with the pointer. `open` comes from the caller:
/// the footer renders during a `Pickers` update, so reading that entity here
/// would be a re-entrant GPUI borrow and panic at startup.
pub(crate) fn ring(
    usage: Option<&ChatUsage>,
    open: bool,
    theme: &Theme,
    pickers: &Entity<Pickers>,
) -> Option<gpui::Stateful<Div>> {
    let occupancy = occupancy_of(usage?);
    let pressed = pickers.clone();
    let clicked = pickers.clone();
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
            .bg(if open {
                theme.element_hover
            } else {
                motion::hover_blend(
                    "chat-usage-ring",
                    gpui::transparent_black(),
                    theme.element_hover,
                )
            })
            .on_hover(motion::hover_listener("chat-usage-ring"))
            .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                pressed.update(cx, |this, _| this.note_context_usage_press())
            })
            .on_click(move |_, window, cx| {
                clicked.update(cx, |this, cx| this.toggle_context_usage(window, cx))
            })
            .child(occupancy_ring(occupancy.fraction(), theme)),
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

/// The popover's context breakdown: occupancy percentage and token ratio over
/// a multi-segment stacked bar, followed by one row per context category.
pub(crate) fn card_content(usage: &ChatUsage, theme: &Theme) -> Div {
    let occupancy = occupancy_of(usage);
    let categories = context_categories(usage);
    let mut card = div()
        .debug_selector(|| "chat-usage-card".into())
        .flex()
        .flex_col()
        .gap(px(10.0))
        .text_size(crate::typography::ui_rems(12.0))
        .child(card_occupancy_header(&occupancy, theme))
        .child(context_stacked_bar(
            occupancy.fraction(),
            &categories,
            theme,
        ))
        .child(
            div().flex().flex_col().gap(px(7.0)).pt(px(2.0)).children(
                categories
                    .iter()
                    .map(|category| category_row(category, theme)),
            ),
        );
    if let Some(note) = source_note(&occupancy) {
        card = card.child(footnote(note.into_owned(), theme));
    }
    card
}

/// Format an integer token count with thousands commas (`1,729`, `4,262`).
pub(crate) fn comma_tokens(tokens: u64) -> String {
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

fn estimate_utf16_tokens(text: &str) -> u64 {
    (text.encode_utf16().count() as f64 / 4.0).ceil() as u64
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExtractedRule {
    pub(crate) path: String,
    pub(crate) content: String,
    pub(crate) estimated_tokens: u64,
    pub(crate) is_user_rule: bool,
}

/// A rule is user-level when it is NOT on the project walk: every project
/// `AGENTS.md` sits on the ancestor chain of the chat's cwd, and personal
/// rules (`~/.claude/…`) do not. Both inputs are engine-side strings (the
/// walked rule paths and the chat's folder), so a remote engine's paths
/// classify correctly — the env-var sniff this replaces judged the UI
/// machine's `HOME` instead.
fn is_user_rule_path(path: &str, cwd: Option<&str>) -> bool {
    let Some(cwd) = cwd else {
        // No folder to walk against: keep the project grouping, matching
        // the walk-less chat the engine itself runs.
        return false;
    };
    let Some(rule_dir) = std::path::Path::new(path).parent() else {
        return true;
    };
    let mut dir = Some(std::path::Path::new(cwd));
    while let Some(current) = dir {
        if current == rule_dir {
            return false;
        }
        dir = current.parent();
    }
    true
}

/// Split the composite System Prompt into its base instructions, any
/// `Project instructions from <path>:` rule sections (`AGENTS.md`), and the
/// trailing `<available_skills>` catalog block so the UI does not double-count
/// rules or skills inside the System prompt row. `cwd` classifies each rule
/// as project (on the walk) or user-level (see [`is_user_rule_path`]).
pub(crate) fn split_system_prompt(
    prompt: &str,
    fallback_tokens: u64,
    cwd: Option<&str>,
) -> (u64, Vec<ExtractedRule>) {
    const SKILLS_MARKER: &str = "\n\nThe following skills provide specialized instructions";
    const SKILLS_TAG_MARKER: &str = "\n\n<available_skills>";
    const RULE_MARKER: &str = "\n\nProject instructions from ";

    let without_skills = prompt
        .find(SKILLS_MARKER)
        .or_else(|| prompt.find(SKILLS_TAG_MARKER))
        .or_else(|| prompt.find("<available_skills>"))
        .map_or(prompt, |index| &prompt[..index]);

    let has_skills = without_skills.len() < prompt.len();
    let mut parts = without_skills.split(RULE_MARKER);
    let base = parts.next().unwrap_or(without_skills);
    let mut rules = Vec::new();
    for segment in parts {
        let (path, body) = segment
            .split_once(":\n")
            .map_or(("", segment), |(path, body)| (path.trim(), body.trim()));
        let full_chunk = format!("{RULE_MARKER}{segment}");
        rules.push(ExtractedRule {
            is_user_rule: is_user_rule_path(path, cwd),
            path: path.to_string(),
            content: body.to_string(),
            estimated_tokens: estimate_utf16_tokens(&full_chunk),
        });
    }

    let base_tokens = if rules.is_empty() && !has_skills {
        fallback_tokens
    } else {
        estimate_utf16_tokens(base)
    };
    (base_tokens, rules)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ContextCategoryShare {
    pub(crate) key: &'static str,
    pub(crate) label: String,
    pub(crate) tokens: u64,
    pub(crate) color: Hsla,
}

pub(crate) fn category_color(key: &str) -> Hsla {
    match key {
        "system" => gpui::rgb(0xd2b83f).into(),
        "tools" => gpui::rgb(0x6366b3).into(),
        "rules" => gpui::rgb(0xd99a3d).into(),
        "skills" => gpui::rgb(0x5aa6c8).into(),
        "mcp" => gpui::rgb(0xb65e32).into(),
        "conversation" => gpui::rgb(0x249b78).into(),
        _ => gpui::rgb(0x9ca3af).into(),
    }
}

pub(crate) fn context_categories(usage: &ChatUsage) -> Vec<ContextCategoryShare> {
    use holt_proto::ChatContextEntryKind as Kind;

    let mut system_tokens = 0u64;
    let mut tools_tokens = 0u64;
    let mut rules_tokens = 0u64;
    let mut skills_tokens = 0u64;
    let mut mcp_tokens = 0u64;
    let mut conversation_tokens = 0u64;

    if let Some(snapshot) = usage.context.as_ref() {
        for entry in &snapshot.entries {
            match entry.kind {
                Kind::SystemPrompt => {
                    // The category split only sums rule tokens — the
                    // user/project classification is the report's concern,
                    // not the bar's.
                    let (base, rules) = split_system_prompt(
                        &entry.content,
                        entry.estimated_tokens,
                        snapshot.cwd.as_deref(),
                    );
                    system_tokens += base;
                    rules_tokens += rules.iter().map(|rule| rule.estimated_tokens).sum::<u64>();
                }
                Kind::ToolDefinitions | Kind::Tool => {
                    tools_tokens += entry.estimated_tokens;
                }
                Kind::Skills | Kind::Skill => {
                    skills_tokens += entry.estimated_tokens;
                }
                Kind::McpServer => {
                    mcp_tokens += entry.estimated_tokens;
                }
                Kind::UserMessage | Kind::AssistantMessage | Kind::ToolResult => {
                    conversation_tokens += entry.estimated_tokens;
                }
                Kind::Unknown => {
                    // A newer engine's entry kind: count it as prompt-side
                    // chrome rather than dropping its tokens from the bar.
                    system_tokens += entry.estimated_tokens;
                }
            }
        }
    } else {
        conversation_tokens = usage.occupancy.tokens;
    }

    vec![
        ContextCategoryShare {
            key: "system",
            label: t!("conversation_transcript.usage.category.system_prompt").into_owned(),
            tokens: system_tokens,
            color: category_color("system"),
        },
        ContextCategoryShare {
            key: "tools",
            label: t!("conversation_transcript.usage.category.tool_definitions").into_owned(),
            tokens: tools_tokens,
            color: category_color("tools"),
        },
        ContextCategoryShare {
            key: "rules",
            label: t!("conversation_transcript.usage.category.rules").into_owned(),
            tokens: rules_tokens,
            color: category_color("rules"),
        },
        ContextCategoryShare {
            key: "skills",
            label: t!("conversation_transcript.usage.category.skills").into_owned(),
            tokens: skills_tokens,
            color: category_color("skills"),
        },
        ContextCategoryShare {
            key: "mcp",
            label: t!("conversation_transcript.usage.category.mcp").into_owned(),
            tokens: mcp_tokens,
            color: category_color("mcp"),
        },
        ContextCategoryShare {
            key: "conversation",
            label: t!("conversation_transcript.usage.category.conversation").into_owned(),
            tokens: conversation_tokens,
            color: category_color("conversation"),
        },
    ]
}

fn card_occupancy_header(occupancy: &Occupancy, theme: &Theme) -> Div {
    let left = match occupancy.window.filter(|window| *window > 0) {
        Some(window) => {
            let pct = percent(occupancy.tokens as f64 / window as f64);
            t!("conversation_transcript.usage.full", percent = pct).into_owned()
        }
        None => t!("conversation_transcript.usage.window_unknown")
            .trim_start_matches("· ")
            .to_string(),
    };
    // The `≈` mark follows the frame's `estimated` flag exactly as the report
    // summary's does — a measured occupancy prints bare numbers.
    let mark = if occupancy.estimated { "≈" } else { "" };
    let right = match occupancy.window.filter(|window| *window > 0) {
        Some(window) => t!(
            "conversation_transcript.usage.tokens_of_window",
            tokens = format!("{mark}{}", compact_tokens(occupancy.tokens)),
            window = compact_tokens(window)
        )
        .into_owned(),
        None => t!(
            "conversation_transcript.usage.tokens_only",
            tokens = format!("{mark}{}", compact_tokens(occupancy.tokens))
        )
        .into_owned(),
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .child(
            div()
                .flex_none()
                .text_color(theme.text_muted)
                .child(SharedString::from(left)),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_color(theme.text_muted)
                .child(SharedString::from(right)),
        )
}

fn context_stacked_bar(fraction: f32, categories: &[ContextCategoryShare], theme: &Theme) -> Div {
    let active: Vec<&ContextCategoryShare> = categories
        .iter()
        .filter(|category| category.tokens > 0)
        .collect();
    let total: u64 = active.iter().map(|category| category.tokens).sum();
    let last = active.len().saturating_sub(1);
    let mut fill = div()
        .debug_selector(|| "usage-context-fill".into())
        .h_full()
        .w(gpui::relative(fraction.clamp(0.0, 1.0)))
        .rounded(px(3.0))
        .overflow_hidden()
        .flex()
        .flex_row();
    if active.is_empty() || total == 0 {
        fill = fill.bg(theme.accent);
    } else {
        for (index, category) in active.iter().enumerate() {
            let key = category.key;
            let share = (category.tokens as f32 / total as f32).clamp(0.0, 1.0);
            let segment = div()
                .debug_selector(move || format!("usage-category-segment-{key}"))
                .h_full()
                .bg(category.color);
            fill = fill.child(if index == last {
                segment.flex_1()
            } else {
                segment.w(gpui::relative(share))
            });
        }
    }
    div()
        .debug_selector(|| "usage-context-track".into())
        .h(px(6.0))
        .w_full()
        .rounded(px(3.0))
        .overflow_hidden()
        .bg(theme.ink(0.12))
        .child(fill)
}

fn category_row(category: &ContextCategoryShare, theme: &Theme) -> gpui::Stateful<Div> {
    let key = category.key;
    // The popover rows' keys map 1:1 to report section ids except MCP, whose
    // per-server sections are `mcp-{index}` — the row reveals the first.
    let section = match key {
        "mcp" => "mcp-0",
        other => other,
    };
    div()
        .id(SharedString::from(format!("usage-category-{key}")))
        .debug_selector(move || format!("usage-category-{key}"))
        .cursor_pointer()
        .rounded(px(4.0))
        .px(px(6.0))
        .py(px(3.0))
        .hover(|style| style.bg(theme.element_hover))
        .on_click(move |_, window, cx| {
            window.dispatch_action(
                Box::new(crate::shell::OpenContextUsageSection(section.into())),
                cx,
            );
        })
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .flex_none()
                .size(px(10.0))
                .rounded(px(2.5))
                .bg(category.color),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_color(theme.text)
                .child(SharedString::from(category.label.clone())),
        )
        .child(div().flex_1())
        .child(
            div()
                .flex_none()
                .text_color(theme.text_muted)
                .child(SharedString::from(compact_tokens(category.tokens))),
        )
}

/// The card's small print: the occupancy's source note.
fn footnote(text: String, theme: &Theme) -> Div {
    div()
        .text_size(crate::typography::ui_rems(10.0))
        .text_color(theme.text_faint)
        .child(SharedString::from(text))
}

/// Repo and branch of the selected chat, plus the occupancy the summary
/// prints. Kept off the snapshot so a report or a checkout change can refresh
/// the header without rebuilding the prompt text.
fn usage_facts(state: &AppState) -> (Occupancy, String) {
    let chat = state.selected_chat_row();
    let location = checkout_label(
        chat.and_then(|chat| chat.cwd.as_deref()),
        chat.and_then(|chat| chat.branch.as_deref()),
    );
    let occupancy = state
        .chat_usage
        .as_ref()
        .map(occupancy_of)
        .unwrap_or(Occupancy {
            tokens: 0,
            window: None,
            estimated: false,
        });
    (occupancy, location)
}

fn checkout_label(cwd: Option<&str>, branch: Option<&str>) -> String {
    let project = cwd
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(|cwd| holt_proto::view::project_label(Some(cwd)));
    let branch = branch.map(str::trim).filter(|branch| !branch.is_empty());
    match (project, branch) {
        (Some(project), Some(branch)) => format!("{project} · {branch}"),
        (Some(project), None) => project,
        (None, Some(branch)) => branch.to_string(),
        (None, None) => t!("conversation_transcript.usage.report.no_repo").into_owned(),
    }
}

fn context_value(occupancy: &Occupancy) -> String {
    match occupancy.window.filter(|window| *window > 0) {
        Some(window) => compact_tokens(window),
        None => t!("conversation_transcript.usage.window_unknown")
            .trim_start_matches('·')
            .trim()
            .to_string(),
    }
}

fn used_value(occupancy: &Occupancy) -> String {
    let tokens = comma_tokens(occupancy.tokens);
    let mark = if occupancy.estimated { "≈" } else { "" };
    match occupancy.window.filter(|window| *window > 0) {
        Some(window) => {
            let share = percent(occupancy.tokens as f64 / window as f64);
            format!("{mark}{tokens} · {mark}{share}")
        }
        None => format!("{mark}{tokens}"),
    }
}

/// One category bar. Every row shares the same rounded token axis so the
/// lengths and the three tick labels describe the same scale.
struct CategoryBar {
    key: &'static str,
    label: String,
    tokens: u64,
    share: f32,
    color: Hsla,
}

const CHART_LABEL_WIDTH: f32 = 112.0;
const CHART_VALUE_WIDTH: f32 = 52.0;
const CHART_COLUMN_GAP: f32 = 8.0;

/// The smallest whole-number mantissa × 10ⁿ ceiling that contains the
/// largest category. The finer steps keep bars from looking needlessly short
/// (6.2k uses a 7k axis, not 10k), while the midpoint stays round.
fn chart_ceiling(peak: u64) -> u64 {
    if peak == 0 {
        return 1;
    }
    let magnitude = 10f64.powf((peak as f64).log10().floor());
    let mantissa = (peak as f64 / magnitude).ceil();
    (mantissa * magnitude) as u64
}

fn category_bars(categories: &[ContextCategoryShare]) -> Vec<CategoryBar> {
    let mut rows: Vec<&ContextCategoryShare> = categories
        .iter()
        .filter(|category| category.tokens > 0)
        .collect();
    rows.sort_by(|a, b| b.tokens.cmp(&a.tokens).then_with(|| a.label.cmp(&b.label)));
    let ceiling = chart_ceiling(rows.first().map(|category| category.tokens).unwrap_or(0));
    rows.into_iter()
        .map(|category| CategoryBar {
            key: category.key,
            label: category.label.clone(),
            tokens: category.tokens,
            share: category.tokens as f32 / ceiling as f32,
            color: category.color,
        })
        .collect()
}

fn category_bars_view(bars: &[CategoryBar], theme: &Theme) -> gpui::Stateful<Div> {
    let root = div().id("context-usage-bars").w_full();
    if bars.is_empty() {
        return root;
    }

    let ceiling = chart_ceiling(bars[0].tokens);
    let grid_color = theme.ink(0.12);
    let grid = div()
        .debug_selector(|| "context-usage-chart-grid".into())
        .absolute()
        .top_0()
        .bottom_0()
        .left(px(CHART_LABEL_WIDTH + CHART_COLUMN_GAP))
        .right(px(CHART_VALUE_WIDTH + CHART_COLUMN_GAP))
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(px(1.0))
                .bg(grid_color),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(gpui::relative(0.5))
                .w(px(1.0))
                .bg(grid_color),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(1.0))
                .bg(grid_color),
        );

    let plot = div()
        .relative()
        .w_full()
        .pt(px(4.0))
        .flex()
        .flex_col()
        .gap(px(12.0))
        .child(grid)
        .children(bars.iter().map(|bar| {
            let key = bar.key;
            div()
                .w_full()
                .h(px(32.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(CHART_COLUMN_GAP))
                .child(
                    div()
                        .debug_selector(move || format!("context-usage-label-{key}"))
                        .flex_none()
                        .w(px(CHART_LABEL_WIDTH))
                        .min_w_0()
                        .truncate()
                        .text_right()
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text_muted)
                        .child(bar.label.clone()),
                )
                .child(
                    div()
                        .debug_selector(move || format!("context-usage-track-{key}"))
                        .flex_1()
                        .min_w_0()
                        .h(px(20.0))
                        .child(
                            div()
                                .debug_selector(move || format!("context-usage-bar-{key}"))
                                .h_full()
                                .w(gpui::relative(bar.share))
                                .rounded(px(1.0))
                                .bg(bar.color),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .w(px(CHART_VALUE_WIDTH))
                        .truncate()
                        .text_size(crate::typography::ui_rems(12.5))
                        .text_color(theme.text)
                        .child(format!("~{}", compact_tokens(bar.tokens))),
                )
        }));

    let tick = |value: u64| {
        div()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.text_faint)
            .child(compact_tokens(value))
    };
    let axis = div()
        .debug_selector(|| "context-usage-chart-axis".into())
        .w_full()
        .pt(px(6.0))
        .flex()
        .flex_row()
        .gap(px(CHART_COLUMN_GAP))
        .child(div().flex_none().w(px(CHART_LABEL_WIDTH)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .child(tick(0).text_left())
                .child(tick(ceiling / 2).text_center())
                .child(tick(ceiling).text_right()),
        )
        .child(div().flex_none().w(px(CHART_VALUE_WIDTH)));

    root.flex().flex_col().gap(px(6.0)).child(plot).child(axis)
}

#[derive(Clone, Debug)]
struct ReportItem {
    id: String,
    icon: &'static str,
    title: String,
    count: Option<usize>,
    estimated_tokens: u64,
    content: String,
    /// Nested rows shown when the item expands (a turn's messages). Leaves
    /// keep this empty and show `content` instead.
    children: Vec<ReportItem>,
}

#[derive(Clone, Debug)]
struct ReportSection {
    id: String,
    icon: &'static str,
    icon_color: Hsla,
    title: String,
    count: Option<usize>,
    estimated_tokens: u64,
    description: String,
    /// Verbatim body shown under the description when the section is open.
    /// Empty for sections whose detail lives in `children`.
    content: String,
    content_selector: Option<String>,
    children: Vec<ReportItem>,
}

/// The right-pane report. It owns only view state; the engine snapshot stays
/// immutable between model requests, so streaming transcript notifications do
/// not re-render kilobytes of expanded prompt and tool text. Occupancy and
/// the checkout label are copied beside the snapshot because a provider
/// report or a branch change can move the summary without a new Context.
pub(crate) struct ContextUsagePanel {
    snapshot: Option<Arc<holt_proto::ChatContextSnapshot>>,
    occupancy: Occupancy,
    /// `repo · branch` for the selected chat, or the localized empty label.
    location: String,
    chat_id: Option<String>,
    expanded: HashSet<String>,
    /// Section to scroll to on the next render (the usage popover's category
    /// rows target one); cleared once its index has been handed to the
    /// scroll handle.
    pending_reveal: Option<String>,
    scroll: ScrollHandle,
    _observe: Subscription,
}

impl ContextUsagePanel {
    fn default_expanded() -> HashSet<String> {
        HashSet::from([
            "system".to_string(),
            "tools".to_string(),
            "rules".to_string(),
            "conversation".to_string(),
        ])
    }

    pub(crate) fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let (chat_id, snapshot, occupancy, location) = {
            let state = state.read(cx);
            let (occupancy, location) = usage_facts(state);
            (
                state.selected_chat.clone(),
                state
                    .chat_usage
                    .as_ref()
                    .and_then(|usage| usage.context.clone()),
                occupancy,
                location,
            )
        };
        let observe = cx.observe(&state, |this, state, cx| {
            let state = state.read(cx);
            let chat_id = state.selected_chat.clone();
            let chat_changed = chat_id != this.chat_id;
            let next = state
                .chat_usage
                .as_ref()
                .and_then(|usage| usage.context.as_ref());
            // Deep equality, deliberately. Pointer identity was tried and
            // rejected: every frame decodes a fresh Arc, so identity marks
            // every booking frame as a change and rebuilds the whole report
            // while it is open; the lossy watch also makes identity-based
            // revision schemes unsound. The compare walks content only while
            // the report panel lives, and a frame's own decode already paid
            // the same walk.
            let snapshot_changed = next != this.snapshot.as_ref();
            let (occupancy, location) = usage_facts(state);
            let facts_changed = occupancy != this.occupancy || location != this.location;
            if chat_changed {
                this.chat_id = chat_id;
                this.expanded = Self::default_expanded();
                this.pending_reveal = None;
                this.scroll.set_offset(gpui::Point::default());
            }
            if snapshot_changed {
                this.snapshot = next.cloned();
            }
            if facts_changed {
                this.occupancy = occupancy;
                this.location = location;
            }
            if chat_changed || snapshot_changed || facts_changed {
                cx.notify();
            }
        });
        Self {
            snapshot,
            occupancy,
            location,
            chat_id,
            expanded: Self::default_expanded(),
            pending_reveal: None,
            scroll: ScrollHandle::new(),
            _observe: observe,
        }
    }

    /// Reveal a report section for the usage popover's category rows: expand
    /// it and scroll it to the top of the viewport on the next render. A
    /// section the snapshot does not have yet (preview still loading) reveals
    /// once it arrives.
    pub(crate) fn reveal_section(&mut self, section: &str, cx: &mut Context<Self>) {
        self.expanded.insert(section.to_string());
        self.pending_reveal = Some(section.to_string());
        cx.notify();
    }

    fn entry_title(entry: &holt_proto::ChatContextEntry) -> String {
        use holt_proto::ChatContextEntryKind as Kind;
        let index = entry
            .id
            .strip_prefix("message-")
            .and_then(|index| index.parse::<usize>().ok())
            .map_or(0, |index| index + 1);
        match entry.kind {
            Kind::SystemPrompt => t!("conversation_transcript.usage.report.system_prompt").into(),
            Kind::ToolDefinitions => t!(
                "conversation_transcript.usage.report.tool_definitions",
                count = entry.children.len()
            )
            .into(),
            Kind::Tool => entry.name.clone().unwrap_or_default(),
            Kind::McpServer => entry
                .name
                .as_ref()
                .map_or_else(|| "MCP".to_string(), |server| format!("MCP · {server}")),
            Kind::Skill => entry.name.clone().unwrap_or_default(),
            Kind::Skills => t!(
                "conversation_transcript.usage.report.skills",
                count = entry.children.len()
            )
            .into(),
            Kind::UserMessage => {
                t!("conversation_transcript.usage.report.user", index = index).into()
            }
            Kind::AssistantMessage => t!(
                "conversation_transcript.usage.report.assistant",
                index = index
            )
            .into(),
            Kind::ToolResult => {
                let label = t!(
                    "conversation_transcript.usage.report.tool_result",
                    index = index
                );
                entry.name.as_ref().map_or_else(
                    || label.clone().into_owned(),
                    |name| format!("{label} · {name}"),
                )
            }
            // A newer engine's entry kind: name it when the entry carries
            // one, otherwise fall back to the generic label — the row and its
            // tokens stay visible either way.
            Kind::Unknown => entry.name.clone().unwrap_or_else(|| {
                t!("conversation_transcript.usage.report.unknown_entry").into_owned()
            }),
        }
    }

    fn entry_icon(kind: holt_proto::ChatContextEntryKind) -> &'static str {
        use holt_proto::ChatContextEntryKind as Kind;
        match kind {
            Kind::SystemPrompt => crate::icons::SYSTEM_PROMPT,
            Kind::ToolDefinitions | Kind::Tool | Kind::ToolResult => crate::icons::WRENCH,
            Kind::McpServer => crate::icons::WIDGET,
            Kind::Skills | Kind::Skill => crate::icons::CUBE,
            Kind::UserMessage => crate::icons::CHAT_ROUND_LINE,
            Kind::AssistantMessage => crate::icons::BOT,
            Kind::Unknown => crate::icons::WIDGET,
        }
    }

    #[cfg(test)]
    fn request_label(snapshot: &holt_proto::ChatContextSnapshot) -> String {
        if snapshot.preview {
            t!(
                "conversation_transcript.usage.report.next_request",
                model = snapshot.model
            )
            .into_owned()
        } else {
            t!(
                "conversation_transcript.usage.report.latest_request",
                model = snapshot.model
            )
            .into_owned()
        }
    }

    fn build_sections(
        snapshot: &holt_proto::ChatContextSnapshot,
        theme: &Theme,
    ) -> Vec<ReportSection> {
        use holt_proto::ChatContextEntryKind as Kind;

        let mut sections = Vec::new();
        let mut extracted_rules = Vec::new();
        let mut messages: Vec<(bool, ReportItem)> = Vec::new();

        for entry in &snapshot.entries {
            match entry.kind {
                Kind::SystemPrompt => {
                    // The engine records the request's own working directory
                    // on the snapshot — the rule classification walks against
                    // it, not the chat row's possibly-stale folder.
                    let (base_tokens, rules) = split_system_prompt(
                        &entry.content,
                        entry.estimated_tokens,
                        snapshot.cwd.as_deref(),
                    );
                    extracted_rules.extend(rules);
                    sections.push(ReportSection {
                        id: entry.id.clone(),
                        icon: crate::icons::SYSTEM_PROMPT,
                        icon_color: theme.text_muted,
                        title: t!("conversation_transcript.usage.report.system_prompt")
                            .into_owned(),
                        count: None,
                        estimated_tokens: base_tokens,
                        description: t!("conversation_transcript.usage.report.desc.system_prompt")
                            .into_owned(),
                        content: entry.content.clone(),
                        content_selector: Some(format!("context-content-{}", entry.id)),
                        children: Vec::new(),
                    });
                }
                Kind::ToolDefinitions => {
                    let mut children: Vec<ReportItem> = entry
                        .children
                        .iter()
                        .map(|tool| ReportItem {
                            id: tool.id.clone(),
                            icon: crate::icons::WRENCH,
                            title: Self::entry_title(tool),
                            count: None,
                            estimated_tokens: tool.estimated_tokens,
                            content: Self::format_tool_content(&tool.content),
                            children: Vec::new(),
                        })
                        .collect();
                    children.sort_by(|a, b| {
                        b.estimated_tokens
                            .cmp(&a.estimated_tokens)
                            .then_with(|| a.title.cmp(&b.title))
                    });
                    sections.push(ReportSection {
                        id: entry.id.clone(),
                        icon: crate::icons::WRENCH,
                        icon_color: category_color("tools"),
                        title: t!("conversation_transcript.usage.report.tool_definitions_label")
                            .into_owned(),
                        count: Some(entry.children.len()),
                        estimated_tokens: entry.estimated_tokens,
                        description: t!(
                            "conversation_transcript.usage.report.desc.tool_definitions"
                        )
                        .into_owned(),
                        content: String::new(),
                        content_selector: None,
                        children,
                    });
                }
                Kind::Skills => {
                    let mut children: Vec<ReportItem> = entry
                        .children
                        .iter()
                        .map(|skill| ReportItem {
                            id: skill.id.clone(),
                            icon: crate::icons::CUBE,
                            title: Self::entry_title(skill),
                            count: None,
                            estimated_tokens: skill.estimated_tokens,
                            content: skill.content.clone(),
                            children: Vec::new(),
                        })
                        .collect();
                    children.sort_by(|a, b| {
                        b.estimated_tokens
                            .cmp(&a.estimated_tokens)
                            .then_with(|| a.title.cmp(&b.title))
                    });
                    sections.push(ReportSection {
                        id: entry.id.clone(),
                        icon: crate::icons::CUBE,
                        icon_color: category_color("skills"),
                        title: t!("conversation_transcript.usage.report.skills_label").into_owned(),
                        count: Some(entry.children.len()),
                        estimated_tokens: entry.estimated_tokens,
                        description: t!("conversation_transcript.usage.report.desc.skills")
                            .into_owned(),
                        content: String::new(),
                        content_selector: None,
                        children,
                    });
                }
                Kind::McpServer => {
                    let server_prefix =
                        entry.name.as_ref().map(|server| format!("mcp__{server}__"));
                    let mut children: Vec<ReportItem> = entry
                        .children
                        .iter()
                        .map(|tool| {
                            let raw_title = Self::entry_title(tool);
                            let clean_title = server_prefix
                                .as_deref()
                                .and_then(|prefix| raw_title.strip_prefix(prefix))
                                .unwrap_or(&raw_title)
                                .to_string();
                            ReportItem {
                                id: tool.id.clone(),
                                icon: crate::icons::WRENCH,
                                title: clean_title,
                                count: None,
                                estimated_tokens: tool.estimated_tokens,
                                content: Self::format_tool_content(&tool.content),
                                children: Vec::new(),
                            }
                        })
                        .collect();
                    children.sort_by(|a, b| {
                        b.estimated_tokens
                            .cmp(&a.estimated_tokens)
                            .then_with(|| a.title.cmp(&b.title))
                    });
                    sections.push(ReportSection {
                        id: entry.id.clone(),
                        icon: crate::icons::WIDGET,
                        icon_color: category_color("mcp"),
                        title: Self::entry_title(entry),
                        count: Some(entry.children.len()),
                        estimated_tokens: entry.estimated_tokens,
                        description: t!("conversation_transcript.usage.report.desc.mcp")
                            .into_owned(),
                        content: String::new(),
                        content_selector: None,
                        children,
                    });
                }
                Kind::UserMessage | Kind::AssistantMessage | Kind::ToolResult => {
                    messages.push((
                        entry.kind == Kind::UserMessage,
                        ReportItem {
                            id: entry.id.clone(),
                            icon: Self::entry_icon(entry.kind),
                            title: Self::entry_title(entry),
                            count: None,
                            estimated_tokens: entry.estimated_tokens,
                            content: entry.content.clone(),
                            children: Vec::new(),
                        },
                    ));
                }
                Kind::Tool | Kind::Skill => {}
                // A newer engine's entry kind: show it as its own leaf under
                // the conversation rather than dropping the row.
                Kind::Unknown => messages.push((
                    false,
                    ReportItem {
                        id: entry.id.clone(),
                        icon: Self::entry_icon(entry.kind),
                        title: Self::entry_title(entry),
                        count: None,
                        estimated_tokens: entry.estimated_tokens,
                        content: entry.content.clone(),
                        children: Vec::new(),
                    },
                )),
            }
        }

        if !extracted_rules.is_empty() {
            let total_rule_files = extracted_rules.len();
            let rules_tokens: u64 = extracted_rules
                .iter()
                .map(|rule| rule.estimated_tokens)
                .sum();
            let mut user_rules = Vec::new();
            let mut project_rules = Vec::new();
            for rule in extracted_rules {
                if rule.is_user_rule {
                    user_rules.push(rule);
                } else {
                    project_rules.push(rule);
                }
            }
            let mut children = Vec::new();
            if !user_rules.is_empty() {
                let tokens = user_rules.iter().map(|rule| rule.estimated_tokens).sum();
                let content = user_rules
                    .iter()
                    .map(|rule| format!("# {}\n{}", rule.path, rule.content))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                children.push(ReportItem {
                    id: "rule-user".into(),
                    icon: crate::icons::LIST,
                    title: t!("conversation_transcript.usage.report.user_rules").into_owned(),
                    count: (user_rules.len() > 1).then_some(user_rules.len()),
                    estimated_tokens: tokens,
                    content,
                    children: Vec::new(),
                });
            }
            if !project_rules.is_empty() {
                let tokens = project_rules.iter().map(|rule| rule.estimated_tokens).sum();
                let content = project_rules
                    .iter()
                    .map(|rule| format!("# {}\n{}", rule.path, rule.content))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                children.push(ReportItem {
                    id: "rule-project".into(),
                    icon: crate::icons::FOLDER,
                    title: t!("conversation_transcript.usage.report.project_rules").into_owned(),
                    count: (project_rules.len() > 1).then_some(project_rules.len()),
                    estimated_tokens: tokens,
                    content,
                    children: Vec::new(),
                });
            }
            let rules_section = ReportSection {
                id: "rules".into(),
                icon: crate::icons::LIST,
                icon_color: category_color("rules"),
                title: t!("conversation_transcript.usage.report.rules_label").into_owned(),
                count: Some(total_rule_files),
                estimated_tokens: rules_tokens,
                description: t!("conversation_transcript.usage.report.desc.rules").into_owned(),
                content: String::new(),
                content_selector: None,
                children,
            };
            // Place Rules immediately after Tool definitions (or after System prompt).
            let insert_at = sections
                .iter()
                .position(|section| section.id == "tools")
                .map(|index| index + 1)
                .or_else(|| {
                    sections
                        .iter()
                        .position(|section| section.id == "system")
                        .map(|index| index + 1)
                })
                .unwrap_or(sections.len());
            sections.insert(insert_at, rules_section);
        }

        if !messages.is_empty() {
            // Group the flat message list into turns: entries before the first
            // user message stand alone, each user message opens the next turn
            // and owns everything after it until the following user message.
            let mut groups: Vec<(bool, Vec<ReportItem>)> = Vec::new();
            for (is_user, message) in messages {
                if is_user {
                    groups.push((true, vec![message]));
                } else {
                    match groups.last_mut() {
                        Some((true, group)) => group.push(message),
                        _ => match groups.first_mut() {
                            Some((false, group)) => group.push(message),
                            _ => groups.push((false, vec![message])),
                        },
                    }
                }
            }
            let mut children: Vec<ReportItem> = Vec::new();
            let mut turn_index = 0;
            // The section count mirrors the reference layout: total records
            // across turns, not the number of turn rows.
            let total_records: usize = groups.iter().map(|(_, group)| group.len()).sum();
            for (is_turn, group) in groups {
                let tokens: u64 = group.iter().map(|item| item.estimated_tokens).sum();
                let count = group.len();
                // Only real user messages advance the turn counter — an MCP
                // bootstrap pair (a tool call and result with no user message
                // ahead of it) forms a pre-group that must not eat "Turn 1".
                let (title, id) = if is_turn {
                    turn_index += 1;
                    (
                        t!(
                            "conversation_transcript.usage.report.turn",
                            index = turn_index
                        )
                        .into_owned(),
                        format!("turn-{}", group[0].id),
                    )
                } else {
                    (
                        t!("conversation_transcript.usage.report.before_first_user_message")
                            .into_owned(),
                        format!("pre-{}", group[0].id),
                    )
                };
                children.push(ReportItem {
                    id,
                    icon: crate::icons::CHAT_ROUND_LINE,
                    title,
                    count: Some(count),
                    estimated_tokens: tokens,
                    content: String::new(),
                    children: group,
                });
            }
            let conversation_tokens: u64 = children.iter().map(|item| item.estimated_tokens).sum();
            sections.push(ReportSection {
                id: "conversation".into(),
                icon: crate::icons::CHAT_ROUND_LINE,
                icon_color: category_color("conversation"),
                title: t!("conversation_transcript.usage.report.conversation_label").into_owned(),
                count: Some(total_records),
                estimated_tokens: conversation_tokens,
                description: t!("conversation_transcript.usage.report.desc.conversation")
                    .into_owned(),
                content: String::new(),
                content_selector: None,
                children,
            });
        }

        sections
    }

    fn format_tool_content(content: &str) -> String {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
            return content.to_string();
        };
        let Some(description) = value
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|desc| !desc.is_empty())
        else {
            return content.to_string();
        };
        if let Some(params) = value.get("parameters")
            && let Ok(pretty_params) = serde_json::to_string_pretty(params)
        {
            format!("{description}\n\n{pretty_params}")
        } else {
            description.to_string()
        }
    }

    fn render_item(&self, item: &ReportItem, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let expanded = self.expanded.contains(&item.id);
        let id = item.id.clone();
        let selector_id = item.id.clone();
        let title = item.title.clone();
        let count = item.count;
        let tokens = format!("~{}", comma_tokens(item.estimated_tokens));
        let caret = if expanded {
            crate::icons::CARET_DOWN
        } else {
            crate::icons::CARET_RIGHT
        };
        let header = div()
            .id(SharedString::from(format!("context-entry-{}", item.id)))
            .debug_selector(move || format!("context-entry-{selector_id}"))
            .h(px(34.0))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(10.0))
            .rounded(px(5.0))
            .cursor_pointer()
            .hover(|style| style.bg(theme.ink(0.045)))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.expanded.insert(id.clone()) {
                    this.expanded.remove(&id);
                }
                cx.notify();
            }))
            .child(
                div()
                    .flex_none()
                    .size(px(12.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        crate::icons::icon(caret)
                            .size(px(8.5))
                            .text_color(theme.text_muted),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .size(px(16.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        crate::icons::icon(item.icon)
                            .size(px(13.5))
                            .text_color(theme.text_muted),
                    ),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(6.0))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(14.0))
                            .text_color(theme.text)
                            .child(SharedString::from(title)),
                    )
                    .children(count.map(|count| {
                        div()
                            .flex_none()
                            .text_size(crate::typography::ui_rems(14.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(count.to_string()))
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(crate::typography::ui_rems(14.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(tokens)),
            );
        let mut row = div().w_full().flex().flex_col().child(header);
        if expanded && !item.content.is_empty() {
            let content_id = item.id.clone();
            row = row.child(
                div()
                    .debug_selector(move || format!("context-content-{content_id}"))
                    .w_full()
                    .mt(px(2.0))
                    .mb(px(6.0))
                    .px(px(12.0))
                    .py(px(10.0))
                    .rounded(px(6.0))
                    .bg(theme.ink(0.035))
                    .border_1()
                    .border_color(theme.border)
                    .font_family(theme.font_mono.clone())
                    .text_size(crate::typography::ui_rems(12.0))
                    .line_height(gpui::relative(1.45))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(item.content.clone())),
            );
        }
        if expanded && !item.children.is_empty() {
            row = row.child(
                div()
                    .w_full()
                    .pb(px(4.0))
                    .pl(px(14.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .children(
                        item.children
                            .iter()
                            .map(|child| self.render_item(child, theme, cx)),
                    ),
            );
        }
        row
    }

    fn render_section(
        &self,
        section: &ReportSection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let expanded = self.expanded.contains(&section.id);
        let id = section.id.clone();
        let selector_id = section.id.clone();
        let title = section.title.clone();
        let count = section.count;
        let tokens = format!("~{}", comma_tokens(section.estimated_tokens));
        let caret = if expanded {
            crate::icons::CARET_DOWN
        } else {
            crate::icons::CARET_RIGHT
        };
        let header = div()
            .id(SharedString::from(format!("context-entry-{}", section.id)))
            .debug_selector(move || format!("context-entry-{selector_id}"))
            .h(px(36.0))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(10.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(theme.ink(0.055))
            .hover(|style| style.bg(theme.ink(0.085)))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.expanded.insert(id.clone()) {
                    this.expanded.remove(&id);
                }
                cx.notify();
            }))
            .child(
                div()
                    .flex_none()
                    .size(px(12.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        crate::icons::icon(caret)
                            .size(px(8.5))
                            .text_color(theme.text_muted),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .size(px(16.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        crate::icons::icon(section.icon)
                            .size(px(14.0))
                            .text_color(section.icon_color),
                    ),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(6.0))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(14.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(title)),
                    )
                    .children(count.map(|count| {
                        div()
                            .flex_none()
                            .text_size(crate::typography::ui_rems(14.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(count.to_string()))
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(crate::typography::ui_rems(14.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(tokens)),
            );

        let mut block = div().w_full().flex().flex_col().child(header);
        if expanded {
            let has_content = !section.content.is_empty();
            let mut desc = div()
                .w_full()
                .px(px(8.0))
                .pt(px(8.0))
                .pb(px(6.0))
                .text_size(crate::typography::ui_rems(14.0))
                .line_height(gpui::relative(1.45))
                .text_color(theme.text_muted)
                .child(SharedString::from(section.description.clone()));
            if !has_content && let Some(selector) = section.content_selector.clone() {
                desc = desc.debug_selector(move || selector.clone());
            }
            block = block.child(desc);
            if has_content {
                let selector = section.content_selector.clone();
                let mut body = div()
                    .w_full()
                    .min_w_0()
                    .mb(px(6.0))
                    .px(px(12.0))
                    .py(px(10.0))
                    .rounded(px(6.0))
                    .bg(theme.ink(0.035))
                    .border_1()
                    .border_color(theme.border)
                    .text_size(crate::typography::ui_rems(14.0))
                    .line_height(gpui::relative(1.45))
                    .text_color(theme.text)
                    .child(SharedString::from(section.content.clone()));
                if let Some(selector) = selector {
                    body = body.debug_selector(move || selector);
                }
                block = block.child(body);
            }
            if !section.children.is_empty() {
                block = block.child(
                    div()
                        .w_full()
                        .pb(px(4.0))
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .children(
                            section
                                .children
                                .iter()
                                .map(|item| self.render_item(item, theme, cx)),
                        ),
                );
            }
        }
        block
    }

    fn render_summary(
        &self,
        snapshot: &Arc<holt_proto::ChatContextSnapshot>,
        theme: &Theme,
    ) -> impl IntoElement {
        let categories = context_categories(&ChatUsage {
            context: Some(Arc::clone(snapshot)),
            occupancy: holt_proto::ChatOccupancy {
                tokens: self.occupancy.tokens,
                context_window: self.occupancy.window,
                estimated: self.occupancy.estimated,
            },
            ..ChatUsage::default()
        });
        let bars = category_bars(&categories);
        let model = if snapshot.model.is_empty() {
            "—".to_string()
        } else {
            snapshot.model.clone()
        };
        let fact = |label: String, value: String| {
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_faint)
                        .child(label),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(crate::typography::ui_rems(14.0))
                        .text_color(theme.text)
                        .child(value),
                )
        };
        div()
            .id("context-usage-summary")
            .flex_none()
            .w_full()
            .px(px(14.0))
            .pt(px(12.0))
            .pb(px(14.0))
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(16.0))
                            .child(fact(
                                t!("conversation_transcript.usage.report.repo").into_owned(),
                                self.location.clone(),
                            ))
                            .child(fact(
                                t!("conversation_transcript.usage.report.model").into_owned(),
                                model,
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(16.0))
                            .child(fact(
                                t!("conversation_transcript.usage.report.context_size")
                                    .into_owned(),
                                context_value(&self.occupancy),
                            ))
                            .child(fact(
                                t!("conversation_transcript.usage.report.used").into_owned(),
                                used_value(&self.occupancy),
                            )),
                    )
                    .when(self.occupancy.estimated, |facts| {
                        facts.child(
                            div()
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text_muted)
                                .child(t!("conversation_transcript.usage.note.estimated")),
                        )
                    })
                    .when(snapshot.preview, |facts| {
                        facts.child(
                            div()
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text_muted)
                                .child(t!(
                                    "conversation_transcript.usage.report.next_request",
                                    model = snapshot.model.clone()
                                )),
                        )
                    }),
            )
            .child(
                div()
                    .w_full()
                    .pt(px(4.0))
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(15.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(t!("conversation_transcript.usage.report.chart_title")),
                    )
                    .child(category_bars_view(&bars, theme)),
            )
    }
}

impl Render for ContextUsagePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let Some(snapshot) = self.snapshot.as_ref() else {
            return div()
                .id("context-usage-empty")
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p(px(24.0))
                .text_center()
                .text_size(crate::typography::ui_rems(14.0))
                .text_color(theme.text_muted)
                .child(t!("conversation_transcript.usage.report.empty"));
        };
        let sections = Self::build_sections(snapshot, &theme);
        // The reveal targets a section index. The summary is the scroll's first
        // direct child, so a section sits one past its index in `sections`.
        if let Some(reveal) = self.pending_reveal.clone()
            && let Some(index) = sections.iter().position(|section| section.id == reveal)
        {
            self.scroll.scroll_to_top_of_item(index + 1);
            self.pending_reveal = None;
        }
        let top_bar = div()
            .id("context-usage-header")
            .flex_none()
            .h(px(36.0))
            .px(px(14.0))
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(14.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(t!("conversation_transcript.usage.title")),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .children((!snapshot.loading_mcp_servers.is_empty()).then(|| {
                        div()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child(t!(
                                "conversation_transcript.usage.report.mcp_loading",
                                servers = snapshot.loading_mcp_servers.join(", ")
                            ))
                    })),
            );

        div()
            .id("context-usage-report")
            .size_full()
            .flex()
            .flex_col()
            .child(top_bar)
            .child(
                div()
                    .id("context-usage-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .occlude()
                    .w_full()
                    .px(px(12.0))
                    .py(px(10.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .text_size(crate::typography::ui_rems(14.0))
                    .child(self.render_summary(snapshot, &theme))
                    .children(
                        sections
                            .iter()
                            .map(|section| self.render_section(section, &theme, cx)),
                    ),
            )
    }
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
                                // A landed snapshot answers the pending
                                // preview request — the slot frees for the
                                // next one.
                                let snapshot_landed = usage.context.is_some();
                                state.chat_usage = Some(usage);
                                if snapshot_landed {
                                    state.context_preview_pending = None;
                                }
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

/// Ask the engine for the report's next-request Context preview — the shared
/// request path for the usage popover and the right-pane report tab.
///
/// One request per chat at a time: the pending marker keeps the two surfaces
/// from racing each other into duplicate preview builds (each one mounts
/// MCP). The unary reply doubles as the watch's fallback — if the frame
/// stream dropped the snapshot, it lands here instead, so the report never
/// parks on its preparing state; a failed reply frees the marker so the next
/// open retries.
pub(crate) async fn request_context_preview(
    state: gpui::WeakEntity<AppState>,
    cx: &mut gpui::AsyncApp,
) {
    let request = state
        .update(cx, |state, _| {
            let chat_id = state.selected_chat.clone()?;
            let engine = state.engine().cloned()?;
            let needed = state
                .chat_usage
                .as_ref()
                .and_then(|usage| usage.context.as_ref())
                .is_none()
                && state.context_preview_pending.as_deref() != Some(chat_id.as_str());
            needed.then(|| {
                state.context_preview_pending = Some(chat_id.clone());
                (chat_id, engine)
            })
        })
        .ok()
        .flatten();
    let Some((chat_id, engine)) = request else {
        return;
    };
    let reply = engine
        .client()
        .call(
            methods::PREVIEW_CHAT_CONTEXT,
            serde_json::json!({"chatId": chat_id}),
        )
        .await;
    match reply {
        Ok(value) => {
            let snapshot: Option<holt_proto::ChatContextSnapshot> =
                serde_json::from_value(value).ok();
            let _ = state.update(cx, |state, cx| {
                if state.selected_chat.as_deref() != Some(chat_id.as_str()) {
                    return;
                }
                state.context_preview_pending = None;
                // The watch normally lands the frame first; grafting only
                // matters when it dropped this snapshot — otherwise this is
                // a no-op.
                if let Some(snapshot) = snapshot
                    && state
                        .chat_usage
                        .as_ref()
                        .and_then(|usage| usage.context.as_ref())
                        .is_none()
                    && let Some(usage) = state.chat_usage.as_mut()
                {
                    usage.context = Some(Arc::new(snapshot));
                    cx.notify();
                }
            });
        }
        Err(error) => {
            tracing::warn!(%error, "context preview failed");
            // A failed preview frees the slot so the next open retries
            // instead of parking on the pending marker forever.
            let _ = state.update(cx, |state, _| {
                if state.context_preview_pending.as_deref() == Some(chat_id.as_str()) {
                    state.context_preview_pending = None;
                }
            });
        }
    }
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

    fn decode(value: Value) -> ChatUsage {
        WatchCoordinator::decode(value).unwrap()
    }

    #[test]
    fn occupancy_folds_the_frame_and_a_blank_frame_stays_empty() {
        let _locale = crate::i18n::tests::locale_lock();
        let usage = decode(json!({
            "occupancy": {"tokens": 700, "contextWindow": 1000, "estimated": false}
        }));
        let occupancy = occupancy_of(&usage);
        assert_eq!(occupancy.tokens, 700);
        assert_eq!(occupancy.window, Some(1000));
        assert!(!occupancy.estimated);
        assert!((occupancy.fraction() - 0.7).abs() < 1e-6);

        // A custom model's window is withheld: an empty ring arc rather than
        // a fabricated one.
        let unknown = Occupancy {
            window: None,
            ..occupancy
        };
        assert_eq!(unknown.fraction(), 0.0);

        // A blank frame yields no fraction and no NaN — the ring just stays
        // empty.
        let blank = occupancy_of(&ChatUsage::default());
        assert_eq!(
            blank,
            Occupancy {
                tokens: 0,
                window: None,
                estimated: false
            }
        );
        assert_eq!(blank.fraction(), 0.0);
    }

    #[test]
    fn the_source_note_covers_every_state_of_the_two_flags() {
        let _locale = crate::i18n::tests::locale_lock();
        let of = |estimated, window| {
            source_note(&occupancy_of(&ChatUsage {
                occupancy: holt_proto::ChatOccupancy {
                    tokens: 1,
                    context_window: window,
                    estimated,
                },
                ..Default::default()
            }))
        };
        assert_eq!(of(false, Some(1_000)), None);
        assert_eq!(
            of(false, None),
            Some(t!("conversation_transcript.usage.note.window_unknown"))
        );
        assert_eq!(
            of(true, Some(1_000)),
            Some(t!("conversation_transcript.usage.note.estimated"))
        );
        assert_eq!(
            of(true, None),
            Some(t!("conversation_transcript.usage.note.estimated_unknown"))
        );
    }

    /// The ring and the card both paint, at the geometry the bars claim:
    /// the arc is stroked in a paint pass and the bars only exist on a layout
    /// pass, so this is where a bad fraction, an unset theme global, or a bar
    /// that never measured would surface.
    #[gpui::test]
    fn the_ring_and_its_card_paint_their_bars(cx: &mut TestAppContext) {
        let _locale = crate::i18n::tests::locale_lock();
        cx.update(|cx| cx.set_global(Theme::default()));
        let mut usage = decode(json!({
            "gross": 1000,
            "byKind": {
                "turn": {"input": 560, "output": 40, "cacheRead": 200},
                "title": {"input": 160, "output": 40}
            },
            "recordCount": 3,
            "occupancy": {"tokens": 40000, "contextWindow": 1000000, "estimated": false}
        }));
        usage.context = Some(Arc::new(holt_proto::ChatContextSnapshot {
            model: "gpt-test".into(),
            cwd: None,
            preview: false,
            loading_mcp_servers: Vec::new(),
            entries: vec![
                holt_proto::ChatContextEntry {
                    id: "system".into(),
                    kind: holt_proto::ChatContextEntryKind::SystemPrompt,
                    name: None,
                    content: "system".into(),
                    estimated_tokens: 10000,
                    children: Vec::new(),
                },
                holt_proto::ChatContextEntry {
                    id: "tools".into(),
                    kind: holt_proto::ChatContextEntryKind::ToolDefinitions,
                    name: None,
                    content: String::new(),
                    estimated_tokens: 10000,
                    children: Vec::new(),
                },
                holt_proto::ChatContextEntry {
                    id: "message-0".into(),
                    kind: holt_proto::ChatContextEntryKind::UserMessage,
                    name: None,
                    content: "hello".into(),
                    estimated_tokens: 20000,
                    children: Vec::new(),
                },
            ],
        }));
        let window = cx.add_window(|_, cx| UsagePreview::new(Some(usage), cx));
        let mut visual = gpui::VisualTestContext::from_window(*window, cx);

        let ring = visual.debug_bounds("chat-usage-ring").unwrap();
        assert_eq!((ring.size.width, ring.size.height), (px(20.0), px(20.0)));
        let card = visual.debug_bounds("chat-usage-card").unwrap();
        assert_eq!(card.size.width, px(CARD_WIDTH));

        let track = visual.debug_bounds("usage-context-track").unwrap();
        let fill = visual.debug_bounds("usage-context-fill").unwrap();
        assert_eq!(track.size.height, px(6.0));
        assert_close(fill.size.width / track.size.width, 0.04, "occupancy fill");

        let system_seg = visual
            .debug_bounds("usage-category-segment-system")
            .unwrap();
        let tools_seg = visual.debug_bounds("usage-category-segment-tools").unwrap();
        let conv_seg = visual
            .debug_bounds("usage-category-segment-conversation")
            .unwrap();
        assert_close(
            system_seg.size.width / fill.size.width,
            0.25,
            "system segment",
        );
        assert_close(
            tools_seg.size.width / fill.size.width,
            0.25,
            "tools segment",
        );
        assert_close(
            conv_seg.size.width / fill.size.width,
            0.50,
            "conversation segment",
        );

        assert!(visual.debug_bounds("usage-category-system").is_some());
        assert!(visual.debug_bounds("usage-category-tools").is_some());
        assert!(visual.debug_bounds("usage-category-rules").is_some());
        assert!(visual.debug_bounds("usage-category-skills").is_some());
        assert!(visual.debug_bounds("usage-category-mcp").is_some());
        assert!(visual.debug_bounds("usage-category-conversation").is_some());
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
        let window = cx.add_window(|_, cx| UsagePreview::new(None, cx));
        let mut visual = gpui::VisualTestContext::from_window(*window, cx);
        assert!(visual.debug_bounds("chat-usage-ring").is_none());
    }

    /// The ring beside the card, both mounted in a real window.
    struct UsagePreview {
        usage: Option<ChatUsage>,
        pickers: Entity<Pickers>,
    }

    impl UsagePreview {
        fn new(usage: Option<ChatUsage>, cx: &mut Context<Self>) -> Self {
            let state = cx.new(|_| AppState::new());
            let pickers = cx.new(|cx| Pickers::new(state, cx));
            Self { usage, pickers }
        }
    }

    impl Render for UsagePreview {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = Theme::of(cx).clone();
            let card = self
                .usage
                .as_ref()
                .map(|usage| card_content(usage, &theme).w(px(CARD_WIDTH)));
            let open = self.pickers.read(cx).context_usage_open();
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .children(ring(self.usage.as_ref(), open, &theme, &self.pickers))
                .children(card)
        }
    }

    #[gpui::test]
    fn the_ring_click_pins_and_unpins_the_usage_popover(cx: &mut TestAppContext) {
        let _locale = crate::i18n::tests::locale_lock();
        cx.update(|cx| cx.set_global(Theme::default()));
        let window = cx.add_window(|_, cx| {
            UsagePreview::new(Some(decode(frame(780, Some(1_000_000), false))), cx)
        });
        let mut visual = gpui::VisualTestContext::from_window(*window, cx);
        let ring = visual.debug_bounds("chat-usage-ring").unwrap();
        visual.simulate_click(ring.center(), Default::default());
        window
            .update(cx, |preview, _, cx| {
                assert!(preview.pickers.read(cx).context_usage_open());
            })
            .unwrap();
        visual.simulate_click(ring.center(), Default::default());
        window
            .update(cx, |preview, _, cx| {
                assert!(!preview.pickers.read(cx).context_usage_open());
            })
            .unwrap();
    }

    #[test]
    fn the_context_report_distinguishes_preview_from_a_captured_request() {
        let _locale = crate::i18n::tests::locale_lock();
        let mut snapshot = holt_proto::ChatContextSnapshot {
            model: "gpt-test".into(),
            cwd: None,
            ..Default::default()
        };
        assert_eq!(
            ContextUsagePanel::request_label(&snapshot),
            t!(
                "conversation_transcript.usage.report.latest_request",
                model = "gpt-test"
            )
        );
        snapshot.preview = true;
        assert_eq!(
            ContextUsagePanel::request_label(&snapshot),
            t!(
                "conversation_transcript.usage.report.next_request",
                model = "gpt-test"
            )
        );
    }

    #[test]
    fn the_report_names_mcp_servers_and_skills_independently() {
        let _locale = crate::i18n::tests::locale_lock();
        let entry =
            |kind: holt_proto::ChatContextEntryKind, name: &str| holt_proto::ChatContextEntry {
                id: "entry".into(),
                kind,
                name: Some(name.into()),
                content: String::new(),
                estimated_tokens: 0,
                children: Vec::new(),
            };
        let mcp = entry(holt_proto::ChatContextEntryKind::McpServer, "figma");
        assert_eq!(ContextUsagePanel::entry_title(&mcp), "MCP · figma");
        let skill = entry(holt_proto::ChatContextEntryKind::Skill, "grill");
        assert_eq!(ContextUsagePanel::entry_title(&skill), "grill");
        let mut skills = entry(holt_proto::ChatContextEntryKind::Skills, "unused");
        skills.name = None;
        skills.children = vec![skill];
        assert_eq!(
            ContextUsagePanel::entry_title(&skills),
            t!("conversation_transcript.usage.report.skills", count = 1)
        );
    }

    /// An MCP bootstrap pair (a tool call and its result with no user message
    /// ahead of it) forms the conversation's pre-group. It must not eat
    /// "Turn 1": only real user messages advance the turn counter.
    #[test]
    fn a_bootstrap_pair_before_the_first_user_message_does_not_eat_turn_one() {
        let _locale = crate::i18n::tests::locale_lock();
        let message = |id: &str, kind: holt_proto::ChatContextEntryKind, content: &str| {
            holt_proto::ChatContextEntry {
                id: id.into(),
                kind,
                name: None,
                content: content.into(),
                estimated_tokens: 1,
                children: Vec::new(),
            }
        };
        use holt_proto::ChatContextEntryKind as Kind;
        let snapshot = holt_proto::ChatContextSnapshot {
            model: "gpt-test".into(),
            cwd: None,
            preview: false,
            loading_mcp_servers: Vec::new(),
            entries: vec![
                message("message-0", Kind::AssistantMessage, "bootstrap tool call"),
                message("message-1", Kind::ToolResult, "bootstrap tool result"),
                message("message-2", Kind::UserMessage, "first prompt"),
                message("message-3", Kind::AssistantMessage, "first answer"),
                message("message-4", Kind::UserMessage, "second prompt"),
            ],
        };
        let sections = ContextUsagePanel::build_sections(&snapshot, &Theme::default());
        let conversation = sections
            .iter()
            .find(|section| section.id == "conversation")
            .expect("the conversation section builds");
        let titles: Vec<&str> = conversation
            .children
            .iter()
            .map(|item| item.title.as_str())
            .collect();
        assert_eq!(
            titles[0],
            t!("conversation_transcript.usage.report.before_first_user_message"),
            "{titles:?}"
        );
        assert_eq!(
            titles[1],
            t!("conversation_transcript.usage.report.turn", index = 1),
            "the first real user message is Turn 1: {titles:?}"
        );
        assert_eq!(
            titles[2],
            t!("conversation_transcript.usage.report.turn", index = 2),
            "{titles:?}"
        );
    }

    /// A newer engine's entry kind must degrade to a generic report row —
    /// one unknown kind must never fail the whole frame's decode and take
    /// the usage numbers down with the report.
    #[test]
    fn an_unknown_entry_kind_degrades_to_a_generic_row_not_a_failed_frame() {
        let _locale = crate::i18n::tests::locale_lock();
        let usage = decode(json!({
            "gross": 100,
            "byKind": {"turn": {"input": 100}},
            "recordCount": 1,
            "occupancy": {"tokens": 100, "contextWindow": 1000, "estimated": false},
            "context": {
                "model": "gpt-test",
                "preview": false,
                "entries": [
                    {
                        "id": "future",
                        "kind": "someFutureKind",
                        "name": "future entry",
                        "content": "opaque",
                        "estimatedTokens": 5
                    }
                ]
            }
        }));
        assert_eq!(usage.gross, 100, "the numbers survive the unknown kind");
        let snapshot = usage.context.as_ref().unwrap();
        assert_eq!(
            snapshot.entries[0].kind,
            holt_proto::ChatContextEntryKind::Unknown
        );
        // The report shows the entry by its carried name instead of dropping
        // it — under the conversation, like every other message-shaped row
        // (here: inside the pre-first-user-message group).
        let sections = ContextUsagePanel::build_sections(snapshot, &Theme::default());
        let conversation = sections
            .iter()
            .find(|section| section.id == "conversation")
            .expect("the conversation section builds");
        let leaves: Vec<&str> = conversation
            .children
            .iter()
            .flat_map(|group| group.children.iter())
            .map(|item| item.title.as_str())
            .collect();
        assert!(leaves.contains(&"future entry"), "{leaves:?}");
    }

    #[test]
    fn the_summary_names_the_checkout_and_prints_occupancy() {
        let _locale = crate::i18n::tests::locale_lock();
        assert_eq!(
            checkout_label(Some("/work/holt/"), Some("main")),
            "holt · main"
        );
        assert_eq!(checkout_label(Some("/work/holt"), None), "holt");
        assert_eq!(
            checkout_label(None, None),
            t!("conversation_transcript.usage.report.no_repo")
        );

        let measured = Occupancy {
            tokens: 18_164,
            window: Some(272_000),
            estimated: false,
        };
        assert_eq!(context_value(&measured), compact_tokens(272_000));
        assert_eq!(used_value(&measured), "18,164 · 6.7%");
        let estimated = Occupancy {
            estimated: true,
            ..measured
        };
        assert_eq!(used_value(&estimated), "≈18,164 · ≈6.7%");
        let unknown = Occupancy {
            window: None,
            ..measured
        };
        assert_eq!(
            context_value(&unknown),
            t!("conversation_transcript.usage.window_unknown")
                .trim_start_matches('·')
                .trim()
        );
        assert_eq!(used_value(&unknown), "18,164");
    }

    #[test]
    fn the_bars_share_a_rounded_token_axis() {
        let categories = [
            ("system", "System prompt", 25),
            ("tools", "Tool definitions", 0),
            ("conversation", "Conversation", 75),
        ]
        .map(|(key, label, tokens)| ContextCategoryShare {
            key,
            label: label.into(),
            tokens,
            color: category_color(key),
        });
        let bars = category_bars(&categories);
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].label, "Conversation");
        assert!((bars[0].share - 0.9375).abs() < 1e-5);
        assert_eq!(bars[1].label, "System prompt");
        assert!((bars[1].share - 0.3125).abs() < 1e-5);
        assert_eq!(chart_ceiling(6_200), 7_000);
        assert_eq!(chart_ceiling(93_000), 100_000);
        assert_eq!(chart_ceiling(18_000), 20_000);
        assert_eq!(chart_ceiling(0), 1);
    }

    #[test]
    fn comma_tokens_inserts_thousands_separators() {
        assert_eq!(comma_tokens(0), "0");
        assert_eq!(comma_tokens(71), "71");
        assert_eq!(comma_tokens(564), "564");
        assert_eq!(comma_tokens(1_729), "1,729");
        assert_eq!(comma_tokens(80_312), "80,312");
        assert_eq!(comma_tokens(1_234_567), "1,234,567");
    }

    #[test]
    fn split_system_prompt_extracts_rules_and_excludes_skills_from_base_tokens() {
        let prompt = "Base instructions here.\n\nProject instructions from /repo/AGENTS.md:\nAlways run tests.\n\nThe following skills provide specialized instructions\n<available_skills>\n<skill><name>demo</name></skill>\n</available_skills>";
        let (base_tokens, rules) = split_system_prompt(prompt, 999, Some("/repo/project"));
        assert_eq!(
            base_tokens,
            estimate_utf16_tokens("Base instructions here.")
        );
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].path, "/repo/AGENTS.md");
        assert_eq!(rules[0].content, "Always run tests.");
        assert!(rules[0].estimated_tokens > 0);
        // A walked AGENTS.md above the chat's folder is a project rule.
        assert!(!rules[0].is_user_rule);
    }

    /// The classification compares engine-side strings only: a rule off the
    /// cwd's ancestor chain is user-level even when the UI machine's `HOME`
    /// says nothing about the engine's paths (a remote engine's `/home/...`
    /// must not be judged by the local `$HOME`).
    #[test]
    fn rules_off_the_cwd_chain_are_user_rules_regardless_of_the_local_home() {
        let prompt =
            "Base.\n\nProject instructions from /remote/home/dev/.claude/CLAUDE.md:\nBe terse.";
        let (_, rules) = split_system_prompt(prompt, 1, Some("/remote/work/project"));
        assert_eq!(rules.len(), 1);
        assert!(rules[0].is_user_rule, "the personal rule is off the chain");

        // The same path ON the chain (an AGENTS.md the walk picked up above
        // the chat's folder) stays a project rule.
        let prompt = "Base.\n\nProject instructions from /remote/home/AGENTS.md:\nBe terse.";
        let (_, rules) = split_system_prompt(prompt, 1, Some("/remote/home/work/project"));
        assert!(!rules[0].is_user_rule);
    }

    #[gpui::test]
    fn the_context_report_expands_prompt_tools_and_history(cx: &mut TestAppContext) {
        let _locale = crate::i18n::tests::locale_lock();
        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| AppState::new());
        state.update(cx, |state, cx| {
            state.chat_usage = Some(ChatUsage {
                context: Some(Arc::new(holt_proto::ChatContextSnapshot {
                    model: "gpt-test".into(),
                    cwd: None,
                    preview: false,
                    loading_mcp_servers: Vec::new(),
                    entries: vec![
                        holt_proto::ChatContextEntry {
                            id: "system".into(),
                            kind: holt_proto::ChatContextEntryKind::SystemPrompt,
                            name: None,
                            content: "actual system text\n\nProject instructions from /workspace/AGENTS.md:\nproject rules body".into(),
                            estimated_tokens: 20,
                            children: Vec::new(),
                        },
                        holt_proto::ChatContextEntry {
                            id: "tools".into(),
                            kind: holt_proto::ChatContextEntryKind::ToolDefinitions,
                            name: None,
                            content: String::new(),
                            estimated_tokens: 5,
                            children: vec![holt_proto::ChatContextEntry {
                                id: "tool-0".into(),
                                kind: holt_proto::ChatContextEntryKind::Tool,
                                name: Some("read".into()),
                                content: "actual read schema".into(),
                                estimated_tokens: 5,
                                children: Vec::new(),
                            }],
                        },
                        holt_proto::ChatContextEntry {
                            id: "message-0".into(),
                            kind: holt_proto::ChatContextEntryKind::UserMessage,
                            name: None,
                            content: "actual user text".into(),
                            estimated_tokens: 3,
                            children: Vec::new(),
                        },
                        holt_proto::ChatContextEntry {
                            id: "message-1".into(),
                            kind: holt_proto::ChatContextEntryKind::AssistantMessage,
                            name: None,
                            content: "actual assistant text".into(),
                            estimated_tokens: 4,
                            children: Vec::new(),
                        },
                    ],
                })),
                ..Default::default()
            });
            cx.notify();
        });
        let window = cx.add_window({
            let state = state.clone();
            move |_, cx| ContextUsagePanel::new(state, cx)
        });
        let mut visual = gpui::VisualTestContext::from_window(*window, cx);
        visual.simulate_resize(gpui::size(px(1920.0), px(1600.0)));
        window
            .update(cx, |panel, _, _| {
                let snapshot = panel.snapshot.as_ref().expect("snapshot");
                let system = ContextUsagePanel::build_sections(snapshot, &Theme::default())
                    .into_iter()
                    .find(|section| section.id == "system")
                    .expect("system section");
                assert_eq!(system.content, snapshot.entries[0].content);
            })
            .unwrap();
        // Top-level categories start expanded by default with their descriptions
        // and child rows visible; leaf items start collapsed until clicked.
        assert!(visual.debug_bounds("context-content-system").is_some());
        assert!(visual.debug_bounds("context-entry-tool-0").is_some());
        assert!(visual.debug_bounds("context-entry-rules").is_some());
        assert!(visual.debug_bounds("context-entry-rule-project").is_some());
        assert!(visual.debug_bounds("context-entry-conversation").is_some());
        // The conversation history is grouped into turns; the turn row is
        // visible but its messages stay hidden until the turn expands.
        assert!(
            visual
                .debug_bounds("context-entry-turn-message-0")
                .is_some()
        );
        assert!(visual.debug_bounds("context-entry-message-0").is_none());
        assert!(visual.debug_bounds("context-content-tool-0").is_none());
        assert!(visual.debug_bounds("context-content-message-0").is_none());

        // The report's horizontal bars and their grid share one plot column;
        // the category labels stay outside it and every row starts together.
        let grid = visual.debug_bounds("context-usage-chart-grid").unwrap();
        let conversation_track = visual
            .debug_bounds("context-usage-track-conversation")
            .unwrap();
        let system_track = visual.debug_bounds("context-usage-track-system").unwrap();
        let conversation_label = visual
            .debug_bounds("context-usage-label-conversation")
            .unwrap();
        let conversation_bar = visual
            .debug_bounds("context-usage-bar-conversation")
            .unwrap();
        assert_eq!(grid.origin.x, conversation_track.origin.x);
        assert_eq!(grid.size.width, conversation_track.size.width);
        assert_eq!(conversation_track.origin.x, system_track.origin.x);
        assert_eq!(conversation_track.size.height, px(20.0));
        assert_eq!(conversation_bar.size.height, conversation_track.size.height);
        assert!(conversation_bar.size.width > px(0.0));
        assert!(conversation_bar.size.width < conversation_track.size.width);
        assert!(
            conversation_label.origin.x + conversation_label.size.width
                <= conversation_track.origin.x
        );
        assert!(visual.debug_bounds("context-usage-chart-axis").is_some());
        assert!(visual.debug_bounds("context-usage-toggle-all").is_none());

        // Clicking a top-level section toggles it closed and back open.
        let system = visual.debug_bounds("context-entry-system").unwrap();
        visual.simulate_click(system.center(), Default::default());
        assert!(visual.debug_bounds("context-content-system").is_none());
        visual.simulate_click(system.center(), Default::default());
        assert!(visual.debug_bounds("context-content-system").is_some());

        // Clicking leaf items expands their content blocks.
        let tool = visual.debug_bounds("context-entry-tool-0").unwrap();
        visual.simulate_click(tool.center(), Default::default());
        assert!(visual.debug_bounds("context-content-tool-0").is_some());

        let rule = visual.debug_bounds("context-entry-rule-project").unwrap();
        visual.simulate_click(rule.center(), Default::default());
        assert!(
            visual
                .debug_bounds("context-content-rule-project")
                .is_some()
        );

        // Expanding a turn reveals its message rows; each message row then
        // expands into its content block.
        let turn = visual.debug_bounds("context-entry-turn-message-0").unwrap();
        visual.simulate_click(turn.center(), Default::default());
        assert!(visual.debug_bounds("context-entry-message-0").is_some());
        assert!(visual.debug_bounds("context-entry-message-1").is_some());

        let message = visual.debug_bounds("context-entry-message-0").unwrap();
        visual.simulate_click(message.center(), Default::default());
        assert!(visual.debug_bounds("context-content-message-0").is_some());
    }

    #[gpui::test]
    fn a_reveal_expands_the_section_and_hands_the_scroll_target_over(cx: &mut TestAppContext) {
        let _locale = crate::i18n::tests::locale_lock();
        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| AppState::new());
        state.update(cx, |state, cx| {
            state.chat_usage = Some(ChatUsage {
                context: Some(Arc::new(holt_proto::ChatContextSnapshot {
                    model: "gpt-test".into(),
                    cwd: None,
                    preview: false,
                    loading_mcp_servers: Vec::new(),
                    entries: vec![
                        holt_proto::ChatContextEntry {
                            id: "system".into(),
                            kind: holt_proto::ChatContextEntryKind::SystemPrompt,
                            name: None,
                            content: "system body".into(),
                            estimated_tokens: 2,
                            children: Vec::new(),
                        },
                        holt_proto::ChatContextEntry {
                            id: "message-0".into(),
                            kind: holt_proto::ChatContextEntryKind::UserMessage,
                            name: None,
                            content: "user text".into(),
                            estimated_tokens: 3,
                            children: Vec::new(),
                        },
                    ],
                })),
                ..Default::default()
            });
            cx.notify();
        });
        let window = cx.add_window({
            let state = state.clone();
            move |_, cx| ContextUsagePanel::new(state, cx)
        });
        let mut visual = gpui::VisualTestContext::from_window(*window, cx);
        // The conversation section starts expanded by default; collapse it so
        // the reveal has real work to do.
        assert!(
            visual
                .debug_bounds("context-entry-turn-message-0")
                .is_some()
        );
        let conversation = visual.debug_bounds("context-entry-conversation").unwrap();
        visual.simulate_click(conversation.center(), Default::default());
        assert!(
            visual
                .debug_bounds("context-entry-turn-message-0")
                .is_none()
        );

        // Revealing re-expands the section and spends the pending scroll
        // target on the next paint.
        window
            .update(cx, |panel, _, cx| {
                panel.reveal_section("conversation", cx);
                assert!(panel.pending_reveal.is_some());
            })
            .unwrap();
        cx.run_until_parked();
        assert!(
            visual
                .debug_bounds("context-entry-turn-message-0")
                .is_some()
        );
        let revealed = cx.read(|cx| window.read(cx).unwrap().pending_reveal.clone());
        assert!(revealed.is_none());

        // A section the snapshot lacks keeps its reveal queued until it shows
        // up (the preview may still be loading).
        window
            .update(cx, |panel, _, cx| panel.reveal_section("mcp-0", cx))
            .unwrap();
        cx.run_until_parked();
        let (pending, expanded) = cx.read(|cx| {
            let panel = window.read(cx).unwrap();
            (
                panel.pending_reveal.clone(),
                panel.expanded.contains("mcp-0"),
            )
        });
        assert_eq!(pending.as_deref(), Some("mcp-0"));
        assert!(expanded);
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

    fn displayed_occupancy(state: &Entity<AppState>, cx: &TestAppContext) -> Option<Occupancy> {
        cx.read(|cx| state.read(cx).chat_usage.as_ref().map(occupancy_of))
    }

    #[gpui::test]
    fn selected_usage_updates_switches_and_hides_on_version_skew(cx: &mut TestAppContext) {
        let _locale = crate::i18n::tests::locale_lock();
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
        let first = displayed_occupancy(&state, cx).unwrap();
        assert_eq!(first.tokens, 40_000);
        assert_eq!(first.window, Some(1_000_000));
        assert!(first.estimated);
        engine.watches.lock().unwrap()[0]
            .1
            .unbounded_send(frame(824000, None, false))
            .unwrap();
        pump(&runtime, cx);
        // The unknown window drops the percentage and the ring's arc with it.
        let unknown = displayed_occupancy(&state, cx).unwrap();
        assert_eq!(unknown.window, None);
        assert!(!unknown.estimated);
        assert_eq!(unknown.fraction(), 0.0);

        state.update(cx, |state, cx| state.select_chat(Some("b".into()), cx));
        assert!(displayed_occupancy(&state, cx).is_none());
        pump(&runtime, cx);
        assert!(engine.watches.lock().unwrap()[0].1.is_closed());
        assert_eq!(engine.watches.lock().unwrap()[1].0, "b");
        let next = displayed_occupancy(&state, cx).unwrap();
        assert_eq!(next.tokens, 40_000);
        assert_eq!(next.window, Some(1_000_000));
        assert!(next.estimated);

        state.update(cx, |state, cx| {
            state.select_chat(Some("old-engine".into()), cx)
        });
        pump(&runtime, cx);
        assert!(displayed_occupancy(&state, cx).is_none());
        state.update(cx, |state, cx| state.select_chat(None, cx));
        pump(&runtime, cx);
        assert!(displayed_occupancy(&state, cx).is_none());
    }

    /// The two report surfaces — the usage popover and the right-pane tab —
    /// can both ask for a next-request preview while the first is still
    /// building. One request per chat at a time; and the unary reply doubles
    /// as the watch's fallback, landing the snapshot when the frame stream
    /// never carried it.
    #[gpui::test]
    fn preview_requests_dedup_per_chat_and_the_reply_lands_when_the_watch_did_not(
        cx: &mut TestAppContext,
    ) {
        /// `WATCH_CHAT_USAGE` serves frames without a context (the sender
        /// stays alive so the stream does not close); the preview blocks
        /// until the test releases it, recording one channel per call.
        struct GateEngine {
            watches: Mutex<Vec<mpsc::UnboundedSender<Value>>>,
            previews: Mutex<Vec<tokio::sync::oneshot::Sender<Value>>>,
        }
        #[async_trait::async_trait]
        impl RpcService for GateEngine {
            async fn handle(&self, method: &str, _: Value) -> Result<RpcReply, RpcError> {
                if method == methods::WATCH_CHAT_USAGE {
                    let (tx, rx) = mpsc::unbounded();
                    tx.unbounded_send(frame(412000, Some(1000000), true))
                        .unwrap();
                    self.watches.lock().unwrap().push(tx);
                    return Ok(RpcReply::Stream(rx.boxed()));
                }
                if method == methods::PREVIEW_CHAT_CONTEXT {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    self.previews.lock().unwrap().push(tx);
                    let reply = rx
                        .await
                        .map_err(|_| RpcError::Failed("preview gate dropped".into()))?;
                    return Ok(RpcReply::Value(reply));
                }
                Ok(RpcReply::Stream(futures::stream::pending().boxed()))
            }
        }

        let ask = |state: &Entity<AppState>, cx: &mut TestAppContext| {
            state.update(cx, |_, cx| {
                cx.spawn(async move |this, cx| request_context_preview(this, cx).await)
                    .detach();
            });
        };

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let engine = Arc::new(GateEngine {
            watches: Mutex::new(Vec::new()),
            previews: Mutex::new(Vec::new()),
        });
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

        // The popover's ask: one preview starts and parks on the gate.
        ask(&state, cx);
        pump(&runtime, cx);
        assert_eq!(engine.previews.lock().unwrap().len(), 1);
        assert_eq!(
            cx.read(|cx| state.read(cx).context_preview_pending.clone()),
            Some("a".to_string())
        );

        // The report tab's ask while the first is still in flight: deduped
        // by the pending marker — no second build.
        ask(&state, cx);
        pump(&runtime, cx);
        assert_eq!(
            engine.previews.lock().unwrap().len(),
            1,
            "an in-flight preview absorbs the second surface's ask"
        );

        // The reply lands even though the watch's frames never carried a
        // context: the graft is the stream's fallback.
        let gate = engine.previews.lock().unwrap().remove(0);
        gate.send(json!({
            "model": "gpt-gated",
            "preview": true,
            "entries": [],
            "loadingMcpServers": [],
        }))
        .unwrap();
        pump(&runtime, cx);
        cx.read(|cx| {
            let state = state.read(cx);
            assert_eq!(state.context_preview_pending, None);
            let usage = state.chat_usage.as_ref().unwrap();
            let snapshot = usage.context.as_ref().unwrap();
            assert_eq!(snapshot.model, "gpt-gated");
            assert!(snapshot.preview);
        });

        // A landed snapshot answers the next ask: still no second build (the
        // one recorded call was already consumed above).
        ask(&state, cx);
        pump(&runtime, cx);
        assert!(
            engine.previews.lock().unwrap().is_empty(),
            "a landed snapshot answers the next ask without a new build"
        );
    }

    #[gpui::test]
    fn scripted_engine_totals_reach_the_selected_ring(cx: &mut TestAppContext) {
        let _locale = crate::i18n::tests::locale_lock();
        use super::engine_fixture::{self, Fixture, ScriptedProvider, ScriptedReply};
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let fixture = Fixture::new();
        let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]).with_usage(
            holt_core::ai::types::Usage {
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
            // Pinned end to end: gross 700 + 70 + 7 + 3 with one record, and
            // the occupancy the main run's own request input plus both cache
            // fields (700 + 7 + 3).
            assert_eq!(usage.gross, 780);
            assert_eq!(usage.record_count, 1);
            let occupancy = occupancy_of(usage);
            assert_eq!(occupancy.tokens, 710);
            assert!(!occupancy.estimated);
            // Prompt 700 + 7 read + 3 written; only the reads hit.
            let turn = &usage.by_kind["turn"];
            assert_eq!(turn.input + turn.cache_read + turn.cache_write, 710);
            assert_eq!(turn.cache_read, 7);
            assert_eq!(turn.cache_write, 3);
            // Run acceptance stores the request's model on the chat, so a
            // settled queue divides by that model's catalog window — 272k for
            // this one. The UI's job is to print what the frame carries; the
            // catalog number itself is the engine's to know.
            let window = occupancy.window.unwrap();
            assert!(window > 0);
            assert!((occupancy.fraction() - 710.0 / window as f32).abs() < 1e-6);
        });
    }
}
