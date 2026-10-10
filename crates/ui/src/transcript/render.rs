//! GPUI rendering for the transcript: `render_row` and the shared chip
//! builders, `impl Render for Transcript`, and the paint-side helpers
//! (frame-stats knobs, the ADR-0013 scroll chaining). Row kinds live in
//! submodules: `user_rows` (skill invocation, compaction divider, bubble
//! strips, working trailer), `turn_card` (ADR-0024 change card),
//! `tool_group` (nested chips, detail bodies, subagent tabs), `highlight`
//! (background syntax store). Entity state, sync, and events stay in the
//! facade; this module only reads them.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, BorderStyle, Bounds, ClipboardItem, Context, CursorStyle, Focusable, KeyDownEvent,
    MouseButton, ObjectFit, SharedString, StyledImage as _, StyledText, Task, TextRun, Window,
    canvas, div, img, list, point, prelude::*, px, quad, size,
};
use holt_doc::{MessagePart, MessageRole, MessageStatus, SubagentStatus, ToolGateState};
use holt_proto::ToolCall;
use holt_proto::TurnChangeSet;

use crate::state::TurnRestoreMark;
use holt_proto::TurnFileChange;
use holt_proto::TurnFileChangeStatus;
use holt_proto::view::tool_chip_content_in;

use super::model::{
    Row, RowKind, ToolItem, UserSkill, fnv1a, format_skill_title, format_timestamp, is_agent_call,
    is_spawn_link, skill_file_display, tool_group_collapses, top_gap_for,
};
use super::question_card;
use super::tool::{
    BLOB_AFFORDANCE_HEIGHT, CHIP_GAP, CHIP_HEIGHT, CHIPS_TOP_PAD, ChipAffordance,
    OUTPUT_LINE_HEIGHT, ToolDetail, chips_height, detail_height, format_kb, tool_group_summary,
};
use super::viewport::{
    SCROLL_BUTTON_THRESHOLD_PX, ViewportFinalizeToken, flavour_seed, flavour_word, format_elapsed,
    sending_bridge,
};
use super::{BlobFetch, FOLD_TWEEN_WINDOW, FoldState, Transcript, TranscriptEvent};
use crate::markdown::parser::{Block, BlockTree, InlineRun};
use crate::markdown::render::{self, RenderOptions};
use crate::markdown::veil::RowVeil;
use crate::motion::{self, AnimationExt as _, RESIZE};
use crate::syntax_cache::{DocumentHighlightKey, SyntaxHighlightCache};
use crate::theme::Theme;
use holt_syntax::LanguageId as Lang;

mod highlight;
mod tool_group;
mod turn_card;
mod user_rows;

pub(super) use highlight::HighlightStore;

/// Transcript column max width (holt 46rem).
pub const MAX_CONTENT_WIDTH: f32 = 736.0;

/// User-bubble attachment thumbnails (user-attachments.tsx): 112×80 thumbs in
/// a FIXED-height strip (load-state flips never shift the virtualizer).
pub const ATT_THUMB_W: f32 = 112.0;
pub const ATT_THUMB_H: f32 = 80.0;
pub const ATT_STRIP_H: f32 = ATT_THUMB_H + 10.0;

/// `HOLT_FRAME_STATS=1` logs live-row render-cost percentiles (p50/p95 µs
/// over rolling windows of [`FRAME_STATS_WINDOW`] samples) at `warn` level —
/// the smoothness measurement knob. Off by default; zero cost when off.
fn frame_stats_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var("HOLT_FRAME_STATS").is_ok_and(|v| !v.is_empty() && v != "0"))
}

const FRAME_STATS_WINDOW: usize = 240;

