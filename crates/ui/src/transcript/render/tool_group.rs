//! The tool-group row: nested tool chips with foldable detail bodies (diffs
//! through the changes pane's renderer, outputs as code blocks), subagent
//! spawn chips, and their tab-strip titles. `render_row` delegates whole
//! ToolGroup rows here.

use super::super::model::is_agent_tool;
use super::*;

const SUBAGENT_HEADER_AVATARS: usize = 5;
const SUBAGENT_HEADER_AVATAR_SIZE: f32 = 22.0;
const SUBAGENT_HEADER_AVATAR_GAP: f32 = 4.0;

/// "30 subagents · 8 running · 12 queued · 2 failed" — lifecycle counts by
/// the same mood the faces wear, with children still waiting for a slot
/// (unresolved, never bound to a doc) split out; zero buckets omitted.
/// Settled children's token totals trail it — spend is shown, not capped.
fn subagent_fanout_summary(tools: &[ToolItem]) -> String {
    let (mut running, mut queued, mut done, mut failed) = (0, 0, 0, 0);
    for tool in tools {
        match subagent_avatar_mood(tool) {
            AvatarMood::Running
                if !tool.resolved
                    && tool.subagent_ref.is_none()
                    && tool.subagent_status.is_none() =>
            {
                queued += 1
            }
            AvatarMood::Running => running += 1,
            AvatarMood::Done => done += 1,
            AvatarMood::Failed => failed += 1,
        }
    }
    let count = if tools.len() == 1 {
        t!(
            "conversation_transcript.tool.subagent_fanout.count.one",
            count = 1
        )
    } else {
        t!(
            "conversation_transcript.tool.subagent_fanout.count.other",
            count = tools.len()
        )
    };
    // A workflow's phase rows lead with their phase.
    let mut parts: Vec<String> = tools
        .first()
        .and_then(|tool| super::super::model::tool_phase(&tool.call))
        .map(str::to_owned)
        .into_iter()
        .collect();
    parts.push(count.into_owned());
    for (n, key) in [
        (
            running,
            "conversation_transcript.tool.subagent_fanout.running",
        ),
        (
            queued,
            "conversation_transcript.tool.subagent_fanout.queued",
        ),
        (
            failed,
            "conversation_transcript.tool.subagent_fanout.failed",
        ),
        (done, "conversation_transcript.tool.subagent_fanout.done"),
    ] {
        if n > 0 {
            parts.push(t!(key, count = n).into_owned());
        }
    }
    let tokens: u64 = tools.iter().filter_map(|tool| tool.subagent_usage).sum();
    if tokens > 0 {
        parts.push(
            t!(
                "conversation_transcript.tool.tokens",
                count = crate::token_display::compact_tokens(tokens)
            )
            .into_owned(),
        );
    }
    parts.join(" · ")
}

fn subagent_header_order(tools: &[ToolItem]) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..tools.len()).collect();
    // Keep active work visible in the five-face summary. The sort is stable,
    // so each lifecycle group retains transcript order.
    indices.sort_by_key(|ix| {
        let tool = &tools[*ix];
        if matches!(tool.subagent_status, Some(SubagentStatus::Running))
            || (!tool.resolved && tool.subagent_ref.is_some())
        {
            0
        } else if !tool.resolved && tool.subagent_ref.is_none() {
            1
        } else {
            2
        }
    });
    indices
}

