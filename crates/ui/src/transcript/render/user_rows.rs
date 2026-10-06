//! Row-kind renderers: the skill-invocation chip that opens a reply, the
//! compaction divider, the user bubble's skill/attachment strips, and the
//! working trailer. `render_row` (parent module) dispatches here per RowKind.

use super::*;

impl Transcript {
    /// The invocation chip that OPENS the agent's reply (seeded by the
    /// engine ahead of any thinking): a flush-left process row in the
    /// tool-chip language — quiet header, thinking-style fold. Expanding
    /// reveals the exact `<skill>` block the model received, with the
    /// source file one click away. Collapsing tweens the measured height
    /// to zero like the tool-group folds.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_skill_invocation(
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
                // Test seam (the turn-card pattern): bounds lookup by name.
                .debug_selector(|| "skill-toggle".to_string())
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
            let open_path = file.trim_start_matches("file://").to_string();
            let scroll = self.nested_scroll_handle(row_id);
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
                        // Nested reading viewport inside the transcript list:
                        // occlude so one wheel gesture cannot scroll both this
                        // body and the outer list (ADR-0013); the wheel
                        // handler chains the unabsorbed remainder to the list
                        // at the body's scroll boundary.
                        .occlude()
                        .track_scroll(&scroll)
                        .on_scroll_wheel(cx.listener(move |this, _, _, cx| {
                            this.chain_nested_scroll(&scroll, cx);
                        }))
                        .font_family(theme.font_mono.clone())
                        .text_size(crate::typography::ui_rems(11.0))
                        .line_height(crate::typography::ui_rems(16.0))
                        .text_color(theme.text_muted.opacity(0.9))
                        .child(content.clone()),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("{row_id}#skill-file")))
                        .debug_selector(|| "skill-open-file".to_string())
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .cursor_pointer()
                        .hover(|el| el.opacity(0.75))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(super::TranscriptEvent::OpenSkillFile {
                                path: open_path.clone(),
                            });
                        }))
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
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.text_muted)
            .child(
                crate::icons::icon(crate::icons::CONTEXT_COMPACT)
                    .size_4()
                    .flex_none()
                    .text_color(theme.text_muted.opacity(opacity)),
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
                "Compacting Context"
            } else {
                "Context Compacted"
            })
    }

    /// The compaction rule: hairlines either side of a centered label.
    pub(super) fn compaction_rule(center: impl IntoElement, theme: &Theme) -> gpui::Div {
        let line = || div().flex_1().h(px(1.0)).bg(theme.hairline(0.12));
        div()
            .h(px(CHIP_HEIGHT))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(line())
            .child(center)
            .child(line())
    }

    /// A divider rule marking where the model's verbatim memory begins; the
    /// label folds open to the summary the model carries.
    pub(super) fn render_compaction_divider(
        &mut self,
        row_id: &SharedString,
        summary: &Arc<BlockTree>,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fold = self.folds.get(row_id).copied().unwrap_or_default();
        let open = fold.open.unwrap_or(false);
        let toggle_row_id = row_id.clone();
        let toggle =
            div()
                .id(SharedString::from(format!("{row_id}#divider-toggle")))
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .px(px(8.0))
                .py(px(2.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .hover(|el| el.bg(theme.ink(0.04)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_skill_fold(toggle_row_id.clone(), cx)
                }))
                .child(self.compaction_label(false, theme, cx))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(10.0))
                        .text_color(theme.text_muted.opacity(0.8))
                        .child(SharedString::from(if open { "▾" } else { "▸" })),
                );
        let mut column = div()
            .w_full()
            .flex()
            .flex_col()
            .child(Self::compaction_rule(toggle, theme));
        if open {
            let opts = RenderOptions {
                row_key: row_id.clone(),
                veil: None,
                cache: (!render_cache_disabled()).then(|| self.render_cache.clone()),
                now: Instant::now(),
                copy: Some(self.copy_ui_for(row_id, cx)),
                mermaid_ui: Some(self.mermaid_ui_for(row_id, summary, cx)),
            };
            let highlight = self.code_highlight_for(row_id, summary, None, cx);
            let mermaid = self.mermaid_for(row_id, summary, None, cx);
            let body = render::render_tree(
                summary,
                &opts,
                theme,
                window,
                &|ix| highlight.get(&ix).cloned().flatten(),
                &|ix, _| mermaid.get(&ix).cloned().flatten(),
            );
            let scroll = self.nested_scroll_handle(row_id);
            column = column.child(
                div()
                    .w_full()
                    .mt(px(6.0))
                    .mb(px(6.0))
                    .px(px(14.0))
                    .py(px(12.0))
                    .rounded(px(8.0))
                    .bg(theme.ink(0.03))
                    .id(SharedString::from(format!("{row_id}#divider-body")))
                    .max_h(px(320.0))
                    .overflow_y_scroll()
                    // This is a nested reading viewport. Occlude the outer
                    // transcript hitbox so one wheel gesture cannot scroll
                    // both the summary and the transcript list; the wheel
                    // handler chains the unabsorbed remainder to the list at
                    // the summary's scroll boundary.
                    .occlude()
                    .track_scroll(&scroll)
                    .on_scroll_wheel(cx.listener(move |this, _, _, cx| {
                        this.chain_nested_scroll(&scroll, cx);
                    }))
                    .child(body),
            );
        }
        column.into_any_element()
    }

    /// A `/skill` invocation inside the user bubble (legacy transcripts):
    /// the skill title as an accent chip at the head of the text flow (the
    /// composer's treatment), with a click through to the source file. New
    /// invocations keep the raw mention text inline instead (ADR-0035).
    /// The `<skill>` block itself rides the AGENT entry's opening chip —
    /// this bubble only records what the user did.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_user_skill(
        &mut self,
        row_id: &SharedString,
        skill: &Arc<UserSkill>,
        text: &SharedString,
        mentions: &Arc<Vec<crate::composer::SentMentionSpan>>,
        theme: &Theme,
        clicks: BubbleClicks,
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
            clicks,
        )
    }

    /// The right-aligned thumbnail strip above a user bubble.
    pub(super) fn render_user_attachments(
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
                        cx.new(|_| crate::popover::TextTooltip(cause.clone()))
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

    pub(super) fn render_working_trailer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
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
                let label = self.compaction_label(true, &theme, cx).px(px(8.0));
                return Some(
                    div()
                        .pt(px(Theme::SPACE_LG))
                        .child(Self::compaction_rule(label, &theme))
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
        // discoverable next to the running status. Pending gates build no
        // row, so the check reads the doc this transcript follows.
        let approval_pending = {
            let state = self.state.read(cx);
            match &self.doc_override {
                Some(doc_id) => {
                    super::super::pending_approval_gate(state.sub_transcript(doc_id)).is_some()
                }
                None => super::super::pending_approval_gate(&state.transcript).is_some(),
            }
        };
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
}