/// `HOLT_NO_RENDER_CACHE=1` bypasses the cross-frame flatten cache — the
/// A/B knob for the frame-cost measurement above.
fn render_cache_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| {
        std::env::var("HOLT_NO_RENDER_CACHE").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

fn record_live_frame_us(us: u64) {
    thread_local! {
        static SAMPLES: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    }
    SAMPLES.with(|s| {
        let mut s = s.borrow_mut();
        s.push(us);
        if s.len() >= FRAME_STATS_WINDOW {
            s.sort_unstable();
            let p50 = s[s.len() / 2];
            let p95 = s[s.len() * 95 / 100];
            let max = *s.last().unwrap();
            tracing::warn!(
                n = s.len(),
                p50_us = p50,
                p95_us = p95,
                max_us = max,
                "live-row render cost"
            );
            s.clear();
        }
    });
}

/// ADR-0013 chaining for a nested reading viewport (skill body, compaction
/// summary) inside the transcript list: the occluded div owns the wheel only
/// while it can move, and whatever it could NOT absorb is forwarded to the
/// outer list so the gesture continues instead of dead-ending at the
/// boundary. The div's built-in scroll listener applies the delta
/// (unclamped) before bubble dispatch reaches this handler, so the tracked
/// offset sitting past its clamp IS the unabsorbed remainder. The handle is
/// written back clamped so a second wheel event in the same frame cannot
/// re-forward the same overshoot. Returns the forwarded distance — zero when
/// the body absorbed the whole delta.
pub(super) fn forward_scroll_remainder(
    list: &gpui::ListState,
    handle: &gpui::ScrollHandle,
) -> gpui::Pixels {
    let offset = handle.offset();
    let clamped_y = offset.y.clamp(-handle.max_offset().y, px(0.0));
    let remainder = offset.y - clamped_y;
    if remainder != px(0.0) {
        handle.set_offset(gpui::point(offset.x, clamped_y));
        list.scroll_by(-remainder);
    }
    remainder
}

impl Transcript {
    /// The row-external vertical pads `render_row` applies at `ix`: the top
    /// turn gap and the bottom clearance. Shared with the change card's
    /// fold, whose closed painted base is card chrome + these pads —
    /// computed from the SAME live state at click time rather than cached
    /// from a render, so a click can never inherit a stale geometry.
    pub(super) fn row_outer_pads(&self, ix: usize, row: &Row) -> (f32, f32) {
        // The viewport spans the full window (under the titlebar): the first
        // row's gap adds the titlebar's height so a top-scrolled transcript
        // rests below the chrome it fades under. The right pane already pads
        // for the titlebar — an override instance's first row keeps only the
        // ordinary turn gap, or the content sits double-chrome low.
        let top_gap = if ix == 0 {
            if self.doc_override.is_some() {
                Theme::SPACE_LG
            } else {
                Theme::TITLEBAR_HEIGHT + Theme::SPACE_LG + 10.0
            }
        } else {
            top_gap_for(ix.checked_sub(1).and_then(|i| self.rows.get(i)), row)
        };
        // The last row must clear the composer/status stack the transcript
        // scrolls under PLUS the fade band above it, or the timestamp strip
        // (the row's lowest content) renders half-faded (or hidden) when the
        // transcript is pinned to the bottom.
        let bottom_pad = if ix + 1 == self.rows.len() {
            let runway = self
                .own_turn
                .as_ref()
                .filter(|anchor| {
                    self.rows
                        .iter()
                        .any(|candidate| candidate.entry_id == anchor.message_id)
                })
                .map_or(0.0, |anchor| anchor.runway);
            self.bottom_clearance + Theme::TRANSCRIPT_FADE_BAND + 8.0 + runway
        } else {
            0.0
        };
        (top_gap, bottom_pad)
    }

    fn render_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(ix).cloned() else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        let (top_gap, bottom_pad) = self.row_outer_pads(ix, &row);
        // Live-run loader rides under the LAST row's content (above its
        // clearance pad), so it sits right beneath the working reply.
        let trailer = (ix + 1 == self.rows.len())
            .then(|| self.render_working_trailer(cx))
            .flatten();

        let inner: AnyElement = match &row.kind {
            RowKind::User {
                text,
                raw,
                mentions,
                attachments,
                badges,
                skill,
                pending,
            } => {
                let attachments = attachments.clone();
                let badges = badges.clone();
                let text = text.clone();
                let raw = raw.clone();
                let mentions = mentions.clone();
                let skill = skill.clone();
                let pending = *pending;
                let mut targets: Vec<crate::image_viewer::ViewerTarget> = attachments
                    .iter()
                    .map(|a| crate::image_viewer::ViewerTarget {
                        path: a.path.clone().into(),
                        label: a.name.clone().into(),
                    })
                    .collect();
                for mention in mentions
                    .iter()
                    .filter(|m| !m.is_dir && crate::images::is_image_path(&m.path))
                {
                    if !targets.iter().any(|t| t.path == mention.path) {
                        targets.push(crate::image_viewer::ViewerTarget {
                            path: mention.path.clone(),
                            label: std::path::Path::new(mention.path.as_ref())
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned()
                                .into(),
                        });
                    }
                }
                let weak = cx.weak_entity();
                let inline_targets = targets.clone();
                let image_open: ImageOpen = Rc::new(move |path, window, cx| {
                    if let Some(index) = inline_targets.iter().position(|t| t.path.as_ref() == path)
                    {
                        weak.update(cx, |this, cx| {
                            this.open_image_viewer(inline_targets.clone(), index, window, cx)
                        })
                        .ok();
                    }
                });
                // Skill mention chips open the SKILL.md in the sidebar's file
                // tab — the same shell-facing event the invocation chip uses,
                // never an external open.
                let skill_weak = cx.weak_entity();
                let skill_open: SkillOpen = Rc::new(move |path, _window, cx| {
                    skill_weak
                        .update(cx, |_, cx| {
                            cx.emit(super::TranscriptEvent::OpenSkillFile {
                                path: path.to_string(),
                            });
                        })
                        .ok();
                });
                // Chip-projected bubbles copy canonical Markdown: the raw
                // body plus a display→raw chip table (ADR-0035).
                let copy_map =
                    (!mentions.is_empty()).then(|| crate::markdown::selection::CopyMap {
                        raw: raw.to_string(),
                        chips: mentions
                            .iter()
                            .map(|span| (span.range.clone(), span.raw_range.clone()))
                            .collect(),
                    });
                // Attachment thumbnails ride ABOVE the bubble, right-aligned
                // (chat-view.tsx RowView: UserAttachmentStrip then the text
                // HStack); image-only sends show no bubble at all.
                let mut column = div().w_full().flex().flex_col();
                // While this entry is being edited the static attachment
                // presentation (thumbnails, badges) swaps for the edit
                // session's removable chip strip: the editor owns the body,
                // the chips own the lifted path list, and Send recombines.
                let edit_refs = self
                    .message_edit
                    .as_ref()
                    .filter(|edit| edit.message_id == row.entry_id.as_ref())
                    .map(|edit| edit.references.clone());
                if let Some(refs) = edit_refs.as_ref() {
                    if !refs.is_empty() {
                        let mut strip = div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .justify_end()
                            .items_center()
                            .gap(px(6.0))
                            .pb(px(6.0));
                        for (cix, reference) in refs.iter().enumerate() {
                            let path = reference.path.clone();
                            strip = strip.child(crate::badges::render_removable_ref(
                                SharedString::from(format!("{}#edit-ref{cix}", row.id)),
                                SharedString::from(format!("{}#edit-ref-remove{cix}", row.id)),
                                reference,
                                &theme,
                                cx.listener(move |this, _, _, cx| {
                                    this.remove_message_edit_reference(&path, cx);
                                }),
                            ));
                        }
                        column = column.child(strip);
                    }
                } else {
                    if !attachments.is_empty() {
                        column = column.child(self.render_user_attachments(
                            &row.id,
                            &attachments,
                            targets,
                            cx,
                        ));
                    }
                    if !badges.is_empty() {
                        column = column.child(
                            div()
                                .w_full()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .justify_end()
                                .items_center()
                                .gap(px(6.0))
                                .pb(px(6.0))
                                .children(badges.iter().enumerate().map(|(bix, badge)| {
                                    crate::badges::render(
                                        SharedString::from(format!("{}#badge{bix}", row.id)),
                                        badge,
                                        &theme,
                                    )
                                })),
                        );
                    }
                }
                if !text.is_empty()
                    || skill.is_some()
                    || self
                        .message_edit
                        .as_ref()
                        .is_some_and(|edit| edit.message_id == row.entry_id.as_ref())
                {
                    let inline_editor = self
                        .message_edit
                        .as_ref()
                        .filter(|edit| edit.message_id == row.entry_id.as_ref())
                        .map(|edit| edit.input.clone());
                    let edit_state = self
                        .message_edit
                        .as_ref()
                        .filter(|edit| edit.message_id == row.entry_id.as_ref())
                        .map(|edit| (edit.pending, edit.error.clone()));
                    if let Some(input) = inline_editor.as_ref()
                        && self
                            .message_edit
                            .as_mut()
                            .is_some_and(|edit| std::mem::take(&mut edit.focus_pending))
                    {
                        // First edit frame: measure now — at the width the
                        // row's gutters and the bubble's `max_w` cap leave
                        // the input — so the explicit wrapper height below
                        // is correct on the FIRST paint instead of snapping
                        // one frame later. A stale probe width (mid-resize)
                        // still converges via ViewportChanged.
                        let list_width = self.list_width.get();
                        let seed_width = (list_width - 96.0).min(MAX_CONTENT_WIDTH * 0.8) - 32.0;
                        if seed_width > 0.0 {
                            input.update(cx, |input, cx| {
                                input.premeasure(px(seed_width), window, cx)
                            });
                        }
                        window.focus(&input.focus_handle(cx), cx);
                    }
                    let editing = inline_editor.is_some();
                    let bubble_child = if let Some(input) = inline_editor {
                        // taffy measures the leaf at the unclamped row width
                        // before applying the bubble's `max_w` clamp and never
                        // re-aggregates ancestor heights, so a multi-line
                        // draft would overflow the bubble background. Height
                        // the wrapper explicitly from the input's own
                        // measurement (converges via ViewportChanged).
                        let edit_height = input.read(cx).measured_display_height();
                        div()
                            .w_full()
                            .min_w_0()
                            .h(px(edit_height))
                            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                if event.keystroke.key == "escape" {
                                    cx.stop_propagation();
                                    this.cancel_message_edit(cx);
                                }
                            }))
                            .child(input)
                            .into_any_element()
                    } else {
                        let clicks = BubbleClicks {
                            image_open: Some(image_open),
                            skill_open: Some(skill_open),
                            copy_map,
                        };
                        match skill {
                            Some(skill) => self.render_user_skill(
                                &row.id, &skill, &text, &mentions, &theme, clicks,
                            ),
                            None => user_bubble_text_with_chip(
                                &row.id, text, mentions, None, &theme, clicks,
                            )
                            .into_any_element(),
                        }
                    };

                    // `min_w_0` is load-bearing on BOTH the wrapper column (the
                    // justify_end row's flex item) and the bubble inside it: gpui
                    // text answers min/max-content probes with its UNWRAPPED width,
                    // so without it the automatic min-size is the full single-line
                    // width — the flex item can't shrink, `justify_end` pushes the
                    // overflow off the left edge, and long prompts render as one
                    // clipped line instead of wrapping inside the 80% column cap.
                    let bubble = div()
                        .min_w_0()
                        // Editing stretches the bubble to its full cap so the
                        // inline input doesn't shrink-wrap to the text width.
                        .when(editing, |el| el.w_full())
                        .max_w(px(MAX_CONTENT_WIDTH * 0.8))
                        .bg(crate::theme::user_bubble_bg())
                        .rounded(px(Theme::BUBBLE_RADIUS))
                        .px(px(16.0))
                        .py(px(10.0))
                        .text_size(crate::typography::ui_rems(14.0))
                        .line_height(crate::typography::ui_rems(22.0))
                        .text_color(theme.text)
                        .when(pending, |el| el.opacity(0.65))
                        .child(bubble_child);
                    column = column.child(
                        div().w_full().flex().justify_end().child(
                            div()
                                .min_w_0()
                                .when(editing, |el| el.w_full())
                                .flex()
                                .flex_col()
                                .items_end()
                                .child(bubble)
                                .when(editing, |el| {
                                    let (edit_pending, edit_error) =
                                        edit_state.clone().unwrap_or((false, None));
                                    el.when_some(edit_error, |el, error| {
                                        el.child(
                                            div()
                                                .pt(px(6.0))
                                                .text_size(crate::typography::ui_rems(12.0))
                                                .text_color(theme.danger_muted)
                                                .child(error),
                                        )
                                    })
                                    .child(
                                        div()
                                            .pt(px(6.0))
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .gap(px(8.0))
                                            .child(
                                                crate::popover::btn_ghost(
                                                    &theme,
                                                    "Cancel",
                                                    format!("edit-cancel-hover-{}", row.id),
                                                )
                                                .id(SharedString::from(format!(
                                                    "edit-cancel-{}",
                                                    row.id
                                                )))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.cancel_message_edit(cx)
                                                })),
                                            )
                                            .child(
                                                crate::popover::btn_primary(&theme, "Send")
                                                    .id(SharedString::from(format!(
                                                        "edit-send-{}",
                                                        row.id
                                                    )))
                                                    .when(edit_pending, |el| el.opacity(0.5))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.submit_message_edit(cx)
                                                    })),
                                            ),
                                    )
                                }),
                        ),
                    );
                }
                column.into_any_element()
            }
            RowKind::Markdown { tree, block_ix } => {
                let opts = RenderOptions {
                    row_key: row.id.clone(),
                    veil: None,
                    cache: (!render_cache_disabled()).then(|| self.render_cache.clone()),
                    now: Instant::now(),
                    copy: Some(self.copy_ui_for(&row.id, cx)),
                    mermaid_ui: Some(self.mermaid_ui_for(&row.id, tree, cx)),
                };
                let highlight = self.code_highlight_for(&row.id, tree, Some(*block_ix), cx);
                let mermaid = self.mermaid_for(&row.id, tree, Some(*block_ix), cx);
                let Some(top) = tree.blocks.get(*block_ix) else {
                    return gpui::Empty.into_any_element();
                };
                render::render_block(
                    &top.block,
                    *block_ix,
                    *block_ix,
                    &opts,
                    &theme,
                    window,
                    highlight
                        .get(block_ix)
                        .and_then(|o| o.as_deref())
                        .map(|document| document.lines.as_slice()),
                    mermaid.get(block_ix).cloned().flatten().as_ref(),
                )
            }
            RowKind::LiveMarkdown { tree, block_ix } => {
                // Per-appended-chunk fade veil (opacity only — layout commits
                // instantly). Reduced motion renders with no veil at all.
                // Baseline rows (text already streamed when the transcript
                // attached) start seeded: the existing reply must not fade in
                // on a session switch — only fresh appends animate.
                let veil = (!motion::reduced_motion(cx)).then(|| {
                    self.veils
                        .entry(row.id.clone())
                        .or_insert_with(|| {
                            if self.veil_baseline.contains(&row.id) {
                                Rc::new(RefCell::new(RowVeil::seeded()))
                            } else {
                                Rc::default()
                            }
                        })
                        .clone()
                });
                let opts = RenderOptions {
                    row_key: row.id.clone(),
                    veil: veil.clone(),
                    cache: (!render_cache_disabled()).then(|| self.render_cache.clone()),
                    now: Instant::now(),
                    copy: Some(self.copy_ui_for(&row.id, cx)),
                    mermaid_ui: Some(self.mermaid_ui_for(&row.id, tree, cx)),
                };
                let highlight = self.code_highlight_for(&row.id, tree, Some(*block_ix), cx);
                let mermaid = self.mermaid_for(&row.id, tree, Some(*block_ix), cx);
                let Some(top) = tree.blocks.get(*block_ix) else {
                    return gpui::Empty.into_any_element();
                };
                let timer = frame_stats_enabled().then(Instant::now);
                let el = render::render_block(
                    &top.block,
                    *block_ix,
                    *block_ix,
                    &opts,
                    &theme,
                    window,
                    highlight
                        .get(block_ix)
                        .and_then(|o| o.as_deref())
                        .map(|document| document.lines.as_slice()),
                    mermaid.get(block_ix).cloned().flatten().as_ref(),
                );
                if let Some(start) = timer {
                    record_live_frame_us(start.elapsed().as_micros() as u64);
                }
                // The attach pass for this row is done (every element rendered
                // above seeded its baseline synchronously): elements appearing
                // from the NEXT pass on are newly streamed and fade normally.
                if let Some(veil) = &veil {
                    veil.borrow_mut().finish_seeding();
                }
                // Drive the veil clock: while any chunk is still dissolving,
                // repaint next frame (self-limiting — one callback per frame).
                if veil.is_some_and(|v| v.borrow().is_fading()) {
                    let id = cx.entity_id();
                    window.on_next_frame(move |_, cx| cx.notify(id));
                }
                el
            }
            RowKind::ToolGroup { tools, auto_open } => {
                self.render_tool_group(&row.id, tools, *auto_open, &theme, cx)
            }
            RowKind::InputChip { header, resolved } => {
                input_chip(header.clone(), *resolved, &theme)
            }
            RowKind::SkillChip {
                name,
                file,
                content,
                pending,
            } => match content {
                // An invocation: the collapsible process row that opens the
                // agent's reply — expand to see the exact `<skill>` block
                // the model received, thinking-style.
                Some(content) => {
                    self.render_skill_invocation(&row.id, name, file, content, *pending, &theme, cx)
                }
                // A read collapse: the compact pointer chip.
                None => {
                    let open = (!file.is_empty()).then(|| {
                        let path = file.trim_start_matches("file://").to_string();
                        cx.listener(move |_, _, _, cx| {
                            cx.emit(super::TranscriptEvent::OpenSkillFile { path: path.clone() });
                        })
                    });
                    skill_chip(name.clone(), file.clone(), *pending, open, &theme)
                }
            },
            RowKind::ErrorChip { message } => error_chip(message.clone(), &theme),
            RowKind::RetryChip {
                attempt,
                max_retries,
                delay_secs,
                error,
            } => retry_chip(*attempt, *max_retries, *delay_secs, error.clone(), &theme),
            RowKind::Notice { message } => notice_row(message.clone(), &theme),
            RowKind::GoalEnd { message } => goal_end_row(message.clone(), &theme),
            RowKind::CompactionDivider { summary } => {
                self.render_compaction_divider(&row.id, summary, &theme, window, cx)
            }
            RowKind::PlanApproval { content, state } => {
                self.render_plan_approval_card(&row.id, content, state, &theme)
            }
            RowKind::ModelProposal {
                proposal_id,
                targets,
                summary,
                lines,
                state,
            } => self.render_model_proposal_card(
                &row.id,
                proposal_id,
                targets,
                summary,
                lines,
                *state,
                &theme,
                cx,
            ),
            RowKind::ProviderChoice {
                card_id,
                options,
                chosen,
                state,
            } => self.render_provider_choice_card(
                &row.id,
                card_id,
                options,
                chosen.as_ref(),
                *state,
                &theme,
                cx,
            ),
            RowKind::QuestionCard {
                questions,
                answers,
                state,
            } => question_card::render_question_card(questions, answers, *state, &theme),
            RowKind::KeyRequest {
                provider_name,
                destination,
                state,
            } => self.render_key_request_card(
                &row.id,
                provider_name,
                destination,
                *state,
                &theme,
                cx,
            ),
            RowKind::TurnChangeCard {
                change_set,
                restore,
            } => self.render_turn_change_card(&row.id, change_set, *restore, &theme, cx),
        };

        // Hover-revealed metadata strip: a RESERVED 32px lane under the
        // entry's last row. Timestamp, copy action, and copied feedback only
        // flip visibility/content, so none of them shifts the virtualizer.
        // User entries align end (under the bubble), assistant entries start.
        // Both read timestamp first, then the copy action.
        let is_user_row = matches!(row.kind, RowKind::User { .. });
        let hovered = self
            .hovered_entry
            .as_ref()
            .is_some_and(|(_, entry)| entry == &row.entry_id);
        let copied_message = self.copied_message.as_ref() == Some(&row.entry_id);
        let copy_text = row.copy_text.clone();
        let copy_entry_id = row.entry_id.clone();
        let edit_text = if is_user_row
            && self.chat_id.is_some()
            && self.doc_override.is_none()
            && self
                .state
                .read(cx)
                .transcript
                .iter()
                .rev()
                .find(|entry| entry.role == MessageRole::User)
                .is_some_and(|entry| entry.id == row.entry_id.as_ref())
        {
            self.state
                .read(cx)
                .transcript
                .iter()
                .find(|entry| entry.id == row.entry_id.as_ref())
                .map(|entry| {
                    entry
                        .parts
                        .iter()
                        .filter_map(|part| match part {
                            MessagePart::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n")
                })
        } else {
            None
        };
        let edit_entry_id = row.entry_id.clone();
        let edit_chat_id = self.chat_id.clone().unwrap_or_default();
        // While this entry is being edited inline its hover strip is
        // replaced by the editor's own Cancel/Send affordance — no
        // timestamp, edit, or copy affordances underneath it.
        let strip = row
            .timestamp
            .filter(|_| {
                self.message_edit
                    .as_ref()
                    .is_none_or(|edit| edit.message_id != row.entry_id.as_ref())
            })
            .map(|ms| {
                let timestamp = div()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted.opacity(0.55))
                    .child(SharedString::from(format_timestamp(ms, &chrono::Local)));
                let copy = copy_text.map(|text| {
                    let entry_id = copy_entry_id.clone();
                    let fade_key = format!("copy-message-hover-{entry_id}");
                    div()
                        .id(SharedString::from(format!("copy-message-{entry_id}")))
                        .size(px(Theme::SPACE_MD * 2.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .cursor_pointer()
                        // Same quiet icon-button treatment as the copy action
                        // over transcript code blocks.
                        .bg(motion::hover_blend(
                            &fade_key,
                            gpui::transparent_black(),
                            crate::theme::ink(0.08),
                        ))
                        .on_hover(motion::hover_listener(fade_key))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.copy_message(entry_id.clone(), text.clone(), cx)
                        }))
                        .child(
                            crate::icons::icon(if copied_message {
                                crate::icons::CHECK
                            } else {
                                crate::icons::COPY
                            })
                            .size(px(14.0))
                            .text_color(theme.text_muted),
                        )
                });
                let metadata = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_SM));
                let edit = edit_text.map(|text| {
                    let entry_id = edit_entry_id.clone();
                    let chat_id = edit_chat_id.clone();
                    let fade_key = format!("edit-message-hover-{entry_id}");
                    div()
                        .id(SharedString::from(format!("edit-message-{entry_id}")))
                        .size(px(Theme::SPACE_MD * 2.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .cursor_pointer()
                        .bg(motion::hover_blend(
                            &fade_key,
                            gpui::transparent_black(),
                            crate::theme::ink(0.08),
                        ))
                        .on_hover(motion::hover_listener(fade_key))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(TranscriptEvent::EditLastMessage {
                                chat_id: chat_id.clone(),
                                message_id: entry_id.to_string(),
                                text: text.clone(),
                            });
                        }))
                        .child(
                            crate::icons::icon(crate::icons::PEN)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        )
                });
                let metadata = metadata.child(timestamp).children(edit).children(copy);
                div()
                    .h(px(Theme::SPACE_SM + Theme::SPACE_MD * 2.0))
                    .pt(px(Theme::SPACE_SM))
                    .w_full()
                    .flex()
                    .items_center()
                    // No horizontal inset: the original's `px-1` netted out flush
                    // because its message text was inset by the same amount (group
                    // padding 4 + inner VStack 4 = 8 = group 4 + px-1 4). Here the
                    // markdown text / user bubble sit AT the content column edges,
                    // so the label must too — assistant label's left edge on the
                    // text's first-character x, user label's right edge on the
                    // bubble's right edge (user-reported 4px drift).
                    .when(is_user_row, |el| el.justify_end())
                    .when(hovered, |el| {
                        el.child(motion::fade_quick(
                            SharedString::from(format!("meta-{}", row.id)),
                            metadata,
                        ))
                    })
            });
        let caption = (is_user_row
            && self.doc_override.is_none()
            && self
                .rows
                .iter()
                .position(|r| matches!(r.kind, RowKind::User { .. }))
                == Some(ix))
        .then(|| self.render_run_caption(&theme, cx))
        .flatten();
        let entry_id = row.entry_id.clone();
        let row_id = row.id.clone();
        div()
            .id(row.id.clone())
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    let next = Some((row_id.clone(), entry_id.clone()));
                    if this.hovered_entry != next {
                        let entry_changed = this
                            .hovered_entry
                            .as_ref()
                            .is_none_or(|(_, entry)| entry != &entry_id);
                        this.hovered_entry = next;
                        if entry_changed {
                            cx.notify();
                        }
                    }
                } else if this
                    .hovered_entry
                    .as_ref()
                    .is_some_and(|(row, _)| row == &row_id)
                {
                    // Only the row that OWNS the current reveal may clear it —
                    // a stale leave from an earlier row must not blank the
                    // strip the newly entered row just lit.
                    this.hovered_entry = None;
                    cx.notify();
                }
            }))
            .w_full()
            .flex()
            .justify_center()
            .pt(px(top_gap))
            .pb(px(bottom_pad))
            // Wide gutters (holt `px-4 @3xl:px-12`) around the 46rem column.
            .px(px(48.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(MAX_CONTENT_WIDTH))
                    .min_w_0()
                    .children(caption)
                    .child(inner)
                    .children(strip)
                    .children(trailer),
            )
            .into_any_element()
    }

    /// A Routine run chat's caption over its first message, read from the
    /// Chat's run marker (never agent events): "◷ name", "· 补跑" for a
    /// Catch-up run. Clicking opens the Routine while it still exists.
    fn render_run_caption(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.state.read(cx);
        let marker = state.selected_chat_row()?.routine_run.clone()?;
        let exists = state
            .routines
            .iter()
            .any(|view| view.routine.id == marker.routine_id);
        let mut label = marker.routine_name.clone();
        if marker.missed_fires > 0 {
            label.push_str(" \u{b7} 补跑");
        }
        let routine_id = marker.routine_id;
        let caption = div()
            .id("run-caption")
            .debug_selector(|| "run-caption".into())
            .max_w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.0))
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.text_muted.opacity(0.7))
            .child(
                crate::icons::icon(crate::icons::CLOCK_CIRCLE)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(div().min_w_0().truncate().child(SharedString::from(label)))
            .when(exists, |el| {
                el.cursor_pointer()
                    .hover(|s| s.text_color(theme.text))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(TranscriptEvent::OpenRoutine {
                            routine_id: routine_id.clone(),
                        });
                    }))
            });
        Some(
            div()
                .w_full()
                .flex()
                .justify_end()
                .pb(px(6.0))
                .child(caption)
                .into_any_element(),
        )
    }

    /// Copy-button wiring for one row's code blocks ([`render::CopyUi`]):
    /// click writes the block's code to the clipboard and shows a transient
    /// "Copied" check on that block for ~1.2s (overlay — no layout shift).
    fn copy_ui_for(&self, row_id: &SharedString, cx: &mut Context<Self>) -> render::CopyUi {
        let copied_ix = self
            .copied_code
            .as_ref()
            .filter(|(id, _)| id == row_id)
            .map(|(_, ix)| *ix);
        let row_key = row_id.clone();
        let entity = cx.weak_entity();
        let handler: Rc<render::CopyHandler> = Rc::new(move |ix, code, _window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(code.to_string()));
            let row_key = row_key.clone();
            entity
                .update(cx, |this, cx| {
                    this.copied_code = Some((row_key, ix));
                    this.copied_clear = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(1200))
                            .await;
                        this.update(cx, |this, cx| {
                            this.copied_code = None;
                            this.copied_clear = None;
                            cx.notify();
                        })
                        .ok();
                    }));
                    cx.notify();
                })
                .ok();
        });
        render::CopyUi { handler, copied_ix }
    }

    /// Request highlights for the code blocks of a tree. `only` limits to one
    /// block index (split rows); `None` covers the whole tree (live rows).
    fn code_highlight_for(
        &mut self,
        row_id: &SharedString,
        tree: &Arc<BlockTree>,
        only: Option<usize>,
        cx: &mut Context<Self>,
    ) -> HashMap<usize, Option<Arc<holt_syntax::HighlightedDocument>>> {
        let mut out = HashMap::new();
        for (ix, top) in tree.blocks.iter().enumerate() {
            if only.is_some_and(|o| o != ix) {
                continue;
            }
            if let Block::CodeBlock { language, code, .. } = &top.block
                && let Some(lang) = language
                    .as_deref()
                    .and_then(holt_syntax::language_for_alias)
            {
                out.insert(
                    ix,
                    self.highlights.request(row_id.clone(), ix, lang, code, cx),
                );
            }
        }
        out
    }

    /// Request diagram images for the mermaid code blocks of a tree (same
    /// shape as [`Self::code_highlight_for`]); `None` until the background
    /// render lands — those blocks display the code until then.
    fn mermaid_for(
        &mut self,
        row_id: &SharedString,
        tree: &Arc<BlockTree>,
        only: Option<usize>,
        cx: &mut Context<Self>,
    ) -> HashMap<usize, Option<Arc<gpui::RenderImage>>> {
        let mut out = HashMap::new();
        for (ix, top) in tree.blocks.iter().enumerate() {
            if only.is_some_and(|o| o != ix) {
                continue;
            }
            if let Block::CodeBlock {
                language,
                code,
                closed: true,
            } = &top.block
                && crate::markdown::mermaid::is_mermaid(language.as_deref())
                && crate::markdown::mermaid::supported_diagram(code)
            {
                out.insert(ix, self.mermaids.request(row_id.clone(), ix, code, cx));
            }
        }
        out
    }

    /// Wheel-zoom wiring for one row's diagrams ([`render::MermaidUi`]):
    /// zooms are baked per frame (render-time read), wheel steps route back
    /// to this entity's [`MermaidStore`](crate::markdown::mermaid::MermaidStore).
    fn mermaid_ui_for(
        &self,
        row_id: &SharedString,
        tree: &Arc<BlockTree>,
        cx: &mut Context<Self>,
    ) -> render::MermaidUi {
        let row_key = row_id.clone();
        let zooms: HashMap<usize, (f32, f32)> = (0..tree.blocks.len())
            .map(|ix| {
                let zoom = self.mermaids.zoom_for(&row_key, ix);
                let raster = self.mermaids.raster_zoom_for(&row_key, ix);
                (ix, (zoom, raster))
            })
            .collect();
        let rasters = zooms.clone();
        let entity = cx.weak_entity();
        let reset_entity = entity.clone();
        let step_row = row_key.clone();
        let handler = Rc::new(
            move |ix: usize, factor: f32, _window: &mut Window, cx: &mut gpui::App| {
                let row_key = step_row.clone();
                entity
                    .update(cx, |this, cx| {
                        this.mermaids.zoom_step(row_key, ix, factor, cx);
                    })
                    .ok();
            },
        );
        let reset_row = row_key.clone();
        let reset = Rc::new(move |ix: usize, _window: &mut Window, cx: &mut gpui::App| {
            let row_key = reset_row.clone();
            reset_entity
                .update(cx, |this, cx| {
                    this.mermaids.zoom_reset(row_key, ix, cx);
                })
                .ok();
        });
        render::MermaidUi {
            zoom: Rc::new(move |ix| zooms.get(&ix).map_or(1.0, |z| z.0)),
            raster_zoom: Rc::new(move |ix| rasters.get(&ix).map_or(1.0, |z| z.1)),
            handler,
            reset,
        }
    }
}

