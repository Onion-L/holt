//! GPUI rendering for the transcript: `render_row` and its chip/detail
//! builders, the user bubble and attachment strips, the working trailer,
//! `impl Render for Transcript`, and the paint-side helpers (highlight
//! store, frame-stats knobs). Entity state, sync, and events stay in the
//! facade; this module only reads them.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, BorderStyle, ClipboardItem, Context, MouseButton, ObjectFit, SharedString,
    StyledImage as _, StyledText, Task, TextRun, Window, canvas, div, img, list, prelude::*, px,
    quad,
};
use holt_doc::{MessageRole, MessageStatus, SubagentStatus, ToolGateState};
use holt_proto::ToolCall;
use holt_proto::view::tool_chip_content;

use super::model::{
    RowKind, ToolItem, UserSkill, fnv1a, format_skill_title, format_timestamp, is_agent_call,
    is_spawn_link, skill_file_display, tool_group_collapses, top_gap_for,
};
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

// ---------------------------------------------------------------------------
// Highlight store (background, time-sliced, paint-only)
// ---------------------------------------------------------------------------

pub(super) struct HighlightEntry {
    pub(super) key: DocumentHighlightKey,
    pub(super) document: Option<Weak<holt_syntax::HighlightedDocument>>,
    pub(super) _task: Option<Task<()>>,
}

/// Cache of tokenized code blocks keyed by `(row id, block ix)`. Tokenization
/// runs on the background executor, time-sliced; results apply as paint-only
/// run colors when they land.
#[derive(Default)]
pub(super) struct HighlightStore {
    pub(super) entries: HashMap<(SharedString, usize), HighlightEntry>,
    pub(super) cache: SyntaxHighlightCache,
}

impl HighlightStore {
    /// Current tokens if ready; kicks a background tokenize when stale/missing.
    fn request(
        &mut self,
        row_id: SharedString,
        block_ix: usize,
        lang: Lang,
        code: &str,
        cx: &mut Context<Transcript>,
    ) -> Option<Arc<holt_syntax::HighlightedDocument>> {
        let slot_key = (row_id.clone(), block_ix);
        let document_key = DocumentHighlightKey::new(lang, code);
        if let Some(entry) = self.entries.get(&slot_key)
            && entry.key == document_key
        {
            let document = entry.document.as_ref()?;
            if let Some(document) = document.upgrade() {
                return Some(document);
            }
        }
        if let Some(document) = self.cache.get(&document_key) {
            self.entries.insert(
                slot_key,
                HighlightEntry {
                    key: document_key,
                    document: Some(Arc::downgrade(&document)),
                    _task: None,
                },
            );
            return Some(document);
        }
        let code = code.to_string();
        let source_bytes = code.len();
        let task = cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let document = cx
                .background_executor()
                .spawn(async move {
                    holt_syntax::highlight(holt_syntax::HighlightRequest {
                        source: &code,
                        path: None,
                        fence_tag: Some(match lang {
                            Lang::Rust => "rust",
                            Lang::JavaScript => "javascript",
                            Lang::Jsx => "jsx",
                            Lang::TypeScript => "typescript",
                            Lang::Tsx => "tsx",
                            Lang::Python => "python",
                            Lang::Go => "go",
                            Lang::Json => "json",
                            Lang::Jsonc => "jsonc",
                            Lang::Bash => "bash",
                            Lang::Toml => "toml",
                            Lang::Markdown => "markdown",
                            Lang::Html => "html",
                            Lang::Css => "css",
                            Lang::Yaml => "yaml",
                            Lang::C => "c",
                            Lang::Cpp => "cpp",
                            Lang::CSharp => "csharp",
                            Lang::Java => "java",
                            Lang::Kotlin => "kotlin",
                            Lang::Swift => "swift",
                            Lang::Ruby => "ruby",
                            Lang::Php => "php",
                            Lang::Sql => "sql",
                            Lang::Lua => "lua",
                            Lang::Dockerfile => "dockerfile",
                            Lang::Nix => "nix",
                            Lang::Make => "make",
                        }),
                    })
                    .ok()
                })
                .await;
            this.update(cx, |transcript, cx| {
                if let Some(document) = document {
                    let document = Arc::new(document);
                    let retained = transcript
                        .highlights
                        .cache
                        .insert(document_key, document.clone());
                    if let Some(entry) = transcript.highlights.entries.get_mut(&slot_key)
                        && entry.key == document_key
                    {
                        tracing::debug!(
                            language = ?lang,
                            source_bytes,
                            spans = document.lines.iter().map(Vec::len).sum::<usize>(),
                            elapsed_us = started.elapsed().as_micros() as u64,
                            "syntax highlight ready"
                        );
                        entry.document = retained.then(|| Arc::downgrade(&document));
                        cx.notify();
                    }
                }
            })
            .ok();
        });
        self.entries.insert(
            (row_id, block_ix),
            HighlightEntry {
                key: document_key,
                document: None,
                _task: Some(task),
            },
        );
        None
    }
}