impl Transcript {
    fn render_subagent_fanout(
        &mut self,
        row_id: &SharedString,
        tools: &Arc<Vec<ToolItem>>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chip_cwd = self
            .chat_id
            .as_deref()
            .and_then(|id| self.state.read(cx).chat_row(id).and_then(|c| c.cwd.clone()));
        let identities: Vec<Option<AgentIdentity>> = tools
            .iter()
            .map(|tool| {
                self.agent_identities
                    .identity_for(tool.call_id.as_deref(), tool.subagent_ref.as_deref())
            })
            .collect();
        let order = subagent_header_order(tools);
        let summary = subagent_fanout_summary(tools);
        let face_count = order.len().min(SUBAGENT_HEADER_AVATARS);
        let faces_width = face_count as f32 * SUBAGENT_HEADER_AVATAR_SIZE
            + face_count.saturating_sub(1) as f32 * SUBAGENT_HEADER_AVATAR_GAP;
        let key = SharedString::from(format!("{row_id}#agents-fanout"));
        let mut fold = self.folds.get(&key).copied().unwrap_or_default();
        let open = fold.open.unwrap_or(false);
        let open_height = tools.len() as f32 * CHIP_HEIGHT;
        let target = if open { open_height } else { 0.0 };
        let animating = fold.epoch > 0
            && fold
                .toggled_at
                .is_some_and(|at| at.elapsed() < FOLD_TWEEN_WINDOW);
        fold.last_target = target;
        self.folds.insert(key.clone(), fold);
        let toggle_id = key.clone();
        // Same shape as the ordinary group header (chevron slot, quiet 12px
        // summary) so the two read as one family; the faces sit between.
        let header = div()
            .id(key.clone())
            .debug_selector(|| "subagent-fanout".to_string())
            .h(px(CHIP_HEIGHT))
            .px(px(4.0))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .text_size(px(12.0))
            .line_height(px(18.0))
            .text_color(theme.text_muted)
            .hover(|s| s.text_color(theme.text))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_fold(toggle_id.clone(), open_height, false);
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
                    .w(px(faces_width))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(SUBAGENT_HEADER_AVATAR_GAP))
                    .debug_selector(|| "subagent-fanout-faces".to_string())
                    .children(order.iter().take(SUBAGENT_HEADER_AVATARS).enumerate().map(
                        |(position, ix)| {
                            let tool = &tools[*ix];
                            div()
                                .size(px(SUBAGENT_HEADER_AVATAR_SIZE))
                                .flex_none()
                                .debug_selector(move || format!("subagent-fanout-face-{position}"))
                                .when_some(identities[*ix].as_ref(), |face, identity| {
                                    face.child(agent_avatar(
                                        identity,
                                        subagent_avatar_mood(tool),
                                        cx.entity_id(),
                                        cx,
                                    ))
                                })
                        },
                    )),
            )
            .child(
                div()
                    .min_w_0()
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .truncate()
                    .debug_selector(|| "subagent-fanout-summary".to_string())
                    .child(SharedString::from(summary)),
            );
        let rows = div()
            .flex()
            .flex_col()
            .children(tools.iter().enumerate().map(|(ix, tool)| {
                let identity = identities[ix].clone();
                let chip = if let Some(doc_id) = tool.subagent_ref.clone() {
                    let chat_id = self.chat_id.clone().unwrap_or_default();
                    let title = subagent_tab_title(&tool.call);
                    let frozen = matches!(
                        tool.subagent_status,
                        Some(SubagentStatus::Done) | Some(SubagentStatus::Failed)
                    );
                    subagent_chip(
                        tool,
                        identity,
                        SharedString::from(format!("{row_id}#fanout-s{ix}")),
                        cx.listener(move |_, _, _, cx| {
                            cx.emit(TranscriptEvent::OpenSubagent {
                                chat_id: chat_id.clone(),
                                doc_id: doc_id.to_string(),
                                title: title.to_string(),
                                frozen,
                            });
                        }),
                        true,
                        theme,
                        cx.entity_id(),
                        chip_cwd.as_deref(),
                        cx,
                    )
                } else {
                    tool_chip(
                        tool,
                        identity,
                        true,
                        (SharedString::from(format!("{row_id}#fanout-r{ix}")), None),
                        theme,
                        cx.entity_id(),
                        chip_cwd.as_deref(),
                        cx,
                    )
                };
                div()
                    .h(px(CHIP_HEIGHT))
                    .w_full()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .debug_selector(|| format!("subagent-fanout-row-{ix}"))
                    .child(chip)
                    .into_any_element()
            }));
        let body: AnyElement = if animating {
            let from = fold.from;
            div()
                .overflow_hidden()
                .debug_selector(|| "subagent-fanout-body".to_string())
                .child(rows)
                .with_animation(
                    SharedString::from(format!("{key}-tween{}", fold.epoch)),
                    RESIZE.animation(),
                    move |el, t| el.h(px(motion::lerp(from, target, t))),
                )
                .into_any_element()
        } else {
            div()
                .overflow_hidden()
                .debug_selector(|| "subagent-fanout-body".to_string())
                .h(px(target))
                .child(rows)
                .into_any_element()
        };
        div()
            .w_full()
            .pt(px(CHIPS_TOP_PAD))
            .flex()
            .flex_col()
            .font_family(theme.font_sans_fixed.clone())
            .child(header)
            .child(body)
            .into_any_element()
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

    pub(super) fn render_tool_group(
        &mut self,
        row_id: &SharedString,
        tools: &Arc<Vec<ToolItem>>,
        auto_open: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !tools.is_empty() && tools.iter().all(is_agent_tool) {
            return self.render_subagent_fanout(row_id, tools, theme, cx);
        }
        let mut fold = self.folds.get(row_id).copied().unwrap_or_default();
        // Path-bearing chip details render cwd-relative when the path is under
        // the chat's working directory (the absolute prefix is noise in a
        // 12px detail slot). None for pinned subagent docs — they aren't chat
        // rows, so those chips keep absolute paths.
        let chip_cwd = self
            .chat_id
            .as_deref()
            .and_then(|id| self.state.read(cx).chat_row(id).and_then(|c| c.cwd.clone()));
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
                    (
                        tool.diff_ref.as_ref(),
                        t!("conversation_transcript.tool.what.diff"),
                        None,
                    ),
                    (
                        tool.output_ref.as_ref(),
                        t!("conversation_transcript.tool.what.output"),
                        tool.output_bytes,
                    ),
                ];
                for (blob_ref, what, bytes) in candidates {
                    let Some(blob_ref) = blob_ref else { continue };
                    let label = match self.blob_details.get(blob_ref) {
                        Some(BlobFetch::Ready(_)) => {
                            if shown == Some(blob_ref) {
                                continue;
                            }
                            t!("conversation_transcript.tool.show_full", what = what).into_owned()
                        }
                        Some(BlobFetch::Loading(_)) => {
                            t!("conversation_transcript.tool.loading_full", what = what)
                                .into_owned()
                        }
                        Some(BlobFetch::Failed) => {
                            t!("conversation_transcript.tool.load_failed", what = what).into_owned()
                        }
                        None => match bytes {
                            Some(b) => t!(
                                "conversation_transcript.tool.show_full_size",
                                what = what,
                                size = format_kb(b)
                            )
                            .into_owned(),
                            None => t!("conversation_transcript.tool.show_full", what = what)
                                .into_owned(),
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
            .map(|(ix, _tool)| {
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
                // No chip defaults open, thoughts included (user request:
                // the live thinking stays folded until the user opens it).
                // A user toggle pins either way.
                Some((false, open_h, CHIP_HEIGHT))
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
                // Every chip defaults closed — thought chips included, so
                // the streaming thinking no longer auto-expands. A user
                // toggle overrides.
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
        // trailing group loses `auto_open` when text follows, and the settle
        // closes it. Those flips used to hard-cut the row height, and the
        // bottom-pinned viewport follows content height 1:1 — every flip
        // read as a page-wide jump (user report: jitter while the agent
        // outputs). Arm the same 200ms tween a user toggle gets, seeded
        // from the height committed at the previous render. First sight
        // seeds silently (a new row simply appears at its height); a user
        // pin masks the auto rule; reduced motion keeps the snap.
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

        // Agent chips wear their identity from the first paint: the draw
        // keys on the call id while the spawn is in flight and adopts the
        // subagent's doc id when the ref lands (the doc half persists, so a
        // reopened chat keeps its names and faces).
        let identities: Vec<Option<AgentIdentity>> = tools
            .iter()
            .map(|tool| {
                if !is_agent_call(&tool.call) {
                    return None;
                }
                self.agent_identities
                    .identity_for(tool.call_id.as_deref(), tool.subagent_ref.as_deref())
            })
            .collect();

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

        // The rewind affordance (ADR-rewind-to-a-step-is-all-or-nothing):
        // the main chat's settled chips only — never a subagent tab, never
        // while a Turn runs.
        let rewind_chat = self
            .chat_id
            .clone()
            .filter(|_| self.doc_override.is_none())
            .filter(|id| {
                self.state.read(cx).indicator_for(id, chrono::Utc::now())
                    != crate::state::Indicator::Working
            });
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
                        identities[ix].clone(),
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
                        chip_cwd.as_deref(),
                        cx,
                    );
                }
                let rewind_group = SharedString::from(format!("{row_id}#r{ix}"));
                let rewind = match (&rewind_chat, &tool.call_id) {
                    (Some(chat_id), Some(call_id)) if tool.resolved && !tool.is_thought => {
                        let chat_id = chat_id.clone();
                        let tool_call_id = call_id.to_string();
                        Some(crate::rewind::rewind_button(
                            rewind_group.clone(),
                            SharedString::from(format!("{row_id}#rb{ix}")),
                            theme,
                            cx.listener(move |_, _, _, cx| {
                                // The header under it toggles the chip.
                                cx.stop_propagation();
                                cx.emit(TranscriptEvent::RewindToStep {
                                    chat_id: chat_id.clone(),
                                    tool_call_id: tool_call_id.clone(),
                                });
                            }),
                        ))
                    }
                    _ => None,
                };
                let detail = details[ix].clone();
                let invocation = invocations[ix].clone();
                if detail.is_none() && invocation.is_none() {
                    return tool_chip(
                        tool,
                        identities[ix].clone(),
                        collapses,
                        (rewind_group, rewind),
                        theme,
                        cx.entity_id(),
                        chip_cwd.as_deref(),
                        cx,
                    );
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
                            .group(rewind_group)
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
                            .child(chip_header(
                                tool,
                                identities[ix].clone(),
                                open,
                                theme,
                                cx.entity_id(),
                                chip_cwd.as_deref(),
                                cx,
                            ))
                            .children(rewind),
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
        .child(SharedString::from(
            t!(
                "conversation_transcript.tool.more_lines",
                count = truncated_by
            )
            .into_owned(),
        ))
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
        _ => return t!("conversation_transcript.tool.subagent").into(),
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
    t!("conversation_transcript.tool.subagent").into()
}

/// A plain (non-expandable) chip: a flat quiet row, plus the group guide rail
/// when the chip lives under a collapsible header.
#[expect(clippy::too_many_arguments)]
fn tool_chip(
    tool: &ToolItem,
    identity: Option<AgentIdentity>,
    rail: bool,
    (group, rewind): (SharedString, Option<AnyElement>),
    theme: &Theme,
    view: gpui::EntityId,
    cwd: Option<&str>,
    cx: &mut gpui::App,
) -> AnyElement {
    div()
        .group(group)
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
                .child(chip_header_row(tool, identity, None, theme, view, cwd, cx)),
        )
        .children(rewind)
        .into_any_element()
}

/// A spawn chip: same flat row as [`tool_chip`], but the WHOLE row is the
/// "open the subagent tab" click (open-arrow affordance in the trailing
/// slot). No accordion — an inline body would only repeat the subagent's own
/// transcript. The guide rail hangs from whichever header folds it — the
/// mixed group's summary or the agent-only fan-out row.
#[expect(clippy::too_many_arguments)]
fn subagent_chip(
    tool: &ToolItem,
    identity: Option<AgentIdentity>,
    id: SharedString,
    on_open: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    rail: bool,
    theme: &Theme,
    view: gpui::EntityId,
    cwd: Option<&str>,
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
                .id(id.clone())
                .debug_selector(move || format!("subagent-chip-{id}"))
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
                    identity,
                    Some(ChipTrail::OpenArrow),
                    theme,
                    view,
                    cwd,
                    cx,
                )),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use holt_proto::view::tool_chip_content;

    fn agent(ix: usize, status: Option<SubagentStatus>, resolved: bool, has_ref: bool) -> ToolItem {
        ToolItem {
            call: ToolCall::Unknown {
                name: format!("Agent: task {ix}"),
                input: None,
            },
            call_id: Some(SharedString::from(format!("call-{ix}"))),
            is_error: status == Some(SubagentStatus::Failed),
            resolved,
            detail: None,
            invocation: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            subagent_ref: has_ref.then(|| SharedString::from(format!("chat--sub--{ix}"))),
            subagent_status: status,
            subagent_tail: None,
            subagent_usage: None,
            is_thought: false,
            gate: None,
        }
    }

    #[test]
    fn fanout_header_shows_active_faces_and_lifecycle_counts() {
        let mut tools = Vec::new();
        for ix in 0..30 {
            let (status, resolved, has_ref) = match ix {
                0..=15 => (Some(SubagentStatus::Done), true, true),
                16 | 17 => (Some(SubagentStatus::Failed), true, true),
                18..=25 => (Some(SubagentStatus::Running), true, true),
                _ => (None, false, false),
            };
            tools.push(agent(ix, status, resolved, has_ref));
        }
        let order = subagent_header_order(&tools);
        assert_eq!(&order[..SUBAGENT_HEADER_AVATARS], &[18, 19, 20, 21, 22]);
        assert_eq!(&order[8..12], &[26, 27, 28, 29]);
        assert_eq!(
            subagent_fanout_summary(&tools),
            "30 subagents · 8 running · 4 queued · 2 failed · 16 done"
        );

        let mut settled: Vec<ToolItem> = (0..5)
            .map(|ix| agent(ix, Some(SubagentStatus::Done), true, true))
            .collect();
        assert_eq!(subagent_header_order(&settled), [0, 1, 2, 3, 4]);
        assert_eq!(subagent_fanout_summary(&settled), "5 subagents · 5 done");
        settled[0].subagent_usage = Some(900_000);
        settled[1].subagent_usage = Some(300_000);
        assert_eq!(
            subagent_fanout_summary(&settled),
            "5 subagents · 5 done · 1.2M tokens"
        );
        assert_eq!(
            subagent_fanout_summary(&settled[2..3]),
            "1 subagent · 1 done"
        );
        let mut phased = settled[2].clone();
        phased.call = ToolCall::Unknown {
            name: "Agent: task".into(),
            input: Some(serde_json::json!({"phase": "Verify"})),
        };
        assert_eq!(
            subagent_fanout_summary(&[phased]),
            "Verify · 1 subagent · 1 done"
        );
    }

    #[test]
    fn subagent_tab_titles() {
        let _locale = crate::i18n::tests::locale_lock();
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
        assert_eq!(
            subagent_tab_title(&blank).as_ref(),
            t!("conversation_transcript.tool.subagent").as_ref()
        );
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
            t!("conversation_transcript.tool.subagent").as_ref()
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
            ("MCP", "Gh issues".to_string())
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
}