/// Whether an AUTO-derived open state flipped on a row the user hasn't
/// pinned — the streaming tail moving off a trailing group, a thought losing
/// the tail, or the settle. `last_auto` is `None` until first sight: a row
/// APPEARS at its height, and only a flip on an already-rendered row arms
/// the height tween. Pure.
fn auto_flip_armed(pinned: Option<bool>, last_auto: Option<bool>, auto_now: bool) -> bool {
    pinned.is_none() && last_auto.is_some_and(|last| last != auto_now)
}

/// A sent message's text with its file-mention chips. The same recipe as the
/// markdown renderer's inline code (`flat_text_element`): chip ranges shape in
/// the mono font at the spectrum's `code_text`, [`StyledText`] supplies wrapped glyph
/// geometry through its layout handle, and a canvas paints the rounded
/// `code_wash` *beneath* the glyphs — so chips wrap, clip, and scroll exactly
/// like the text they decorate.
///
/// Per-frame cost while an assistant message streams below: shaping hits
/// gpui's line-layout cache (identical text + runs ⇒ reuse) and the underlay
/// repaints O(chips) quads — no layout work, no re-projection (spans were
/// computed once in [`rows_for_entry`]).
/// A leading skill chip inside the bubble text: `range` covers the label
/// (always at offset 0), `open_url` is the click-through to its source file.
struct SkillChipRun {
    range: std::ops::Range<usize>,
    open_url: Option<String>,
}