impl Transcript {
    /// The invocation chip that OPENS the agent's reply (seeded by the
    /// engine ahead of any thinking): a flush-left process row in the
    /// tool-chip language — quiet header, thinking-style fold. Expanding
    /// reveals the exact `<skill>` block the model received, with the
    /// source file one click away. Collapsing tweens the measured height
    /// to zero like the tool-group folds.
    #[allow(clippy::too_many_arguments)]
    fn render_skill_invocation(
        &mut self,
        row_id: &SharedString,
        name: &SharedString,
        file: &SharedString,
        content: &SharedString,
        pending: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fold = self.folds.get(row_id).copied().unwrap_or_default();
        let open = fold.open.unwrap_or(false);
        // A fresh COLLAPSE keeps the body mounted for one tween: the wrapper
        // shrinks the measured height to zero over RESIZE (the tool-group
        // fold pattern), then the settled closed state unmounts it.
        let closing = !open
            && fold.epoch > 0
            && fold
                .toggled_at
                .is_some_and(|at| at.elapsed() < FOLD_TWEEN_WINDOW);
        let formatted_title = format_skill_title(name);
        let toggle_row_id = row_id.clone();
        let header =
            div()
                .id(SharedString::from(format!("{row_id}#skill-toggle")))
                .h(px(CHIP_HEIGHT))
                .w_full()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .cursor_pointer()
                .when(pending, |el| el.opacity(0.65))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_skill_fold(toggle_row_id.clone(), cx)
                }))
                .child(
                    // The chip rows' content language (chip_header_row): bare
                    // 13px muted icon, medium muted label, hugging truncating
                    // detail, inline disclosure triangle right after it.
                    div()
                        .min_w_0()
                        .flex_1()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .text_size(px(12.0))
                        .line_height(px(18.0))
                        .child(
                            crate::icons::icon(crate::icons::CUBE)
                                .size(px(13.0))
                                .flex_none()
                                .text_color(theme.text_muted.opacity(0.85)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text_muted)
                                .child(SharedString::from(formatted_title)),
                        )
                        .when(!file.is_empty(), |row| {
                            row.child(
                                div()
                                    .min_w_0()
                                    .flex_shrink(1.0)
                                    .overflow_hidden()
                                    .truncate()
                                    .text_color(theme.text_muted.opacity(0.65))
                                    .child(SharedString::from(skill_file_display(file))),
                            )
                        })
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(10.0))
                                .text_color(theme.text_muted.opacity(0.8))
                                .child(SharedString::from(if open { "▾" } else { "▸" })),
                        ),
                );

        let mut column = div().w_full().flex().flex_col().child(header);
        if open || closing {
            let url = format!("file://{}", file.trim_start_matches("file://"));
            let body = div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .w_full()
                        .mb(px(6.0))
                        .px(px(10.0))
                        .py(px(8.0))
                        .rounded(px(8.0))
                        .bg(crate::theme::ink(0.03))
                        .id(SharedString::from(format!("{row_id}#skill-body")))
                        .max_h(px(320.0))
                        .overflow_y_scroll()
                        .font_family(theme.font_mono.clone())
                        .text_size(crate::typography::ui_rems(11.0))
                        .line_height(crate::typography::ui_rems(16.0))
                        .text_color(theme.text_muted.opacity(0.9))
                        .child(content.clone()),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("{row_id}#skill-file")))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .cursor_pointer()
                        .hover(|el| el.opacity(0.75))
                        .on_click(move |_, _, cx| {
                            cx.open_url(&url);
                        })
                        .child(
                            crate::icons::icon(crate::icons::ARROW_UP_RIGHT)
                                .size(px(11.0))
                                .text_color(theme.accent.opacity(0.8)),
                        )
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(10.5))
                                .text_color(theme.accent.opacity(0.8))
                                .child(SharedString::from("Open SKILL.md")),
                        ),
                );
            if open {
                column = column.child(body);
            } else {
                let from = (fold.from - CHIP_HEIGHT).max(0.0);
                column = column.child(div().overflow_hidden().child(body).with_animation(
                    SharedString::from(format!("{row_id}-fold{}", fold.epoch)),
                    RESIZE.animation(),
                    move |el, t| el.h(px(motion::lerp(from, 0.0, t))),
                ));
            }
        }
        column.into_any_element()
    }

    fn compaction_label(
        &self,
        compacting: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let opacity = if compacting {
            let phase = motion::pulse_delta(&motion::HOLT_PULSE, cx.entity_id(), cx);
            motion::lerp(0.55, 1.0, motion::pulse_wave(phase))
        } else {
            1.0
        };
        div()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2()
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.text_muted)
            .child(
                crate::icons::icon(crate::icons::CONTEXT_COMPACT)
                    .size_4()
                    .flex_none()
                    .opacity(opacity),
            )
            .when(compacting, |el| {
                el.child(crate::loaders::mini_mono_spinner(
                    "compaction-loading",
                    2.0,
                    theme.text_muted,
                    cx.entity_id(),
                    cx,
                ))
            })
            .child(if compacting {
                "Compacting context"
            } else {
                "Context compacted"
            })
    }

    /// A quiet completion row that expands to the summary the model carries.
    fn render_compaction_divider(
        &mut self,
        row_id: &SharedString,
        summary: &SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fold = self.folds.get(row_id).copied().unwrap_or_default();
        let open = fold.open.unwrap_or(false);
        let toggle_row_id = row_id.clone();
        let header =
            div()
                .id(SharedString::from(format!("{row_id}#divider-toggle")))
                .h(px(CHIP_HEIGHT))
                .w_full()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_skill_fold(toggle_row_id.clone(), cx)
                }))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .text_size(px(12.0))
                        .line_height(px(18.0))
                        .child(self.compaction_label(false, theme, cx))
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(10.0))
                                .text_color(theme.text_muted.opacity(0.8))
                                .child(SharedString::from(if open { "▾" } else { "▸" })),
                        ),
                );
        let mut column = div().w_full().flex().flex_col().child(header);
        if open {
            column = column.child(
                div()
                    .w_full()
                    .mt(px(2.0))
                    .mb(px(6.0))
                    .px(px(10.0))
                    .py(px(8.0))
                    .rounded(px(8.0))
                    .bg(crate::theme::ink(0.03))
                    .id(SharedString::from(format!("{row_id}#divider-body")))
                    .max_h(px(320.0))
                    .overflow_y_scroll()
                    // This is a nested reading viewport. Occlude the outer
                    // transcript hitbox so one wheel gesture cannot scroll
                    // both the summary and the transcript list.
                    .occlude()
                    .text_size(px(12.0))
                    .line_height(px(17.0))
                    .text_color(theme.text.opacity(0.85))
                    .child(summary.clone()),
            );
        }
        column.into_any_element()
    }

    /// A `/skill` invocation inside the user bubble: the skill title as an
    /// accent chip at the head of the text flow (the composer's treatment),
    /// with a click through to the source file. The `<skill>` block itself
    /// rides the AGENT entry's opening chip — this bubble only records what
    /// the user did.
    fn render_user_skill(
        &mut self,
        row_id: &SharedString,
        skill: &Arc<UserSkill>,
        text: &SharedString,
        mentions: &Arc<Vec<crate::composer::SentMentionSpan>>,
        theme: &Theme,
        image_open: ImageOpen,
    ) -> AnyElement {
        let open_url = (!skill.file.is_empty())
            .then(|| format!("file://{}", skill.file.trim_start_matches("file://")));
        // Non-breaking side bearings keep the wash from hugging the glyphs
        // and stop the chip from splitting across a wrap.
        let label = format!("\u{00A0}{}\u{00A0}", format_skill_title(&skill.name));
        let chip = SkillChipRun {
            range: 0..label.len(),
            open_url,
        };
        let (full, mentions) = if text.is_empty() {
            (label, Vec::new())
        } else {
            let offset = label.len() + 1;
            let shifted = mentions
                .iter()
                .map(|span| crate::composer::SentMentionSpan {
                    range: span.range.start + offset..span.range.end + offset,
                    ..span.clone()
                })
                .collect();
            (format!("{label} {text}"), shifted)
        };
        user_bubble_text_with_chip(
            row_id,
            SharedString::from(full),
            Arc::new(mentions),
            Some(chip),
            theme,
            Some(image_open),
        )
    }

    /// The right-aligned thumbnail strip above a user bubble.
    fn render_user_attachments(
        &mut self,
        row_id: &SharedString,
        atts: &[crate::attachments::UserImageAttachment],
        targets: Vec<crate::image_viewer::ViewerTarget>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::images::Snapshot;
        let mut strip = div()
            .id(SharedString::from(format!("{row_id}#attachments")))
            .w_full()
            .h(px(ATT_STRIP_H))
            .flex()
            .gap(px(8.0))
            .overflow_x_scroll()
            // Keep clicks local while allowing vertical wheel events to reach
            // the transcript list behind this horizontal strip.
            .block_mouse_except_scroll()
            .px(px(4.0))
            .pt(px(4.0))
            .child(div().flex_1());
        for (index, att) in atts.iter().enumerate() {
            let snapshot = self.attachment_state(&att.path, cx);
            let targets = targets.clone();
            let selected = targets
                .iter()
                .position(|t| t.path.as_ref() == att.path)
                .unwrap_or(index);
            let frame = div()
                .id(SharedString::from(format!("{row_id}#att{index}")))
                .flex_none()
                .w(px(ATT_THUMB_W))
                .h(px(ATT_THUMB_H))
                .rounded(px(8.0))
                .overflow_hidden()
                .border_1()
                .border_color(crate::theme::hairline(0.14))
                .bg(crate::theme::ink(0.055))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_image_viewer(targets.clone(), selected, window, cx)
                }));
            let frame = match snapshot {
                Snapshot::Loaded(thumb) => frame
                    .child(
                        img(thumb.pixels)
                            .w(px(ATT_THUMB_W - 2.0))
                            .h(px(ATT_THUMB_H - 2.0))
                            .rounded(px(7.0))
                            .object_fit(ObjectFit::Cover),
                    )
                    .into_any_element(),
                Snapshot::Loading => frame
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(crate::loaders::mini_mono_spinner(
                        format!("{row_id}-image-{index}"),
                        3.0,
                        Theme::of(cx).text_muted,
                        cx.entity_id(),
                        cx,
                    ))
                    .into_any_element(),
                Snapshot::Error { cause, .. } => frame
                    .flex()
                    .items_center()
                    .justify_center()
                    .tooltip(move |_, cx| {
                        cx.new(|_| crate::image_viewer::ViewerTooltip(cause.clone()))
                            .into()
                    })
                    .child(crate::icons::icon(crate::icons::DANGER_TRIANGLE).size(px(18.0)))
                    .into_any_element(),
            };
            strip = strip.child(frame);
        }
        strip.into_any_element()
    }

    // ---- rendering ----

    fn render_working_trailer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let now = chrono::Utc::now();
        let (sending, queued, elapsed_secs, seed) = if let Some(doc_id) = &self.doc_override {
            // A subagent doc has no Session row — `indicator_for` would read
            // the PARENT chat's live state into this tab. Liveness rides the
            // doc itself instead: the sink's assistant entry streams until
            // the subagent settles (run teardown finalizes abandoned sinks),
            // and a trailing USER entry is a steer still awaiting its reply
            // segment. Frozen snapshots never spin, whatever they claim.
            if !self.doc_live {
                return None;
            }
            let state = self.state.read(cx);
            let last = state.sub_transcript(doc_id).last()?;
            let live =
                last.status == Some(MessageStatus::Streaming) || last.role == MessageRole::User;
            if !live {
                return None;
            }
            let elapsed = (now.timestamp_millis() - last.created_at).max(0) / 1000;
            (false, false, elapsed, flavour_seed(doc_id))
        } else {
            let chat_id = self.chat_id.clone()?;
            if self
                .state
                .read(cx)
                .session_for(&chat_id)
                .is_some_and(|session| session.status == holt_proto::SessionStatus::Compacting)
            {
                let theme = Theme::of(cx).clone();
                return Some(
                    div()
                        .pt(px(Theme::SPACE_LG))
                        .child(self.compaction_label(true, &theme, cx))
                        .into_any_element(),
                );
            }
            // Failed-send state first: past the grace window the trailer IS
            // the retry affordance, whatever the indicator fell back to.
            if self.state.read(cx).send_undelivered(&chat_id, now) {
                let theme = Theme::of(cx).clone();
                return Some(
                    div()
                        .id("undelivered-retry")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(Theme::SPACE_SM))
                        .pt(px(Theme::SPACE_LG))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.danger)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| this.retry_send(cx)))
                        .child(SharedString::from("Not delivered — click to retry"))
                        .into_any_element(),
                );
            }
            let (sending, queued, elapsed) = {
                let state = self.state.read(cx);
                if state.indicator_for(&chat_id, now) != crate::state::Indicator::Working {
                    return None;
                }
                // During the send→turn window the session row's `started_at`
                // still belongs to the PREVIOUS turn — a timer based on the
                // send counted the round-trip and then restarted when the
                // turn actually began (user report). Bridge it as "Sending…"
                // with no timer instead; the word + timer start with the
                // turn.
                let turn_started = state.session_for(&chat_id).and_then(|s| s.started_at);
                let sending =
                    sending_bridge(state.pending_send_started(&chat_id, now), turn_started);
                // Degraded delivery path: the send is a durable local write
                // waiting on connectivity — say so instead of faking
                // progress. (The overlay holds while degraded, so this line
                // owns the surface until the ack or the failed state.)
                let queued = sending && state.chat_delivery_degraded(&chat_id);
                let elapsed = turn_started
                    .map(|t| now.signed_duration_since(t).num_seconds().max(0))
                    .unwrap_or(0);
                (sending, queued, elapsed)
            };
            (sending, queued, elapsed, flavour_seed(&chat_id))
        };
        let word = if queued {
            "Queued — will send automatically"
        } else if sending {
            "Sending"
        } else {
            flavour_word(seed, elapsed_secs)
        };
        let theme = Theme::of(cx).clone();
        // A pending Approval pauses the Turn; the trailer carries the
        // Esc-interrupt hint (prototype 3-A) so the keyboard path is
        // discoverable next to the running status.
        let approval_pending = self
            .rows
            .iter()
            .any(|row| matches!(row.kind, RowKind::Approval { .. }));
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::SPACE_SM))
                .pt(px(Theme::SPACE_LG))
                .text_size(crate::typography::ui_rems(11.0))
                .child(crate::loaders::gradient_spinner(
                    "working-indicator",
                    &theme,
                    2.5,
                    cx.entity_id(),
                    cx,
                ))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(if queued {
                            theme.warning
                        } else {
                            theme.text_muted
                        })
                        .child(SharedString::from(if queued {
                            word.to_string()
                        } else {
                            format!("{word}…")
                        })),
                )
                .when(!sending, |el| {
                    el.child(
                        div()
                            .text_color(theme.text_faint)
                            .child(SharedString::from(format_elapsed(elapsed_secs))),
                    )
                })
                .when(approval_pending, |el| {
                    el.child(div().flex_1()).child(
                        div()
                            .flex_none()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(3.0))
                            .text_color(theme.text_faint)
                            .child(
                                div()
                                    .px(px(3.0))
                                    .rounded(px(4.0))
                                    .border_1()
                                    .border_color(theme.hairline(0.14))
                                    .font_family(theme.font_mono.clone())
                                    .text_size(px(10.0))
                                    .line_height(px(14.0))
                                    .child("Esc"),
                            )
                            .child(" to interrupt"),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(ix).cloned() else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
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
            top_gap_for(ix.checked_sub(1).and_then(|i| self.rows.get(i)), &row)
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
        // Live-run loader rides under the LAST row's content (above its
        // clearance pad), so it sits right beneath the working reply.
        let trailer = (ix + 1 == self.rows.len())
            .then(|| self.render_working_trailer(cx))
            .flatten();

        let inner: AnyElement = match &row.kind {
            RowKind::User {
                text,
                mentions,
                attachments,
                badges,
                skill,
                pending,
            } => {
                let attachments = attachments.clone();
                let badges = badges.clone();
                let text = text.clone();
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
                // Attachment thumbnails ride ABOVE the bubble, right-aligned
                // (chat-view.tsx RowView: UserAttachmentStrip then the text
                // HStack); image-only sends show no bubble at all.
                let mut column = div().w_full().flex().flex_col();
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
                if !text.is_empty() || skill.is_some() {
                    let bubble_child = match skill {
                        Some(skill) => self.render_user_skill(
                            &row.id,
                            &skill,
                            &text,
                            &mentions,
                            &theme,
                            image_open.clone(),
                        ),
                        None => user_bubble_text_with_chip(
                            &row.id,
                            text,
                            mentions,
                            None,
                            &theme,
                            Some(image_open),
                        )
                        .into_any_element(),
                    };

                    // `min_w_0` is load-bearing: gpui text answers min/max-content
                    // probes with its UNWRAPPED width, so without it the bubble's
                    // automatic min-size is the full single-line width — the flex
                    // item can't shrink, `justify_end` pushes the overflow off the
                    // left edge, and long prompts render as one clipped line
                    // instead of wrapping inside the 80% column cap.
                    column = column.child(
                        div().w_full().flex().justify_end().child(
                            div()
                                .min_w_0()
                                .max_w(px(MAX_CONTENT_WIDTH * 0.8))
                                .bg(crate::theme::user_bubble_bg())
                                .rounded(px(Theme::BUBBLE_RADIUS))
                                .px(px(16.0))
                                .py(px(10.0))
                                .text_size(crate::typography::ui_rems(14.0))
                                .line_height(crate::typography::ui_rems(22.0))
                                .text_color(theme.text)
                                .when(pending, |el| el.opacity(0.65))
                                .child(bubble_child),
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
                };
                let highlight = self.code_highlight_for(&row.id, tree, Some(*block_ix), cx);
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
                };
                let highlight = self.code_highlight_for(&row.id, tree, Some(*block_ix), cx);
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
            RowKind::Approval { tool } => {
                self.render_approval_card(&row.id, tool, &theme, window, cx)
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
                None => skill_chip(name.clone(), file.clone(), *pending, &theme),
            },
            RowKind::ErrorChip { message } => error_chip(message.clone(), &theme),
            RowKind::Notice { message } => notice_row(message.clone(), &theme),
            RowKind::CompactionDivider { summary } => {
                self.render_compaction_divider(&row.id, summary, &theme, cx)
            }
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
        let strip = row.timestamp.map(|ms| {
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
            let metadata = metadata.child(timestamp).children(copy);
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
                    .child(inner)
                    .children(strip)
                    .children(trailer),
            )
            .into_any_element()
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
            if let Block::CodeBlock { language, code } = &top.block
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

    fn tool_diff_highlight_for(
        &mut self,
        row_id: &SharedString,
        tool_ix: usize,
        detail: &ToolDetail,
        cx: &mut Context<Self>,
    ) -> Option<Arc<crate::changes::DiffHighlights>> {
        let ToolDetail::Diff {
            file,
            old_text,
            new_text,
        } = detail
        else {
            return None;
        };
        let cache_row: SharedString = format!("{row_id}#tool-diff-{tool_ix}").into();
        let old = match old_text {
            Some(source) => {
                let path = file.old_path.as_deref().unwrap_or(&file.path);
                let lang = holt_syntax::language_for_path(path)?;
                Some(
                    self.highlights
                        .request(cache_row.clone(), 0, lang, source, cx)?,
                )
            }
            None => None,
        };
        let new = match new_text {
            Some(source) => {
                let lang = holt_syntax::language_for_path(&file.path)?;
                Some(self.highlights.request(cache_row, 1, lang, source, cx)?)
            }
            None => None,
        };
        Some(Arc::new(crate::changes::DiffHighlights { old, new }))
    }

    fn render_tool_group(
        &mut self,
        row_id: &SharedString,
        tools: &Arc<Vec<ToolItem>>,
        auto_open: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut fold = self.folds.get(row_id).copied().unwrap_or_default();
        // Agent/spawn chips never fold: they are their own row, always open,
        // no "Called N tools" header — a running subagent stays visible.
        let collapses = tool_group_collapses(tools);
        let open = !collapses || fold.open.unwrap_or(auto_open);
        // Chips render their EFFECTIVE detail: the precomputed doc-resident
        // one, upgraded in place by a fetched sidecar blob (chat2-sync A3).
        // Resolved per paint (a HashMap probe per chip) so fetched content
        // needs no row rebuild — arrival is a cx.notify, like a fold toggle.
        let details: Vec<Option<Arc<ToolDetail>>> = tools
            .iter()
            .map(|tool| {
                // Spawn chips never expand — the subagent doc is the record
                // of what the tool did, and an inline body would only repeat
                // it. The whole chip is the "open that doc" click instead.
                if is_spawn_link(tool) {
                    return None;
                }
                // Among fetched blobs, the most recently REQUESTED one wins —
                // a tool can carry both a diff and an output ref, and the
                // user's last click decides which upgrade is showing.
                let mut best: Option<(u64, Arc<ToolDetail>)> = None;
                for blob_ref in [&tool.diff_ref, &tool.output_ref].into_iter().flatten() {
                    if let Some(BlobFetch::Ready(detail)) = self.blob_details.get(blob_ref) {
                        let order = self.blob_fetch_order.get(blob_ref).copied().unwrap_or(0);
                        if best.as_ref().is_none_or(|(o, _)| order > *o) {
                            best = Some((order, detail.clone()));
                        }
                    }
                }
                best.map(|(_, d)| d).or_else(|| tool.detail.clone())
            })
            .collect();
        // Full-invocation blocks — with them, EVERY chip expands: the click
        // always answers "what exactly was this call?", output or not.
        let invocations: Vec<Option<Arc<ToolDetail>>> = tools
            .iter()
            .map(|tool| tool.invocation.clone().filter(|_| !is_spawn_link(tool)))
            .collect();
        // Fetch affordance under each open detail whose full payload is still
        // sidecar-only: `(ref, label)`. Diff offered first (the richer
        // upgrade), then the output — a fetched ref hands the affordance to
        // the NEXT unfetched one instead of retiring it (both must stay
        // reachable when a tool has both).
        let affordances: Vec<Option<ChipAffordance>> = tools
            .iter()
            .map(|tool| {
                // The currently-displayed ref (same recency rule as
                // `details` above): its affordance is spent; any OTHER
                // Ready ref stays offered as a no-fetch toggle.
                let shown: Option<&SharedString> = {
                    let mut best: Option<(u64, &SharedString)> = None;
                    for blob_ref in [&tool.diff_ref, &tool.output_ref].into_iter().flatten() {
                        if matches!(self.blob_details.get(blob_ref), Some(BlobFetch::Ready(_))) {
                            let order = self.blob_fetch_order.get(blob_ref).copied().unwrap_or(0);
                            if best.is_none_or(|(o, _)| order > o) {
                                best = Some((order, blob_ref));
                            }
                        }
                    }
                    best.map(|(_, r)| r)
                };
                let candidates = [
                    (tool.diff_ref.as_ref(), "diff", None),
                    (tool.output_ref.as_ref(), "output", tool.output_bytes),
                ];
                for (blob_ref, what, bytes) in candidates {
                    let Some(blob_ref) = blob_ref else { continue };
                    let label = match self.blob_details.get(blob_ref) {
                        Some(BlobFetch::Ready(_)) => {
                            if shown == Some(blob_ref) {
                                continue;
                            }
                            format!("Show full {what}")
                        }
                        Some(BlobFetch::Loading(_)) => format!("Loading full {what}…"),
                        Some(BlobFetch::Failed) => {
                            format!("Couldn't load full {what} — tap to retry")
                        }
                        None => match bytes {
                            Some(b) => format!("Show full {what} ({})", format_kb(b)),
                            None => format!("Show full {what}"),
                        },
                    };
                    return Some(ChipAffordance {
                        blob_ref: blob_ref.clone(),
                        label: SharedString::from(label),
                    });
                }
                None
            })
            .collect();
        // Per-chip card metrics (analytic, single source of truth for the
        // detail folds, the chips loop, and auto-flip arming): `(auto-derived
        // open, open height, closed height)`. Chips without a detail body —
        // bare calls, spawn links — have no card at all.
        let card_metrics: Vec<Option<(bool, f32, f32)>> = tools
            .iter()
            .enumerate()
            .map(|(ix, tool)| {
                if details[ix].is_none() && invocations[ix].is_none() {
                    return None;
                }
                let affordance_h = if affordances[ix].is_some() {
                    BLOB_AFFORDANCE_HEIGHT
                } else {
                    0.0
                };
                let open_h = CHIP_HEIGHT
                    + invocations[ix].as_deref().map_or(0.0, detail_height)
                    + details[ix].as_deref().map_or(0.0, detail_height)
                    + affordance_h;
                Some((tool.is_thought && !tool.resolved, open_h, CHIP_HEIGHT))
            })
            .collect();
        // Which chips have their detail block open (render-local, analytic —
        // the FINAL state; a mid-tween detail already counts as its target).
        let mut detail_folds: Vec<FoldState> = details
            .iter()
            .zip(&invocations)
            .enumerate()
            .map(|(ix, (detail, invocation))| {
                if detail.is_none() && invocation.is_none() {
                    return FoldState::default();
                }
                self.tool_details
                    .get(&SharedString::from(format!("{row_id}#d{ix}")))
                    .copied()
                    .unwrap_or_default()
            })
            .collect();
        let detail_opens: Vec<bool> = card_metrics
            .iter()
            .zip(&detail_folds)
            .map(|(card, fold)| {
                // A STREAMING thought chip defaults open (the live thinking
                // is the point); settled chips default closed. A user toggle
                // overrides either way.
                card.is_some_and(|(default_open, ..)| fold.open.unwrap_or(default_open))
            })
            .collect();
        let detail_highlights: Vec<Option<Arc<crate::changes::DiffHighlights>>> = details
            .iter()
            .enumerate()
            .map(|(ix, detail)| {
                detail
                    .as_deref()
                    .filter(|_| detail_opens[ix])
                    .and_then(|detail| self.tool_diff_highlight_for(row_id, ix, detail, cx))
            })
            .collect();
        let open_height = chips_height(tools.len())
            + details
                .iter()
                .zip(&invocations)
                .zip(&affordances)
                .zip(&detail_opens)
                .filter(|(_, open)| **open)
                .map(|(((detail, invocation), affordance), _)| {
                    invocation.as_deref().map_or(0.0, detail_height)
                        + detail.as_deref().map_or(0.0, detail_height)
                        + if affordance.is_some() {
                            BLOB_AFFORDANCE_HEIGHT
                        } else {
                            0.0
                        }
                })
                .sum::<f32>();
        let target = if open { open_height } else { 0.0 };

        // ---- auto-flip tween arming ----------------------------------------
        // The streaming tail moves between parts at doc-commit cadence: a
        // trailing group loses `auto_open` when text follows, a thought chip
        // closes when it loses the tail, and the settle closes both. Those
        // flips used to hard-cut the row height, and the bottom-pinned
        // viewport follows content height 1:1 — every flip read as a
        // page-wide jump (user report: jitter while the agent outputs). Arm
        // the same 200ms tween a user toggle gets, seeded from the height
        // committed at the previous render. First sight seeds silently (a
        // new row simply appears at its height); a user pin masks the auto
        // rule; reduced motion keeps the snap.
        {
            let reduced = motion::reduced_motion(cx);
            let group_flipped =
                collapses && !reduced && auto_flip_armed(fold.open, fold.auto_open_last, auto_open);
            if group_flipped {
                fold.from = fold.last_target.max(0.0);
                fold.epoch += 1;
                fold.toggled_at = Some(Instant::now());
            }
            // Detail flips are invisible inside a closing group body — the
            // body tween carries the motion alone — and only exist while the
            // group is open.
            let mut detail_armed = false;
            for (ix, card) in card_metrics.iter().enumerate() {
                let Some((default_open, open_h, closed_h)) = *card else {
                    continue;
                };
                let dfold = &mut detail_folds[ix];
                if !group_flipped
                    && open
                    && !reduced
                    && auto_flip_armed(dfold.open, dfold.auto_open_last, default_open)
                {
                    dfold.from = if default_open { closed_h } else { open_h };
                    dfold.epoch += 1;
                    dfold.toggled_at = Some(Instant::now());
                    detail_armed = true;
                }
                dfold.auto_open_last = Some(default_open);
            }
            if detail_armed && collapses && open && !reduced {
                // The group body's height is analytic over the FINAL detail
                // state, so it must tween alongside an auto-flipping card for
                // the row to track the card's edge frame-for-frame — the same
                // composition the click handler arms. `last_target` is the
                // pre-flip committed height, exact even when the flip lands
                // in the same commit as an appended chip.
                fold.from = fold.last_target.max(0.0);
                fold.epoch += 1;
                fold.toggled_at = Some(Instant::now());
            }
            if collapses {
                fold.auto_open_last = Some(auto_open);
            }
            fold.last_target = target;
            for (ix, dfold) in detail_folds.iter().enumerate() {
                if card_metrics[ix].is_some() {
                    self.tool_details
                        .insert(SharedString::from(format!("{row_id}#d{ix}")), *dfold);
                }
            }
            self.folds.insert(row_id.clone(), fold);
        }

        let summary = tool_group_summary(tools);

        let toggle_id = row_id.clone();
        // Header (holt tool-group.tsx): a small bare chevron centered over the
        // chips' guide rail, then the quiet 12px summary.
        let header = div()
            .id(SharedString::from(format!("{row_id}-hdr")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(4.0))
            .h(px(CHIP_HEIGHT))
            .cursor_pointer()
            .text_size(px(12.0))
            .line_height(px(18.0))
            // Quiet even when children failed: agents routinely have failed
            // probes mid-work, and a red HEADER read as "this whole step
            // broke" (user report). Failures still show on the individual
            // chips (destructive tint, holt tool-chip.tsx) and in the
            // summary's "· N failed" count.
            .text_color(theme.text_muted)
            .hover(|s| s.text_color(theme.text))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_fold(toggle_id.clone(), open_height, auto_open);
                cx.notify();
            }))
            .child(
                div()
                    .size(px(18.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(10.0))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(SharedString::from(if open { "▾" } else { "▸" })),
            )
            .child(
                div()
                    .min_w_0()
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .truncate()
                    .child(SharedString::from(summary)),
            );

        let chips = div()
            .pt(px(CHIPS_TOP_PAD))
            .flex()
            .flex_col()
            .gap(px(CHIP_GAP))
            .children(tools.iter().enumerate().map(|(ix, tool)| {
                // Spawn chips are LINKS, not accordions: the click opens the
                // subagent's transcript as a right-pane tab (the shell hosts
                // the surface — the chip only announces which doc it indexes).
                if let Some(doc_id) = tool.subagent_ref.clone().filter(|_| is_spawn_link(tool)) {
                    let chat_id = self.chat_id.clone().unwrap_or_default();
                    let title = subagent_tab_title(&tool.call);
                    let frozen = matches!(
                        tool.subagent_status,
                        Some(SubagentStatus::Done) | Some(SubagentStatus::Failed)
                    );
                    return subagent_chip(
                        tool,
                        SharedString::from(format!("{row_id}#s{ix}")),
                        cx.listener(move |_, _, _, cx| {
                            cx.emit(TranscriptEvent::OpenSubagent {
                                chat_id: chat_id.clone(),
                                doc_id: doc_id.to_string(),
                                title: title.to_string(),
                                frozen,
                            });
                        }),
                        collapses,
                        theme,
                        cx.entity_id(),
                        cx,
                    );
                }
                let detail = details[ix].clone();
                let invocation = invocations[ix].clone();
                if detail.is_none() && invocation.is_none() {
                    return tool_chip(tool, collapses, theme, cx.entity_id(), cx);
                }
                let affordance = affordances[ix].clone();
                // Card heights come from the precomputed metrics — the same
                // analytic values the auto-flip arming tweens between.
                let (_, open_h, closed_h) = card_metrics[ix].expect("carded chip has card metrics");
                let open = detail_opens[ix];
                let dfold = detail_folds[ix];
                let key = SharedString::from(format!("{row_id}#d{ix}"));
                // Expandable chip: header row + detail body in ONE flat
                // column (no card chrome) — the guide rail stretches with
                // the row, so an open detail never breaks the rail.
                //
                // The column's height is EXPLICIT, not intrinsic: it is what
                // the open/close tween animates, and it must match the
                // group's analytic height exactly or stacked chips drift
                // (the old bordered card overflowed by its own 2px of
                // borders — user report: "tool calls cut off at the bottom").
                let card_target = if open { open_h } else { closed_h };
                let animating = dfold.epoch > 0
                    && dfold
                        .toggled_at
                        .is_some_and(|at| at.elapsed() < FOLD_TWEEN_WINDOW);
                let toggle_key = key.clone();
                let group_key = row_id.clone();
                let mut card = div()
                    .when(collapses, |el| el.ml(px(12.0)))
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(
                        div()
                            .id(key.clone())
                            .h(px(CHIP_HEIGHT))
                            .flex_none()
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let entry =
                                    this.tool_details.entry(toggle_key.clone()).or_default();
                                let currently_open = entry.open.unwrap_or(false);
                                entry.from = if currently_open { open_h } else { closed_h };
                                entry.open = Some(!currently_open);
                                entry.epoch += 1;
                                entry.toggled_at = Some(Instant::now());
                                // Arm the GROUP body's height tween too (open
                                // state untouched): the body's height is
                                // analytic over the final detail state, so
                                // without a tween the row snaps to the target
                                // height while the card is still mid-tween —
                                // content below teleported on expand and the
                                // shrinking card clipped on collapse (user
                                // report). `open_height` was computed with
                                // the detail still in its pre-click state,
                                // which is exactly the tween's start; both
                                // tweens share the click instant and the
                                // RESIZE curve, so the row tracks the card's
                                // bottom edge frame-for-frame.
                                let group = this.folds.entry(group_key.clone()).or_default();
                                group.from = open_height;
                                group.epoch += 1;
                                group.toggled_at = Some(Instant::now());
                                cx.notify();
                            }))
                            .child(chip_header(tool, open, theme, cx.entity_id(), cx)),
                    );
                // The body stays mounted while the close tween shrinks over it.
                // Invocation first (what was asked), then output/diff (what
                // came back).
                if open || animating {
                    if let Some(invocation) = invocation.as_deref() {
                        card = card.child(detail_body(invocation, None, theme));
                    }
                    if let Some(detail) = detail.as_deref() {
                        card =
                            card.child(detail_body(detail, detail_highlights[ix].clone(), theme));
                    }
                    if let Some(ChipAffordance { blob_ref, label }) = affordance {
                        let loading = matches!(
                            self.blob_details.get(&blob_ref),
                            Some(BlobFetch::Loading(_))
                        );
                        let mut row = div()
                            .id(SharedString::from(format!("{key}-blob")))
                            .h(px(BLOB_AFFORDANCE_HEIGHT))
                            .flex_none()
                            .px(px(12.0))
                            .flex()
                            .items_center()
                            .text_size(px(10.5))
                            .text_color(theme.text_faint)
                            .child(label);
                        if !loading {
                            row = row
                                .cursor_pointer()
                                .hover(|s| s.text_color(theme.text_muted))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.spawn_blob_fetch(blob_ref.clone(), cx);
                                    cx.notify();
                                }));
                        }
                        card = card.child(row);
                    }
                }
                let card: AnyElement = if animating {
                    let from = dfold.from;
                    card.with_animation(
                        SharedString::from(format!("{key}-tween{}", dfold.epoch)),
                        RESIZE.animation(),
                        move |el, t| el.h(px(motion::lerp(from, card_target, t))),
                    )
                    .into_any_element()
                } else {
                    card.h(px(card_target)).into_any_element()
                };
                let card = div().min_w_0().flex_1().child(card);
                div()
                    .w_full()
                    .flex_none()
                    .flex()
                    .flex_row()
                    // Guide rail: no fixed height — stretches to the card,
                    // detail included. Agent-only groups skip it (no header
                    // chevron for the rail to sit under).
                    .when(collapses, |row| {
                        row.child(
                            div()
                                .ml(px(12.0))
                                .w(px(1.0))
                                .flex_none()
                                .bg(crate::theme::ink(0.08)),
                        )
                    })
                    .child(card)
                    .into_any_element()
            }));

        // Fold body: 200ms committed-height tween on a USER toggle only — and
        // only within a short window of the click. Auto-open (streaming) and
        // content growth never tween, and a SETTLED fold renders at its static
        // height: leaving the tween armed replayed it on every remount, which
        // in a virtualized list means every scroll-back-into-view (only `open`
        // toggles animate — composes with the stick spring). Agent groups skip
        // the fold entirely (always open, no header).
        let animating = collapses
            && fold.epoch > 0
            && fold
                .toggled_at
                .is_some_and(|at| at.elapsed() < FOLD_TWEEN_WINDOW);
        let body: AnyElement = if !collapses {
            chips.into_any_element()
        } else if animating {
            let from = fold.from;
            div()
                .overflow_hidden()
                .child(chips)
                .with_animation(
                    SharedString::from(format!("{row_id}-fold{}", fold.epoch)),
                    RESIZE.animation(),
                    move |el, t| el.h(px(motion::lerp(from, target, t))),
                )
                .into_any_element()
        } else {
            div()
                .overflow_hidden()
                .h(px(target))
                .child(chips)
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            // Tool summaries and cards are code-adjacent chrome. Detail bodies
            // retain their explicit mono/diff typography below this boundary.
            .font_family(theme.font_sans_fixed.clone())
            .when(collapses, |el| el.child(header))
            .child(body)
            .into_any_element()
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

fn user_bubble_text_with_chip(
    row_id: &SharedString,
    text: SharedString,
    mentions: Arc<Vec<crate::composer::SentMentionSpan>>,
    skill: Option<SkillChipRun>,
    theme: &Theme,
    image_open: Option<ImageOpen>,
) -> AnyElement {
    // Split runs at chip boundaries (spans are in order): body text keeps the
    // sans font, mention chips read as inline code, the skill chip reads in
    // the accent like the composer's. Size/line-height flow from the bubble's
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
        runs.push(chip_run(span.range.len()));
        at = span.range.end;
    }
    if at < text.len() {
        runs.push(body_run(text.len() - at));
    }
    let styled = StyledText::new(text.clone()).with_runs(runs);
    let layout = styled.layout().clone();
    let skill_range = skill.as_ref().map(|chip| chip.range.clone());
    let mut links = Vec::new();
    if let Some(chip) = skill
        && let Some(url) = chip.open_url
    {
        links.push((chip.range, url, false));
    }
    for mention in mentions
        .iter()
        .filter(|m| !m.is_dir && crate::images::is_image_path(&m.path))
    {
        links.push((mention.range.clone(), mention.path.to_string(), true));
    }
    let text_el = if links.is_empty() {
        styled.into_any_element()
    } else {
        gpui::InteractiveText::new(SharedString::from(format!("{row_id}#links")), styled)
            .on_click(
                links.iter().map(|l| l.0.clone()).collect(),
                move |index, window, cx| {
                    let (_, path, image) = &links[index];
                    if *image {
                        if let Some(open) = &image_open {
                            open(path, window, cx);
                        }
                    } else {
                        cx.open_url(path);
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
        move |_, _, window, _| {
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
                paint(window, &span.range, wash);
            }
            render::paint_text_selection(window, &sel_key, &text, &layout, &sel_theme);
        },
    )
    .absolute()
    .size_full();
    div()
        .relative()
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
/// `SKILL.md` so the user can inspect exactly what the agent was told to
/// follow. Right-aligned like the user bubble it replaces.
fn skill_chip(name: SharedString, file: SharedString, pending: bool, theme: &Theme) -> AnyElement {
    let open_url =
        (!file.is_empty()).then(|| format!("file://{}", file.trim_start_matches("file://")));
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
    match (clickable_id, open_url) {
        (Some(id), Some(url)) => chip
            .id(id)
            .cursor_pointer()
            .hover(|el| el.opacity(0.8))
            .on_click(move |_, _, cx| {
                cx.open_url(&url);
            })
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
        ToolCall::Glob { .. } => crate::icons::FOLDER_WITH_FILES,
        ToolCall::WebFetch { .. } | ToolCall::WebSearch { .. } => crate::icons::GLOBAL,
        ToolCall::Todo { .. } => crate::icons::CHECKLIST,
        call if is_agent_call(call) => crate::icons::BOT,
        ToolCall::Mcp { .. } | ToolCall::Unknown { .. } => crate::icons::WIDGET,
    }
}

/// The body of an expanded chip card, under the header's separator. Diffs
/// render through the changes pane's section body — the real component, with
/// hunk headers, dual line-number gutters, accent bars, row washes, and
/// syntax runs — so an inline tool diff is indistinguishable from the
/// checkout diff sidebar. Output renders as a code block: verbatim mono
/// lines, indentation intact, counted-tail truncation.
fn detail_body(
    detail: &ToolDetail,
    diff_highlights: Option<Arc<crate::changes::DiffHighlights>>,
    theme: &Theme,
) -> AnyElement {
    let body = div().w_full().min_w_0().flex().flex_col().overflow_hidden();
    match detail {
        // No comment layer: an inline tool diff is a record of what the
        // agent already did, not a review surface.
        ToolDetail::Diff { file, .. } => body
            .child(crate::changes::render_file_body_with_syntax(
                file,
                diff_highlights,
                theme,
            ))
            .into_any_element(),
        ToolDetail::Stats { stats } => body
            .py(px(6.0))
            .font_family(theme.font_mono.clone())
            .text_size(px(11.5))
            .children(stats.iter().map(|stat| {
                div()
                    .h(px(OUTPUT_LINE_HEIGHT))
                    .w_full()
                    .min_w_0()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_color(theme.text.opacity(0.85))
                            .child(SharedString::from(stat.path.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.success)
                            .child(SharedString::from(format!("+{}", stat.additions))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.danger)
                            .child(SharedString::from(format!("−{}", stat.deletions))),
                    )
            }))
            .into_any_element(),
        ToolDetail::Output {
            lines,
            truncated_by,
        } => body
            .py(px(6.0))
            .font_family(theme.font_mono.clone())
            .text_size(px(11.5))
            .children(lines.iter().map(|line| {
                div()
                    .h(px(OUTPUT_LINE_HEIGHT))
                    .w_full()
                    .min_w_0()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .text_color(theme.text.opacity(0.85))
                    .child(div().w_full().min_w_0().truncate().child(line.clone()))
            }))
            .when(*truncated_by > 0, |block| {
                block.child(more_lines_row(*truncated_by, theme))
            })
            .into_any_element(),
        ToolDetail::Thought {
            lines,
            truncated_by,
        } => body
            .py(px(6.0))
            .text_size(px(12.0))
            .children(lines.iter().map(|line| {
                let row = div()
                    .h(px(OUTPUT_LINE_HEIGHT))
                    .w_full()
                    .min_w_0()
                    .px(px(12.0))
                    .flex()
                    .items_center();
                let Some((text, runs)) = thought_line_text(line, theme) else {
                    return row; // blank separator row
                };
                row.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .truncate()
                        .child(StyledText::new(text).with_runs(runs)),
                )
            }))
            .when(*truncated_by > 0, |block| {
                block.child(more_lines_row(*truncated_by, theme))
            })
            .into_any_element(),
    }
}

/// The counted-tail row under a truncated Output/Thought detail.
fn more_lines_row(truncated_by: usize, theme: &Theme) -> gpui::Div {
    div()
        .h(px(OUTPUT_LINE_HEIGHT))
        .px(px(12.0))
        .flex()
        .items_center()
        .text_size(px(10.5))
        .text_color(theme.text_faint)
        .child(SharedString::from(format!("… {truncated_by} more lines")))
}

/// Shape one flattened thought line into gpui text runs — the detail-body
/// palette: extra-muted foreground prose (a thought is context, dimmer than
/// the reply around it), semibold for bold, violet mono for code, underlined
/// links (NOT clickable — a thought is a record, not a surface).
fn thought_line_text(line: &[InlineRun], theme: &Theme) -> Option<(SharedString, Vec<TextRun>)> {
    let mut text = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    for run in line {
        if run.text.is_empty() {
            continue;
        }
        let mut f = if run.style.code {
            gpui::font(theme.font_mono.clone())
        } else {
            gpui::font(theme.font_sans.clone())
        };
        if run.style.bold {
            f.weight = gpui::FontWeight::SEMIBOLD;
        }
        if run.style.italic {
            f.style = gpui::FontStyle::Italic;
        }
        runs.push(TextRun {
            len: run.text.len(),
            font: f,
            color: if run.style.code {
                render::inline_code_text(theme)
            } else {
                theme.text.opacity(0.6)
            },
            background_color: None,
            underline: run.style.link.is_some().then_some(gpui::UnderlineStyle {
                color: Some(theme.text_muted),
                thickness: px(1.0),
                wavy: false,
            }),
            strikethrough: run.style.strikethrough.then_some(gpui::StrikethroughStyle {
                thickness: px(1.0),
                color: Some(theme.text_muted),
            }),
        });
        text.push_str(&run.text);
    }
    if text.trim().is_empty() {
        return None;
    }
    Some((text.into(), runs))
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
    cx: &mut gpui::App,
) -> gpui::Div {
    let (label, detail) = if tool.is_thought {
        ("Thought process", String::new())
    } else {
        tool_chip_content(&tool.call)
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
        .when_some(
            // The settled verdict (ADR-0014): a small tinted marker after the
            // detail — "✓ Approved", "⊘ Denied · "note"", "⚡ Prefix exempt", …
            tool.gate.as_ref().and_then(|gate| match &gate.state {
                ToolGateState::Settled { verdict } => Some(super::verdict_chip(verdict)),
                ToolGateState::Pending => None,
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
    cx: &mut gpui::App,
) -> gpui::Div {
    chip_header_row(tool, Some(ChipTrail::Chevron { open }), theme, view, cx)
}

/// Max chars a subagent tab title keeps. The strip chip is fixed-width and
/// truncates visually, but the derived title also rides drag ghosts and any
/// future pickers — cap it at the source.
const SUBAGENT_TITLE_MAX: usize = 40;

/// First line of `text`, trimmed, capped at `max` chars with an ellipsis.
fn title_line(text: &str, max: usize) -> Option<String> {
    let line = text.lines().find(|l| !l.trim().is_empty())?.trim();
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        out.push('…');
    }
    Some(out)
}

/// Drop a leading "Agent"/"Task" genus (with its `:` and spacing) from a
/// spawn-title candidate. Only a real word boundary strips — "Taskmaster"
/// keeps its name. A bare "Agent"/"Task" strips to "" (no context at all).
fn strip_spawn_prefix(text: &str) -> &str {
    let t = text.trim();
    for prefix in ["agent", "task"] {
        if t.len() >= prefix.len()
            && t.is_char_boundary(prefix.len())
            && t[..prefix.len()].eq_ignore_ascii_case(prefix)
        {
            let rest = &t[prefix.len()..];
            if rest.is_empty() {
                return "";
            }
            if rest.starts_with(':') || rest.starts_with(char::is_whitespace) {
                return rest.trim_start_matches(':').trim();
            }
        }
    }
    t
}

/// Tab title for a spawn chip's subagent surface: the BARE task description
/// ("verify the marker pipeline"). The chip keeps the tool's fuller name —
/// a fixed-width tab spent on "Agent: " never shows the task, so the genus
/// is stripped here and the call input's description/prompt fields back up
/// a bare name (older docs); "Subagent" only as the last resort.
fn subagent_tab_title(call: &ToolCall) -> SharedString {
    let (name, input) = match call {
        ToolCall::Unknown { name, input } => (name.as_str(), input.as_ref()),
        ToolCall::Mcp { tool, input, .. } => (tool.as_str(), input.as_ref()),
        _ => return "Subagent".into(),
    };
    let candidates = [
        Some(name),
        input.and_then(|i| i.get("description")?.as_str()),
        input.and_then(|i| i.get("prompt")?.as_str()),
    ];
    for text in candidates.into_iter().flatten() {
        if let Some(title) = title_line(strip_spawn_prefix(text), SUBAGENT_TITLE_MAX) {
            return title.into();
        }
    }
    "Subagent".into()
}

/// A plain (non-expandable) chip: a flat quiet row, plus the group guide rail
/// when the chip lives under a collapsible header.
fn tool_chip(
    tool: &ToolItem,
    rail: bool,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut gpui::App,
) -> AnyElement {
    div()
        .h(px(CHIP_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .when(rail, |row| {
            row.child(
                div()
                    .ml(px(12.0))
                    .h_full()
                    .w(px(1.0))
                    .flex_none()
                    .bg(crate::theme::ink(0.08)),
            )
        })
        .child(
            div()
                .when(rail, |el| el.ml(px(12.0)))
                .min_w_0()
                .flex_1()
                .overflow_hidden()
                .child(chip_header_row(tool, None, theme, view, cx)),
        )
        .into_any_element()
}

/// A spawn chip: same flat row as [`tool_chip`], but the WHOLE row is the
/// "open the subagent tab" click (open-arrow affordance in the trailing
/// slot). No accordion — an inline body would only repeat the subagent's own
/// transcript. The group guide rail is omitted for agent-only rows (no
/// collapse header for it to hang from).
fn subagent_chip(
    tool: &ToolItem,
    id: SharedString,
    on_open: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    rail: bool,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut gpui::App,
) -> AnyElement {
    div()
        .h(px(CHIP_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .when(rail, |row| {
            row.child(
                div()
                    .ml(px(12.0))
                    .h_full()
                    .w(px(1.0))
                    .flex_none()
                    .bg(crate::theme::ink(0.08)),
            )
        })
        .child(
            div()
                .id(id)
                .when(rail, |el| el.ml(px(12.0)))
                .min_w_0()
                .flex_1()
                .overflow_hidden()
                .rounded(px(6.0))
                .cursor_pointer()
                // Invisible until hover — the flat row carries no chrome, the
                // wash is pure clickability feedback.
                .hover(|s| s.bg(crate::theme::ink(0.04)))
                .on_click(on_open)
                .child(chip_header_row(
                    tool,
                    Some(ChipTrail::OpenArrow),
                    theme,
                    view,
                    cx,
                )),
        )
        .into_any_element()
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
        // Drop note editors whose approval settled or scrolled away with a
        // chat switch (a verdict also closes its own editor eagerly).
        self.prune_approval_notes();
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
    fn subagent_tab_titles() {
        // The tab is the BARE task — the "Agent:" genus is stripped.
        let named = ToolCall::Unknown {
            name: "Agent: scan repo".into(),
            input: None,
        };
        assert_eq!(subagent_tab_title(&named).as_ref(), "scan repo");
        // A bare "Task"/"Agent" digs the description out of the call input
        // (which sheds any genus of its own).
        let bare = ToolCall::Unknown {
            name: "Task".into(),
            input: Some(serde_json::json!({
                "description": "Agent: audit the auth flow",
                "prompt": "very long instructions…",
            })),
        };
        assert_eq!(subagent_tab_title(&bare).as_ref(), "audit the auth flow");
        // Word boundaries only — a name that merely STARTS with the genus
        // keeps itself.
        let compound = ToolCall::Unknown {
            name: "Taskmaster".into(),
            input: None,
        };
        assert_eq!(subagent_tab_title(&compound).as_ref(), "Taskmaster");
        // Nothing to derive → the generic label.
        let blank = ToolCall::Unknown {
            name: "agent".into(),
            input: None,
        };
        assert_eq!(subagent_tab_title(&blank).as_ref(), "Subagent");
        // Absurd lengths cap with an ellipsis; multiline prompts keep only
        // their first line.
        let long = ToolCall::Unknown {
            name: "x".repeat(120),
            input: None,
        };
        let title = subagent_tab_title(&long);
        assert_eq!(title.chars().count(), SUBAGENT_TITLE_MAX + 1);
        assert!(title.ends_with('…'));
        // Non-spawn-shaped calls stay generic.
        assert_eq!(
            subagent_tab_title(&ToolCall::Exec {
                command: "ls".into()
            })
            .as_ref(),
            "Subagent"
        );
    }

    #[test]
    fn tool_chip_labels_per_kind() {
        assert_eq!(
            tool_chip_content(&ToolCall::Exec {
                command: "cargo test".into()
            }),
            ("Run", "cargo test".to_string())
        );
        assert_eq!(
            tool_chip_content(&ToolCall::Search {
                pattern: "foo".into(),
                path: Some("src".into())
            }),
            ("Search", "foo in src".to_string())
        );
        assert_eq!(
            tool_chip_content(&ToolCall::ApplyPatch { path: None }),
            ("Patch", "workspace".to_string())
        );
        assert_eq!(
            tool_chip_content(&ToolCall::Mcp {
                server: "gh".into(),
                tool: "issues".into(),
                input: None
            }),
            ("MCP", "gh · issues".to_string())
        );
        let todo = ToolCall::Todo {
            items: vec![
                holt_proto::TodoItem {
                    text: "a".into(),
                    done: true,
                },
                holt_proto::TodoItem {
                    text: "b".into(),
                    done: false,
                },
            ],
        };
        assert_eq!(tool_chip_content(&todo), ("Todo", "1/2 done".to_string()));
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