type ImageOpen = Rc<dyn Fn(&str, &mut Window, &mut gpui::App)>;

/// A skill mention chip's click-through: emits the shell-facing file-open
/// event so the `SKILL.md` lands in the sidebar's file tab.
type SkillOpen = Rc<dyn Fn(&str, &mut Window, &mut gpui::App)>;

/// Where one clickable bubble span leads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BubbleLinkKind {
    /// An image file mention — the lightbox viewer.
    Image,
    /// A skill mention — the shell's `OpenSkillFile` event.
    SkillFile,
    /// Anything else — an external `file://` open (the legacy chip).
    Url,
}

/// The user bubble's click-through and copy behavior: the image lightbox,
/// the skill file-open event, and the raw-copy map for chip-projected text.
struct BubbleClicks {
    image_open: Option<ImageOpen>,
    skill_open: Option<SkillOpen>,
    copy_map: Option<crate::markdown::selection::CopyMap>,
}

fn user_bubble_text_with_chip(
    row_id: &SharedString,
    text: SharedString,
    mentions: Arc<Vec<crate::composer::SentMentionSpan>>,
    skill: Option<SkillChipRun>,
    theme: &Theme,
    clicks: BubbleClicks,
) -> AnyElement {
    // Split runs at chip boundaries (spans are in order): body text keeps the
    // sans font, mention chips read as inline code, skill chips read in the
    // accent like the composer's. Size/line-height flow from the bubble's
    // div like every text child.
    let body_run = |len: usize| TextRun {
        len,
        font: gpui::font(theme.font_sans.clone()),
        color: theme.text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let chip_run = |len: usize| TextRun {
        len,
        font: gpui::font(theme.font_mono.clone()),
        color: theme.code_text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let skill_run = |len: usize| TextRun {
        len,
        font: gpui::Font {
            weight: gpui::FontWeight::MEDIUM,
            ..gpui::font(theme.font_sans.clone())
        },
        color: theme.accent,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let mut runs = Vec::with_capacity(mentions.len() * 2 + 2);
    let mut at = 0;
    if let Some(chip) = &skill {
        runs.push(skill_run(chip.range.len()));
        at = chip.range.end;
    }
    for span in mentions.iter() {
        if at < span.range.start {
            runs.push(body_run(span.range.start - at));
        }
        if span.is_skill {
            runs.push(skill_run(span.range.len()));
        } else {
            runs.push(chip_run(span.range.len()));
        }
        at = span.range.end;
    }
    if at < text.len() {
        runs.push(body_run(text.len() - at));
    }
    let styled = StyledText::new(text.clone()).with_runs(runs);
    let layout = styled.layout().clone();
    let skill_range = skill.as_ref().map(|chip| chip.range.clone());
    let mut links: Vec<(std::ops::Range<usize>, String, BubbleLinkKind)> = Vec::new();
    if let Some(chip) = skill
        && let Some(url) = chip.open_url
    {
        links.push((chip.range, url, BubbleLinkKind::Url));
    }
    for span in mentions.iter() {
        if span.is_skill {
            links.push((
                span.range.clone(),
                span.path.to_string(),
                BubbleLinkKind::SkillFile,
            ));
        } else if !span.is_dir && crate::images::is_image_path(&span.path) {
            links.push((
                span.range.clone(),
                span.path.to_string(),
                BubbleLinkKind::Image,
            ));
        }
    }
    let text_el = if links.is_empty() {
        styled.into_any_element()
    } else {
        gpui::InteractiveText::new(SharedString::from(format!("{row_id}#links")), styled)
            .on_click(
                links.iter().map(|l| l.0.clone()).collect(),
                move |index, window, cx| {
                    let (_, path, kind) = &links[index];
                    match kind {
                        BubbleLinkKind::Image => {
                            if let Some(open) = &clicks.image_open {
                                open(path, window, cx);
                            }
                        }
                        BubbleLinkKind::SkillFile => {
                            if let Some(open) = &clicks.skill_open {
                                open(path, window, cx);
                            }
                        }
                        BubbleLinkKind::Url => cx.open_url(path),
                    }
                },
            )
            .into_any_element()
    };
    let wash = theme.code_wash;
    let skill_wash = theme.accent_wash;
    let sel_key: std::sync::Arc<str> = format!("{row_id}:u").into();
    let sel_theme = theme.clone();
    let underlay = canvas(
        |_, _, _| (),
        move |_, _, window, cx| {
            let paint = |window: &mut Window, range: &std::ops::Range<usize>, color| {
                for rect in render::range_rects(&layout, range, 0.0, 2.0) {
                    window.paint_quad(quad(
                        rect,
                        px(5.0),
                        color,
                        px(0.0),
                        gpui::transparent_black(),
                        BorderStyle::default(),
                    ));
                }
            };
            if let Some(range) = &skill_range {
                paint(window, range, skill_wash);
            }
            for span in mentions.iter() {
                let color = if span.is_skill { skill_wash } else { wash };
                paint(window, &span.range, color);
                // Skill chips lead with the skill identity glyph — the same
                // cube the `/` menu rows show, at text-matching size — inside
                // the label's gutter NBSPs (the display text itself is the
                // bare name — ADR-0035).
                if span.is_skill
                    && let Some(rect) = render::range_rects(&layout, &span.range, 0.0, 2.0)
                        .first()
                        .copied()
                {
                    let icon = px(14.0);
                    let _ = window.paint_svg(
                        Bounds::new(
                            point(
                                rect.origin.x + px(2.5),
                                rect.origin.y + (rect.size.height - icon) / 2.0,
                            ),
                            size(icon, icon),
                        ),
                        crate::icons::CUBE.into(),
                        None,
                        gpui::TransformationMatrix::unit(),
                        sel_theme.accent,
                        cx,
                    );
                }
            }
            render::paint_text_selection(
                window,
                &sel_key,
                &text,
                &layout,
                &sel_theme,
                clicks.copy_map.as_ref(),
            );
        },
    )
    .absolute()
    .size_full();
    div()
        .relative()
        // Selectable text reads as such on hover (mention/skill chips keep
        // their own cursor via their interactive elements).
        .cursor(CursorStyle::IBeam)
        .child(underlay)
        .child(text_el)
        .into_any_element()
}

/// The transcript ErrorChip — a port of holt chat-view.tsx `ErrorChip`
/// (34px-minimum row, `rounded-[10px] border border-red-400/[0.16]
/// bg-red-400/[0.05] px-2 text-[12px]`) with a 20px red-washed tile holding a
/// 12px DangerTriangle (`bg-red-400/[0.12] text-red-300/80`), a medium
/// "Error" label, then the human message at `text-foreground/80` — a subtle
/// red-tinted wash, never a bare red-stroke box. Unlike the web port, the
/// message WRAPS instead of truncating: startup-crash errors carry the
/// agent's exit status and stderr, and a one-line ellipsis was exactly what
/// made holtsh/holt#95 undiagnosable from the screenshot.
fn error_chip(message: SharedString, theme: &Theme) -> AnyElement {
    let red_300 = theme.danger_muted; // tailwind red-300
    let danger = theme.danger; // red-400
    div()
        .py(px(4.0))
        .w_full()
        .child(
            div()
                .min_h(px(34.0))
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .rounded(px(10.0))
                .border_1()
                .border_color(danger.opacity(0.16))
                .bg(danger.opacity(0.05))
                .px(px(8.0))
                .py(px(7.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .flex_none()
                        .size(px(20.0))
                        .rounded(px(6.0))
                        .bg(danger.opacity(0.12))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                                .size(px(12.0))
                                .text_color(red_300.opacity(0.8)),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(red_300.opacity(0.8))
                        .child(SharedString::from("Error")),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .text_color(theme.text.opacity(0.8))
                        .child(message),
                ),
        )
        .into_any_element()
}

/// The transcript retry chip (WatchTurnRetry): the streaming entry's
/// provider request hit a transient failure and is backing off. Same shape
/// as the ErrorChip but in the busy wash — it reports a recoverable pause
/// in the working stream, not a verdict, so it rides the accent family
/// like the streaming indicator it interrupts — with a small spinner
/// standing in for the retry clock. The chip only exists while the stream
/// is quiet: transcript frames resume it away.
fn retry_chip(
    attempt: u32,
    max_retries: u32,
    delay_secs: u64,
    error: SharedString,
    theme: &Theme,
) -> AnyElement {
    let busy = theme.busy;
    div()
        .py(px(4.0))
        .w_full()
        .child(
            div()
                .min_h(px(34.0))
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .rounded(px(10.0))
                .border_1()
                .border_color(busy.opacity(0.16))
                .bg(busy.opacity(0.05))
                .px(px(8.0))
                .py(px(7.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .flex_none()
                        .size(px(20.0))
                        .rounded(px(6.0))
                        .bg(busy.opacity(0.12))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            crate::icons::icon(crate::icons::REFRESH)
                                .size(px(12.0))
                                .text_color(busy.opacity(0.8)),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(busy.opacity(0.9))
                        .child(SharedString::from(format!(
                            "Retrying · attempt {attempt}/{max_retries}"
                        ))),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(theme.text.opacity(0.6))
                        .child(SharedString::from(format!("in {delay_secs}s"))),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .text_color(theme.text.opacity(0.8))
                        .child(error),
                ),
        )
        .into_any_element()
}

/// The transcript notice row (ADR-0010): a quiet full-width line of
/// housekeeping prose — the legacy-chat and damaged-History notices.
/// Deliberately quieter than a message (no bubble, 12px muted text, a
/// faint info glyph) and distinct from an error (neutral ink, no red,
/// no border): it states where the model's memory begins, it does not
/// report a failure. The text wraps — the damaged-file reason can be long.
fn notice_row(message: SharedString, theme: &Theme) -> AnyElement {
    div()
        .py(px(4.0))
        .w_full()
        .flex()
        .items_start()
        .gap(px(6.0))
        .text_size(px(12.0))
        .child(
            crate::icons::icon(crate::icons::INFO_CIRCLE)
                .size(px(12.0))
                .flex_none()
                .mt(px(3.0))
                .text_color(theme.text_muted.opacity(0.55)),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .line_height(px(17.0))
                .text_color(theme.text_muted.opacity(0.9))
                .child(message),
        )
        .into_any_element()
}

/// The goal loop's terminal row (ADR-0044): the goal card's settled
/// form landed in the transcript — the question chip's passive band with
/// the goal's target glyph, so the row reads as the arc's conclusion,
/// never as housekeeping noise beside it. One line, truncated.
fn goal_end_row(message: SharedString, theme: &Theme) -> AnyElement {
    div()
        .py(px(4.0))
        .w_full()
        .child(
            div()
                .h(px(28.0))
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .rounded(px(8.0))
                .border_1()
                .border_color(crate::theme::hairline(0.08))
                .bg(crate::theme::ink(0.045))
                .px(px(6.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .flex_none()
                        .size(px(18.0))
                        .rounded(px(5.0))
                        .bg(crate::theme::ink(0.09))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            crate::icons::icon(crate::icons::TARGET)
                                .size(px(11.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_color(theme.text_muted.opacity(0.9))
                        .child(message),
                ),
        )
        .into_any_element()
}

/// A passive one-line chip marking a question the agent asked — the
/// interactive controls live in the composer (chat-view.tsx `InputChip`):
/// 34px row, `rounded-[10px] border-white/[0.08] bg-white/[0.045] px-2
/// text-[12px]`, a 20px `bg-white/[0.09]` icon tile with a 12px
/// ChatRoundLine, the medium "Question" label, then the truncating value —
/// the first question's header once resolved, "Awaiting your answer…" while
/// pending. Neutral tones throughout; resolution never recolors the chip.
fn input_chip(header: SharedString, resolved: bool, theme: &Theme) -> AnyElement {
    let value: SharedString = if resolved {
        header
    } else {
        "Awaiting your answer…".into()
    };
    div()
        .py(px(4.0))
        .w_full()
        .child(
            div()
                .h(px(34.0))
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .rounded(px(10.0))
                .border_1()
                .border_color(crate::theme::hairline(0.08))
                .bg(crate::theme::ink(0.045))
                .px(px(8.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .flex_none()
                        .size(px(20.0))
                        .rounded(px(6.0))
                        .bg(crate::theme::ink(0.09))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            crate::icons::icon(crate::icons::CHAT_ROUND_LINE)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Question")),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_color(theme.text.opacity(0.9))
                        .child(value),
                ),
        )
        .into_any_element()
}

/// A collapsed skill invocation / skill-file read (ADR-0006): the skill's
/// name in the accent colour with a cube glyph — clicking opens the source
/// `SKILL.md` in the workspace file tab (right pane) so the user can
/// inspect exactly what the agent was told to follow. Right-aligned like
/// the user bubble it replaces. `on_open` carries the click (a listener
/// emitting [`TranscriptEvent::OpenSkillFile`], the shell's file-open
/// path); `None` leaves the chip inert.
fn skill_chip(
    name: SharedString,
    file: SharedString,
    pending: bool,
    on_open: Option<impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static>,
    theme: &Theme,
) -> AnyElement {
    let clickable_id = (!file.is_empty()).then(|| name.clone());
    let chip = div().py(px(4.0)).w_full().flex().justify_end().child(
        div()
            .min_h(px(34.0))
            .flex()
            .items_center()
            .gap(px(7.0))
            .overflow_hidden()
            .rounded(px(10.0))
            .bg(crate::theme::ink(0.06))
            .px(px(12.0))
            .text_size(px(13.0))
            .when(pending, |el| el.opacity(0.65))
            .child(
                crate::icons::icon(crate::icons::CUBE)
                    .size(px(15.0))
                    .text_color(theme.accent),
            )
            .child(
                div()
                    .flex_none()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.accent)
                    .child(name),
            ),
    );
    match (clickable_id, on_open) {
        (Some(id), Some(on_open)) => chip
            .id(id)
            .cursor_pointer()
            .hover(|el| el.opacity(0.8))
            .on_click(on_open)
            .into_any_element(),
        _ => chip.into_any_element(),
    }
}

/// A small glyph standing in for the tool's icon (holt uses an icon set; a
/// quiet monochrome character keeps the tile without shipping SVGs).
/// The glyph for a tool call (holt tool-chip.tsx `toolIcon`, Solar set).
fn tool_icon_path(call: &ToolCall) -> &'static str {
    match call {
        ToolCall::Exec { .. } => crate::icons::COMMAND,
        ToolCall::ReadFile { .. } | ToolCall::ApplyPatch { .. } => crate::icons::DOCUMENT,
        ToolCall::ReadChat { .. } => crate::icons::CHAT_ROUND_LINE,
        ToolCall::WriteFile { .. } => crate::icons::DOCUMENT_ADD,
        ToolCall::EditFile { .. } => crate::icons::PEN,
        ToolCall::Search { .. } => crate::icons::MAGNIFER,
        ToolCall::ListDir { .. } | ToolCall::Glob { .. } => crate::icons::FOLDER_WITH_FILES,
        ToolCall::WebFetch { .. } | ToolCall::WebSearch { .. } => crate::icons::GLOBAL,
        ToolCall::Todo { .. } => crate::icons::CHECKLIST,
        call if is_agent_call(call) => crate::icons::BOT,
        ToolCall::Mcp { .. } | ToolCall::Unknown { .. } => crate::icons::WIDGET,
    }
}

/// The inline trailing affordance on a chip header, when it has one.
enum ChipTrail {
    /// Expand/collapse chevron — flipped while the detail body is open.
    Chevron { open: bool },
    /// Top-right "opens elsewhere" arrow — the spawn chip's link to its
    /// subagent tab.
    OpenArrow,
}

/// The chip's content row: bare icon + label + detail line (+ inline trailing
/// affordance when the chip expands or links out). Shared between the plain
/// chip, the header of an expandable chip, and the spawn link chip. Flat by
/// design — no card chrome; the row is the chip.
///
/// Spawn chips carry their subagent's lifecycle VISUALLY, in the chip's own
/// language: while running the mini working spinner (the sidebar's) pulses
/// at the right of the ordinary static detail; done is the ordinary quiet
/// chip; failed takes the danger tint — no status words, no live text (a
/// header rewriting itself per stream delta read as noise — user report).
fn chip_header_row(
    tool: &ToolItem,
    trail: Option<ChipTrail>,
    theme: &Theme,
    view: gpui::EntityId,
    cwd: Option<&str>,
    cx: &mut gpui::App,
) -> gpui::Div {
    let (label, detail) = if tool.is_thought {
        ("Thought process", String::new())
    } else {
        tool_chip_content_in(&tool.call, cwd)
    };
    let subagent_type = match &tool.call {
        ToolCall::Unknown { input, .. } | ToolCall::Mcp { input, .. }
            if !tool.is_thought && is_agent_call(&tool.call) =>
        {
            input
                .as_ref()
                .and_then(|input| input.get("subagent_type")?.as_str())
                .and_then(|name| {
                    let mut chars = name.trim().chars();
                    let first = chars.next()?;
                    Some(format!("{}{}", first.to_uppercase(), chars.as_str()))
                })
        }
        _ => None,
    };
    let running = tool.subagent_ref.is_some()
        && matches!(tool.subagent_status, Some(SubagentStatus::Running));
    // An ordinary tool mid-call (part not yet resolved): same trailing
    // spinner the subagent chip gets, so "what is it doing" is visible
    // without opening anything.
    let pending = !tool.is_thought && !tool.resolved && tool.subagent_ref.is_none();
    let failed = tool.is_error
        || (tool.subagent_ref.is_some()
            && matches!(tool.subagent_status, Some(SubagentStatus::Failed)));
    let tint = if failed {
        theme.danger
    } else {
        theme.text_muted
    };
    div()
        .h(px(CHIP_HEIGHT))
        .w_full()
        .min_w_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(8.0))
        .text_size(px(12.0))
        .line_height(px(18.0))
        .child(
            // Bare leading icon — the slot keeps the old tile's 18px
            // footprint so the guide rail hanging under it doesn't move.
            div()
                .size(px(18.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    crate::icons::icon(if tool.is_thought {
                        crate::icons::CHAT_ROUND_LINE
                    } else {
                        tool_icon_path(&tool.call)
                    })
                    .size(px(13.0))
                    .text_color(theme.text_muted.opacity(0.85)),
                ),
        )
        .child(
            div()
                .flex_none()
                .h(px(18.0))
                .flex()
                .items_center()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(tint)
                .gap(px(4.0))
                .child(SharedString::from(label))
                .when_some(subagent_type, |row, name| {
                    row.child(
                        div()
                            .text_color(theme.accent)
                            .child(SharedString::from(name)),
                    )
                }),
        )
        // The detail hugs its text (grow 0, shrink 1) so the trailing
        // affordance sits right after it; a long line still truncates.
        .when(!detail.is_empty(), |row| {
            row.child(
                div()
                    .min_w_0()
                    .flex_shrink(1.0)
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .truncate()
                    .text_color(if failed {
                        theme.danger
                    } else {
                        theme.text.opacity(0.85)
                    })
                    .child(SharedString::from(detail)),
            )
        })
        .when_some(tool.call.subagent_model(), |row, model| {
            // Which model the child runs on, when the spawn named one.
            //
            // In the trailing slot rather than suffixed onto the detail: the
            // detail is the truncating slot, and the model is exactly what a
            // reader scanning a fan-out of spawns wants left once the
            // descriptions are cut.
            //
            // Bare faint text, NOT a filled pill: the affordances either side
            // of it (spinner = running, arrow = opens the subagent) already
            // claim the trailing edge; giving a passive label the same chrome
            // made the row read as three buttons — the loudest thing in the
            // row was the one thing you cannot click.
            row.child(
                div()
                    .flex_none()
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .text_size(px(11.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(model.to_owned())),
            )
        })
        .when(
            tool.subagent_usage.is_some()
                && matches!(
                    tool.subagent_status,
                    Some(SubagentStatus::Done) | Some(SubagentStatus::Failed)
                ),
            |row| {
                row.child(
                    div()
                        .flex_none()
                        .h(px(18.0))
                        .flex()
                        .items_center()
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!(
                            "{} tokens",
                            crate::token_display::compact_tokens(
                                tool.subagent_usage.unwrap_or_default()
                            )
                        ))),
                )
            },
        )
        .when_some(
            // The settled verdict (ADR-0014): a small tinted marker after the
            // detail — "✓ Approved", "⊘ Denied · "note"", "⚡ Prefix exempt", …
            tool.gate.as_ref().and_then(|gate| match &gate.state {
                ToolGateState::Settled { verdict } => Some(super::verdict_chip(verdict)),
                ToolGateState::Pending { .. } => None,
            }),
            |row, (text, tint)| {
                row.child(
                    div()
                        .flex_none()
                        .h(px(18.0))
                        .flex()
                        .items_center()
                        .text_size(px(11.0))
                        .text_color(super::verdict_tint_color(tint, theme))
                        .child(SharedString::from(text)),
                )
            },
        )
        .when(running, |row| {
            // The sidebar working-row spinner, in the chip's trailing slot —
            // paint-local (fixed footprint), so it never moves the layout.
            row.child(div().flex_none().child(crate::loaders::mini_glyph_spinner(
                format!(
                    "subagent-chip-{}",
                    tool.subagent_ref.as_deref().unwrap_or_default()
                ),
                2.0,
                theme.glyph,
                view,
                cx,
            )))
        })
        .when(pending, |row| {
            row.child(div().flex_none().child(crate::loaders::mini_glyph_spinner(
                SharedString::from(format!(
                    "tool-chip-{}",
                    fnv1a(format!("{:?}", tool.call).as_bytes())
                )),
                2.0,
                theme.glyph,
                view,
                cx,
            )))
        })
        .when_some(trail, |row, trail| {
            // Inline trailing affordance (no tile): a chevron for the
            // output/diff accordion, or the open-arrow for spawn chips.
            row.child(match trail {
                ChipTrail::Chevron { open } => div()
                    .flex_none()
                    .text_size(px(10.0))
                    .text_color(theme.text_muted.opacity(0.8))
                    .child(SharedString::from(if open { "▾" } else { "▸" })),
                ChipTrail::OpenArrow => div().flex_none().child(
                    crate::icons::icon(crate::icons::ARROW_UP_RIGHT)
                        .size(px(11.0))
                        .text_color(theme.text_muted.opacity(0.8)),
                ),
            })
        })
}

/// The header row of an expandable chip card.
fn chip_header(
    tool: &ToolItem,
    open: bool,
    theme: &Theme,
    view: gpui::EntityId,
    cwd: Option<&str>,
    cx: &mut gpui::App,
) -> gpui::Div {
    chip_header_row(
        tool,
        Some(ChipTrail::Chevron { open }),
        theme,
        view,
        cwd,
        cx,
    )
}

impl Render for Transcript {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let typography_generation = crate::typography::generation(cx);
        if self.typography_generation != typography_generation {
            self.typography_generation = typography_generation;
            // `refresh_windows` re-lays out visible rows, but ListState keeps
            // measured heights for virtualized rows outside the viewport.
            // Mark every row unmeasured while retaining height hints and a
            // proportional scroll anchor; GPUI will refresh each measurement
            // as the row enters its layout range.
            self.list.remeasure();
        }
        // Release gpui-side decoded copies of any images the attachment LRU
        // evicted since the last frame (no-op when nothing was evicted).
        crate::images::flush_evicted(Some(window), cx);
        // Own-turn driver: measurements are only authoritative after layout,
        // so reservation sizing, the send glide, and the outgrown-handoff
        // each advance at most once per requested frame. Scheduled on every
        // frame while an anchor is live (not just on kicks) so viewport
        // resizes and streaming growth re-derive the reservation; the step
        // only notifies on change, so a settled hold schedules no next frame.
        if (self.own_turn.is_some() || self.own_turn_kick) && !self.own_turn_scheduled {
            self.own_turn_scheduled = true;
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |this: &mut Transcript, cx| {
                        this.own_turn_scheduled = false;
                        this.step_own_turn(cx);
                    })
                    .ok();
            });
        }
        // Spring driver: one on_next_frame callback at a time; each tick
        // notifies, which re-enters render and schedules the next frame until
        // the spring parks. Reduced motion never schedules (sync snaps).
        if self.pinned
            && !motion::reduced_motion(cx)
            && !self.spring_scheduled
            && self.spring_should_run()
        {
            self.spring_scheduled = true;
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |this: &mut Transcript, cx| {
                        this.spring_scheduled = false;
                        this.step_spring(cx);
                    })
                    .ok();
            });
        }
        // Programmatic `scroll_to` does not invoke the list's user-scroll
        // handler. Refresh distance-derived state once layout has measured the
        // replay, guarded so a stale A callback cannot mutate B (or a newer A).
        if self.viewport_finalize_pending && !self.viewport_finalize_scheduled {
            self.viewport_finalize_scheduled = true;
            let token = ViewportFinalizeToken {
                generation: self.viewport_generation,
                layout_revision: self.viewport_layout_revision,
            };
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |this: &mut Transcript, cx| {
                        this.viewport_finalize_scheduled = false;
                        if !token.still_current(this.viewport_generation) {
                            if this.viewport_finalize_pending {
                                cx.notify();
                            }
                            return;
                        }
                        let distance = this.distance_from_bottom();
                        this.last_scroll_distance = distance;
                        this.show_jump_button = distance > SCROLL_BUTTON_THRESHOLD_PX
                            && !this.pinned
                            && !this.own_turn.as_ref().is_some_and(|turn| turn.held);
                        if token.layout_settled(this.viewport_layout_revision) {
                            this.viewport_finalize_pending = false;
                        }
                        cx.notify();
                    })
                    .ok();
            });
        }
        let rail = self.render_rail(cx);
        // The scroll-to-bottom pill is rendered by the SHELL (conversation
        // region overlay): it must float just above the composer and paint
        // OVER the bottom fade gradient, which is a later sibling of this
        // outlet — an overlay here would be tinted by the fade.
        let list_el = list(self.list.clone(), cx.processor(Self::render_row))
            .size_full()
            .with_sizing_behavior(gpui::ListSizingBehavior::Auto);
        let content: AnyElement = if self.doc_override.is_some() {
            // The primary transcript's fade lives on the SHELL's outlet
            // wrapper (it spans the titlebar/composer chrome); an override
            // instance owns its own — top edge only (nothing overlays the
            // pane's bottom), gated on real overflow so a short top-anchored
            // transcript shows no fade. Gated here rather than at paint via
            // a ScrollHandle (the list isn't one); scrolls re-render this
            // entity, so the flag can't go stale.
            let scrolled_under_top = {
                let max = f32::from(self.list.max_offset_for_scrollbar().y);
                max - self.distance_from_bottom() > 1.0
            };
            crate::edge_fade::edge_faded(
                Theme::TRANSCRIPT_FADE_BAND,
                scrolled_under_top,
                false,
                list_el,
            )
            .into_any_element()
        } else {
            list_el.into_any_element()
        };
        let list_width = self.list_width.clone();
        let root = div()
            .relative()
            .size_full()
            .min_h_0()
            .on_mouse_move(cx.listener(Self::on_selection_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_selection_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_selection_mouse_up))
            // FIRST child ⇒ paints first: clears the frame's markdown text-
            // selection registry before any row's text elements re-register
            // (document paint order = selection order; see markdown/render.rs).
            .child(crate::markdown::render::selection_frame_reset())
            // Layout probe: the root's laid-out width == the list viewport
            // width (the rail is an absolute overlay). Read by the inline
            // message edit's first-frame measurement seed.
            .child(
                gpui::canvas(
                    move |bounds, _, _| list_width.set(f32::from(bounds.size.width)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(content)
            .child(rail);
        if let Some(preview) = self.attachment_preview.clone() {
            return root.child(preview);
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holt_proto::view::tool_chip_content;

    #[gpui::test]
    fn nested_compaction_scroll_does_not_move_transcript_list(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let outer = gpui::ListState::new(4, gpui::ListAlignment::Top, gpui::px(200.0));
        let inner = gpui::ScrollHandle::new();

        struct TestView {
            outer: gpui::ListState,
            inner: gpui::ScrollHandle,
        }

        impl gpui::Render for TestView {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let inner = self.inner.clone();
                gpui::list(self.outer.clone(), move |ix, _, _| {
                    if ix == 0 {
                        gpui::div()
                            .h(gpui::px(100.0))
                            .child(
                                gpui::div()
                                    .id("compaction-body")
                                    .h(gpui::px(50.0))
                                    .flex()
                                    .flex_col()
                                    .overflow_y_scroll()
                                    .track_scroll(&inner)
                                    .occlude()
                                    .children((0..10).map(|_| gpui::div().h(gpui::px(20.0)))),
                            )
                            .into_any()
                    } else {
                        gpui::div().h(gpui::px(100.0)).into_any()
                    }
                })
                .w_full()
                .h_full()
            }
        }

        let view = cx.update(|_, cx| {
            cx.new(|_| TestView {
                outer: outer.clone(),
                inner: inner.clone(),
            })
        });
        cx.draw(
            gpui::point(gpui::px(0.0), gpui::px(0.0)),
            gpui::size(gpui::px(100.0), gpui::px(100.0)),
            |_, _| view.clone().into_any_element(),
        );

        cx.simulate_event(gpui::ScrollWheelEvent {
            position: gpui::point(gpui::px(50.0), gpui::px(25.0)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(-40.0))),
            ..Default::default()
        });

        assert_eq!(outer.logical_scroll_top().item_ix, 0);
        assert_eq!(outer.logical_scroll_top().offset_in_item, gpui::px(0.0));
        assert_eq!(inner.offset().y, gpui::px(-40.0));
    }

    /// ADR-0013 chaining: at the nested viewport's scroll boundary the
    /// wheel's unabsorbed remainder moves the OUTER list, instead of
    /// dead-ending. The inner body is 50px tall with 200px of content (max
    /// offset 150); the outer list has four 100px items in a 100px window.
    /// The body must be a BLOCK div capped by max_h (the skill/compaction
    /// shape): a fixed-height flex column lets taffy shrink the children
    /// into the container and the scroll extent degenerates to zero.
    #[gpui::test]
    fn nested_scroll_chains_to_transcript_list_at_bounds(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let outer = gpui::ListState::new(4, gpui::ListAlignment::Top, gpui::px(200.0));
        let inner = gpui::ScrollHandle::new();

        struct TestView {
            outer: gpui::ListState,
            inner: gpui::ScrollHandle,
        }

        impl gpui::Render for TestView {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let inner = self.inner.clone();
                let outer = self.outer.clone();
                gpui::list(self.outer.clone(), move |ix, _, _| {
                    if ix == 0 {
                        let wheel_inner = inner.clone();
                        let wheel_outer = outer.clone();
                        gpui::div()
                            .h(gpui::px(100.0))
                            .child(
                                gpui::div()
                                    .id("chaining-body")
                                    .max_h(gpui::px(50.0))
                                    .overflow_y_scroll()
                                    .track_scroll(&inner)
                                    .occlude()
                                    .on_scroll_wheel(move |_, _, _| {
                                        forward_scroll_remainder(&wheel_outer, &wheel_inner);
                                    })
                                    .children((0..10).map(|_| gpui::div().h(gpui::px(20.0)))),
                            )
                            .into_any()
                    } else {
                        gpui::div().h(gpui::px(100.0)).into_any()
                    }
                })
                .w_full()
                .h_full()
            }
        }

        let view = cx.update(|_, cx| {
            cx.new(|_| TestView {
                outer: outer.clone(),
                inner: inner.clone(),
            })
        });
        cx.draw(
            gpui::point(gpui::px(0.0), gpui::px(0.0)),
            gpui::size(gpui::px(100.0), gpui::px(100.0)),
            |_, _| view.clone().into_any_element(),
        );

        let wheel = |delta_y: f32| gpui::ScrollWheelEvent {
            position: gpui::point(gpui::px(50.0), gpui::px(25.0)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(delta_y))),
            ..Default::default()
        };

        // Mid-body: the inner absorbs the whole delta, the outer stays put.
        cx.simulate_event(wheel(-40.0));
        assert_eq!(inner.offset().y, gpui::px(-40.0));
        assert_eq!(outer.logical_scroll_top().item_ix, 0);
        assert_eq!(outer.logical_scroll_top().offset_in_item, gpui::px(0.0));

        // Past the bottom: the inner keeps 110 of the 200 (clamped at -150),
        // the remaining 90 move the outer list.
        cx.simulate_event(wheel(-200.0));
        assert_eq!(inner.offset().y, gpui::px(-150.0));
        assert_eq!(outer.logical_scroll_top().item_ix, 0);
        assert_eq!(outer.logical_scroll_top().offset_in_item, gpui::px(90.0));

        // A second wheel event in the SAME frame: the clamped write-back
        // keeps the first overshoot from being forwarded twice — exactly
        // 200 more reach the outer list, no more.
        cx.simulate_event(wheel(-200.0));
        assert_eq!(inner.offset().y, gpui::px(-150.0));
        assert_eq!(outer.logical_scroll_top().item_ix, 2);
        assert_eq!(outer.logical_scroll_top().offset_in_item, gpui::px(90.0));

        // Back up past the top: the inner absorbs 150, 250 chain upward.
        cx.simulate_event(wheel(400.0));
        assert_eq!(inner.offset().y, gpui::px(0.0));
        assert_eq!(outer.logical_scroll_top().item_ix, 0);
        assert_eq!(outer.logical_scroll_top().offset_in_item, gpui::px(40.0));

        // Inner already at the top: the whole delta chains; the outer
        // clamps at its own top.
        cx.simulate_event(wheel(100.0));
        assert_eq!(inner.offset().y, gpui::px(0.0));
        assert_eq!(outer.logical_scroll_top().item_ix, 0);
        assert_eq!(outer.logical_scroll_top().offset_in_item, gpui::px(0.0));
    }

    #[test]
    fn auto_flip_arms_only_on_an_unpinned_edge() {
        // First sight seeds silently — a new row appears at its height.
        assert!(!auto_flip_armed(None, None, true));
        assert!(!auto_flip_armed(None, None, false));
        // Steady state (streaming growth included) never re-arms.
        assert!(!auto_flip_armed(None, Some(true), true));
        assert!(!auto_flip_armed(None, Some(false), false));
        // The flip on an existing, unpinned row arms: the tail moving off
        // the group, a thought losing the tail, the settle…
        assert!(auto_flip_armed(None, Some(true), false));
        // …and, symmetrically, a re-open.
        assert!(auto_flip_armed(None, Some(false), true));
        // A user pin masks the auto rule entirely, either way.
        assert!(!auto_flip_armed(Some(true), Some(true), false));
        assert!(!auto_flip_armed(Some(false), Some(false), true));
    }

    #[test]
    fn multiline_command_flattens_to_one_chip_line() {
        // The user's breaker: a multi-line script in a Run chip. The detail
        // must come out as ONE sanitized line — the chip's fixed-height row
        // then truncates it with an ellipsis like the original's CSS.
        let (label, detail) = tool_chip_content(&ToolCall::Exec {
            command: "set -e\nfixture_in_original=0\n\tgrep -c  \"x\"".into(),
        });
        assert_eq!(label, "Run");
        assert_eq!(detail, "set -e fixture_in_original=0 grep -c \"x\"");
        assert!(!detail.contains('\n'));
        // The chip row height is a constant, independent of content shape.
        assert_eq!(chips_height(1), CHIPS_TOP_PAD + CHIP_HEIGHT);
        // Every detail kind is sanitized (MCP inputs / queries are model text).
        let (_, q) = tool_chip_content(&ToolCall::WebSearch {
            query: "line one\nline two".into(),
        });
        assert_eq!(q, "line one line two");
    }
}
