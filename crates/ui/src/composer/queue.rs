//! Presentation and commands for the selected chat's engine-owned queue.

use std::time::{Duration, Instant};

use gpui::{
    Context, Entity, Focusable, KeyDownEvent, Render, Subscription, Window, div, prelude::*, px,
};
use holt_rpc::methods;

use super::{Composer, ComposerInput, ComposerInputEvent};
use crate::motion::{self, AnimationExt as _};
use crate::theme::Theme;

/// Grace period after [`motion::COLLAPSE`] ends during which the tween is
/// still considered animating. One frame's slack so a missed render near the
/// deadline doesn't drop the `with_animation` wrapper (next paint would
/// otherwise snap to the post-tween target without the easing curve).
const QUEUE_DISCLOSURE_TWEEN_GRACE: Duration = Duration::from_millis(120);

/// The header's Resume control: automatic execution is paused with work
/// waiting. A queue-level error is not an unreadable queue and must not
/// hide the control (ADR-0021).
pub(super) fn queue_resume_visible(queue: &holt_proto::MessageQueue) -> bool {
    queue.paused && !queue.pending.is_empty()
}

/// Keep long queues from taking over the composer; `uniform_list` scrolls the
/// rows that do not fit in this viewport.
const QUEUE_LIST_MAX_HEIGHT: f32 = 240.0;

/// Interruptible height tween for the queue's expand/collapse body — the same
/// recipe as the shell sidebar disclosure (shell/spaces.rs). `epoch` bumps on
/// every toggle so the element-id-keyed `with_animation` clock remounts; a
/// mid-flight reversal captures the current interpolated height as the new
/// `from`, so the user never sees a snap when they click again.
#[derive(Clone, Copy)]
pub(super) struct QueueDisclosureMotion {
    pub(super) epoch: u64,
    pub(super) from: f32,
    pub(super) to: f32,
    started: Instant,
}

impl QueueDisclosureMotion {
    fn new(epoch: u64, from: f32, to: f32) -> Self {
        Self {
            epoch,
            from,
            to,
            started: Instant::now(),
        }
    }

    fn current(self) -> f32 {
        let total = motion::COLLAPSE.total().as_secs_f32();
        let raw = if total > 0.0 {
            self.started.elapsed().as_secs_f32() / total
        } else {
            1.0
        };
        motion::lerp(self.from, self.to, motion::COLLAPSE.progress(raw))
    }

    fn animating(self) -> bool {
        self.started.elapsed() < motion::COLLAPSE.total() + QUEUE_DISCLOSURE_TWEEN_GRACE
    }
}

impl Composer {
    /// Begin (or reverse) the queue body's expand/collapse tween. Captures the
    /// current interpolated height as the new `from` if a tween is already in
    /// flight, otherwise starts from `resting_height`. Mirrors
    /// [`crate::shell::Shell::begin_sidebar_disclosure_motion`].
    pub(super) fn begin_queue_disclosure_motion(
        &mut self,
        resting_height: f32,
        target_height: f32,
    ) {
        let previous = self.queue_motion;
        let from = previous
            .filter(|motion| motion.animating())
            .map(QueueDisclosureMotion::current)
            .unwrap_or(resting_height);
        let epoch = previous.map_or(1, |motion| motion.epoch + 1);
        self.queue_motion = Some(QueueDisclosureMotion::new(epoch, from, target_height));
    }
}

/// Inline editor state for one pending queue message. The editor renders as
/// a borderless input inside the message's own row — Enter saves, Escape
/// cancels — and never touches the per-chat drafts the composer input
/// holds. `original` is the body the editor opened with: an item that left
/// the pending list (started, deleted elsewhere) dismisses an untouched
/// editor but keeps one holding unsaved text, so Save can surface the
/// engine's "already executing" refusal instead of silently dropping it.
/// For a skill invocation the editor holds only its extra instructions —
/// name, kind, and captured configuration are the queue's.
pub(super) struct QueueEdit {
    pub(super) message_id: String,
    pub(super) original: String,
    pub(super) input: Entity<ComposerInput>,
    pub(super) focus_pending: bool,
    _events: Subscription,
}

impl Composer {
    pub(super) fn retry_submission(&mut self, cx: &mut Context<Self>) {
        if self.sending {
            return;
        }
        let chat_id = self.current_key.clone();
        let Some((text, params)) = self.failed_submissions.get(&chat_id).cloned() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.sending = true;
        cx.notify();
        self.send_task = Some(cx.spawn(async move |this, cx| {
            let result = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                methods::QUEUE_COMMAND,
                params,
                std::time::Duration::from_secs(30),
            )
            .await;
            this.update(cx, |this, cx| {
                this.sending = false;
                match result {
                    Ok(_) => {
                        this.failed_submissions.remove(&chat_id);
                        if this
                            .drafts
                            .get(&chat_id)
                            .is_some_and(|draft| draft.trim() == text)
                        {
                            this.drafts.remove(&chat_id);
                        }
                        if this.failure_key.as_ref() == Some(&chat_id) {
                            this.failure = None;
                        }
                        if this.current_key == chat_id && this.input.read(cx).text().trim() == text
                        {
                            this.input.update(cx, |input, cx| input.set_text("", cx));
                        }
                    }
                    Err(error) => {
                        this.failure = Some(format!("Send failed: {error}").into());
                        this.failure_key = Some(chat_id);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Start editing a pending item's one editable field — an ordinary
    /// message's body, or a skill invocation's extra instructions. The
    /// captured model, the kind, and the position belong to the queue and
    /// are not offered here.
    pub(super) fn open_queue_edit(&mut self, message_id: &str, cx: &mut Context<Self>) {
        let (kind, body) = {
            let state = self.state.read(cx);
            let Some(item) = state.message_queue.as_ref().and_then(|queue| {
                queue
                    .pending
                    .iter()
                    .find(|item| item.message_id == message_id)
            }) else {
                return;
            };
            let body = match item.kind {
                holt_proto::PendingKind::Skill => {
                    item.extra_instructions.clone().unwrap_or_default()
                }
                holt_proto::PendingKind::Ordinary => item.request.prompt.clone(),
                // A pending Compaction has no edit affordance — never open
                // an editor against its (unused) prompt.
                holt_proto::PendingKind::Compact => return,
            };
            (item.kind, body)
        };
        let placeholder = match kind {
            holt_proto::PendingKind::Skill => "Edit the extra instructions",
            _ => "Edit the queued message",
        };
        let input = cx.new(|cx| ComposerInput::new(placeholder, cx));
        input.update(cx, |input, cx| input.set_text(body.clone(), cx));
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.save_queue_edit(cx);
            }
        });
        self.queue_edit = Some(QueueEdit {
            message_id: message_id.into(),
            original: body,
            input,
            focus_pending: true,
            _events: events,
        });
        self.queue_expanded = true;
        cx.notify();
    }

    pub(super) fn run_queue_now(&mut self, message_id: String, cx: &mut Context<Self>) {
        if self.queue_busy {
            return;
        }
        let chat_id = self.current_key.clone();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.queue_busy = true;
        cx.notify();
        self.queue_task = Some(cx.spawn(async move |this, cx| {
            let result = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                methods::QUEUE_COMMAND,
                serde_json::json!({
                    "chatId": chat_id,
                    "command": {
                        "kind": "steer",
                        "prompt": "",
                        "messageId": message_id,
                    },
                }),
                std::time::Duration::from_secs(30),
            )
            .await;
            this.update(cx, |this, cx| {
                this.queue_busy = false;
                if let Err(error) = result {
                    this.failure = Some(format!("Could not run queued message: {error}").into());
                    this.failure_key = Some(chat_id);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Save the edit through the typed RPC boundary. The acknowledgement
    /// means the queue file was persisted; a failure keeps the editor open
    /// with the unsaved text.
    pub(super) fn save_queue_edit(&mut self, cx: &mut Context<Self>) {
        if self.queue_busy {
            return;
        }
        let Some(edit) = self.queue_edit.as_ref() else {
            return;
        };
        let message_id = edit.message_id.clone();
        let prompt = edit.input.read(cx).text().trim().to_string();
        if prompt.is_empty() {
            return;
        }
        let chat_id = self.current_key.clone();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.queue_busy = true;
        cx.notify();
        self.queue_task = Some(cx.spawn(async move |this, cx| {
            let result = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                methods::EDIT_QUEUED_MESSAGE,
                serde_json::json!({"chatId": chat_id, "messageId": message_id, "prompt": prompt}),
                std::time::Duration::from_secs(30),
            )
            .await;
            this.update(cx, |this, cx| {
                this.queue_busy = false;
                match result {
                    Ok(_) => {
                        // Only close the editor that saved; a stale one was
                        // already dismissed when its message left the queue.
                        if this
                            .queue_edit
                            .as_ref()
                            .is_some_and(|edit| edit.message_id == message_id)
                        {
                            this.queue_edit = None;
                        }
                    }
                    Err(error) => {
                        this.failure = Some(format!("Could not save the edit: {error}").into());
                        this.failure_key = Some(chat_id);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn cancel_queue_edit(&mut self, cx: &mut Context<Self>) {
        if self.queue_edit.take().is_some() {
            cx.notify();
        }
    }

    /// Remove a pending message. A stale action (the item already started)
    /// surfaces the engine's refusal instead of touching the active Turn.
    pub(super) fn delete_queued_message(&mut self, message_id: String, cx: &mut Context<Self>) {
        if self.queue_busy {
            return;
        }
        let chat_id = self.current_key.clone();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.queue_busy = true;
        cx.notify();
        self.queue_task = Some(cx.spawn(async move |this, cx| {
            let result = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                methods::DELETE_QUEUED_MESSAGE,
                serde_json::json!({"chatId": chat_id, "messageId": message_id}),
                std::time::Duration::from_secs(30),
            )
            .await;
            this.update(cx, |this, cx| {
                this.queue_busy = false;
                if let Err(error) = result {
                    this.failure = Some(format!("Could not delete the message: {error}").into());
                    this.failure_key = Some(chat_id);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Resume the queue's automatic execution (ADR-0021): lifts the pause
    /// and the single-run scope of an attended send, so parked work drains.
    pub(super) fn resume_queue(&mut self, cx: &mut Context<Self>) {
        if self.queue_busy {
            return;
        }
        let chat_id = self.current_key.clone();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.queue_busy = true;
        cx.notify();
        self.queue_task = Some(cx.spawn(async move |this, cx| {
            let result = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                methods::CONTINUE_MESSAGE_QUEUE,
                serde_json::json!({ "chatId": chat_id }),
                std::time::Duration::from_secs(30),
            )
            .await;
            this.update(cx, |this, cx| {
                this.queue_busy = false;
                if let Err(error) = result {
                    this.failure = Some(format!("Could not resume the queue: {error}").into());
                    this.failure_key = Some(chat_id);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn render_message_queue(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(queue) = self.state.read(cx).message_queue.clone() else {
            return div().into_any_element();
        };
        // A stale editor (its message started or was removed elsewhere) is
        // dismissed only when it holds no unsaved text; an edited one stays
        // open so Save surfaces the engine's "already executing" refusal
        // instead of the edit being silently dropped.
        if let Some(edit) = self.queue_edit.as_ref() {
            let still_pending = queue
                .pending
                .iter()
                .any(|item| item.message_id == edit.message_id);
            let untouched = edit.input.read(cx).text() == edit.original;
            if !still_pending && untouched {
                self.queue_edit = None;
            }
        }
        if queue.pending.is_empty() && queue.error.is_none() {
            return div().into_any_element();
        }
        if let Some(edit) = self.queue_edit.as_mut()
            && std::mem::take(&mut edit.focus_pending)
        {
            let handle = edit.input.focus_handle(cx);
            window.focus(&handle, cx);
        }
        let theme = Theme::of(cx);
        let pending = queue.pending.clone();
        let count = pending.len();
        let editing_id = self.queue_edit.as_ref().map(|edit| edit.message_id.clone());
        // uniform_list's item closure sees only `&mut App`, so row handlers
        // dispatch through the composer's weak handle instead of a listener.
        let composer = cx.weak_entity();
        // The header's Resume control dispatches the same way; the list
        // closure consumes the first handle.
        let header_composer = composer.clone();
        // The editor renders inline in its own row; the entity is cloned in so
        // the closure can hand it to that row.
        let edit_input = self.queue_edit.as_ref().map(|edit| edit.input.clone());
        let error = queue
            .error
            .clone()
            .or_else(|| pending.first().and_then(|item| item.error.clone()));
        let header = if queue.paused {
            format!("Queue paused ({count})")
        } else {
            format!("Queued ({count})")
        };
        let expanded = self.queue_expanded;
        // Body height for the expand/collapse tween: the list viewport (the
        // rows beyond the cap scroll inside `uniform_list`) plus the error
        // line, with a single `gap_2` (8px) between them when both are
        // mounted. Captured by the toggle handlers below so the tween starts
        // from the height the user actually saw when they clicked.
        let list_visible = count > 0;
        let error_visible = error.is_some();
        let body_present = list_visible || error_visible;
        let list_content_height = if list_visible {
            30.0 * count as f32 + 8.0
        } else {
            0.0
        };
        let list_height = list_content_height.min(QUEUE_LIST_MAX_HEIGHT);
        // Single-line body text at the queue's 12px type size.
        const QUEUE_ERROR_LINE_HEIGHT: f32 = 22.0;
        const QUEUE_INTER_GAP: f32 = 8.0;
        let error_height = if error_visible {
            QUEUE_ERROR_LINE_HEIGHT
        } else {
            0.0
        };
        let inter_gap = if list_visible && error_visible {
            QUEUE_INTER_GAP
        } else {
            0.0
        };
        let body_height = list_height + inter_gap + error_height;
        let target_body_height = if expanded { body_height } else { 0.0 };
        let body = if body_present {
            // Always mounted during the tween (the frame's `overflow_hidden`
            // + animated height clip the content). When `body_height` is 0
            // (no list, no error) we wouldn't reach this — `body_present`
            // gates it.
            let mut content = div().w_full().min_w_0().flex().flex_col().gap_2();
            if list_visible {
                content = content.child(
                    gpui::uniform_list("pending-messages", count, move |range, _, cx| {
                        let theme = Theme::of(cx);
                        range
                            .map(|ix| {
                                let item = &pending[ix];
                                // Typed rows (ticket 04): the command kind
                                // leads, never a raw slash directive or the
                                // engine-formatted skill body.
                                let title = match item.kind {
                                    holt_proto::PendingKind::Ordinary => {
                                        item.request.prompt.clone()
                                    }
                                    holt_proto::PendingKind::Skill => format!(
                                        "/skill {}",
                                        item.skill_name.as_deref().unwrap_or_default()
                                    ),
                                    holt_proto::PendingKind::Compact => "/compact".to_string(),
                                };
                                let editing =
                                    editing_id.as_deref() == Some(item.message_id.as_str());
                                let row = div()
                                    .id(gpui::SharedString::from(item.message_id.clone()))
                                    .h(px(30.0))
                                    .w_full()
                                    .min_w_0()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .pr(px(6.0))
                                    .text_size(crate::typography::ui_rems(12.5))
                                    .when(!editing, |el| el.hover(|el| el.bg(theme.glass_hover())))
                                    .when(editing, |el| {
                                        el.bg(theme.glass_hover()).on_key_down({
                                            let composer = composer.clone();
                                            move |event: &KeyDownEvent, _, cx| {
                                                // Escape cancels the edit; it
                                                // must never reach the shell's
                                                // Turn-interrupt binding.
                                                if event.keystroke.key == "escape" {
                                                    cx.stop_propagation();
                                                    composer
                                                        .update(cx, |this, cx| {
                                                            this.cancel_queue_edit(cx)
                                                        })
                                                        .ok();
                                                }
                                            }
                                        })
                                    })
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .items_center()
                                            .gap(px(8.0))
                                            .pl(px(12.0))
                                            // Editing swaps the row's title
                                            // for a borderless inline input.
                                            .when(editing, |el| {
                                                el.when_some(edit_input.clone(), |el, input| {
                                                    el.child(div().w_full().min_w_0().child(input))
                                                })
                                            })
                                            .when(!editing, |el| {
                                                el.child(
                                                    div()
                                                        .truncate()
                                                        .text_color(theme.text)
                                                        .child(title),
                                                )
                                            }),
                                    );
                                // The editing row's affordances: Enter saves,
                                // Escape cancels, plus explicit confirm /
                                // cancel icons on the right.
                                if editing {
                                    return row.child(
                                        div()
                                            .flex_none()
                                            .flex()
                                            .items_center()
                                            .gap(px(2.0))
                                            .child(queue_row_action(
                                                format!("queue-save-{}", item.message_id),
                                                "Save the queued message edit",
                                                crate::icons::CHECK,
                                                theme.glass_hover(),
                                                theme.text_muted,
                                                {
                                                    let composer = composer.clone();
                                                    move |_, _, cx| {
                                                        composer
                                                            .update(cx, |this, cx| {
                                                                cx.stop_propagation();
                                                                this.save_queue_edit(cx);
                                                            })
                                                            .ok();
                                                    }
                                                },
                                            ))
                                            .child(queue_row_action(
                                                format!("queue-cancel-{}", item.message_id),
                                                "Cancel editing the queued message",
                                                crate::icons::CLOSE,
                                                theme.glass_hover(),
                                                theme.text_muted,
                                                {
                                                    let composer = composer.clone();
                                                    move |_, _, cx| {
                                                        composer
                                                            .update(cx, |this, cx| {
                                                                this.cancel_queue_edit(cx)
                                                            })
                                                            .ok();
                                                    }
                                                },
                                            )),
                                    );
                                }
                                let delete = queue_row_action(
                                    format!("queue-delete-{}", item.message_id),
                                    "Delete queued message",
                                    crate::icons::TRASH_BIN_MINIMALISTIC,
                                    theme.glass_hover(),
                                    theme.text_muted,
                                    {
                                        let composer = composer.clone();
                                        let message_id = item.message_id.clone();
                                        move |_, _, cx| {
                                            composer
                                                .update(cx, |this, cx| {
                                                    this.delete_queued_message(
                                                        message_id.clone(),
                                                        cx,
                                                    )
                                                })
                                                .ok();
                                        }
                                    },
                                );
                                // A pending Compaction executes strictly in
                                // order: delete is its only action.
                                if item.kind == holt_proto::PendingKind::Compact {
                                    return row.child(
                                        div()
                                            .flex_none()
                                            .flex()
                                            .items_center()
                                            .gap(px(2.0))
                                            .child(delete),
                                    );
                                }
                                row.child(
                                    div()
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .gap(px(2.0))
                                        .child(queue_row_action(
                                            format!("queue-run-{}", item.message_id),
                                            "Run queued message now",
                                            crate::icons::ARROW_RIGHT,
                                            theme.glass_hover(),
                                            theme.text_muted,
                                            {
                                                let composer = composer.clone();
                                                let message_id = item.message_id.clone();
                                                move |_, _, cx| {
                                                    composer
                                                        .update(cx, |this, cx| {
                                                            this.run_queue_now(
                                                                message_id.clone(),
                                                                cx,
                                                            )
                                                        })
                                                        .ok();
                                                }
                                            },
                                        ))
                                        .child(queue_row_action(
                                            format!("queue-edit-{}", item.message_id),
                                            "Edit queued message",
                                            crate::icons::PEN,
                                            theme.glass_hover(),
                                            theme.text_muted,
                                            {
                                                let composer = composer.clone();
                                                let message_id = item.message_id.clone();
                                                move |_, _, cx| {
                                                    composer
                                                        .update(cx, |this, cx| {
                                                            this.open_queue_edit(&message_id, cx)
                                                        })
                                                        .ok();
                                                }
                                            },
                                        ))
                                        .child(delete),
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .h(px(list_height))
                    .max_h(px(QUEUE_LIST_MAX_HEIGHT))
                    .w_full()
                    .min_w_0()
                    .occlude(),
                );
            }
            if let Some(error) = error.as_ref() {
                content = content.child(
                    div()
                        .min_w_0()
                        .text_color(theme.danger)
                        .child(error.clone()),
                );
            }
            // Frame clips the always-mounted body; the tween interpolates
            // its height between 0 and `body_height`, with a soft opacity
            // ramp and a 2px top lift for the "settling into place" feel.
            let frame = div()
                .w_full()
                .min_w_0()
                .flex_none()
                .overflow_hidden()
                .child(content);
            let tween = self.queue_motion.filter(|m| m.animating());
            let rendered: gpui::AnyElement = if let Some(tween) = tween {
                let epoch = tween.epoch;
                let from = tween.from;
                let to = tween.to;
                let denom = body_height.max(1.0);
                frame
                    .with_animation(
                        gpui::SharedString::from(format!("queue-body-{epoch}")),
                        motion::COLLAPSE.animation(),
                        move |el, t| {
                            let h = motion::lerp(from, to, t);
                            let reveal = (h / denom).clamp(0.0, 1.0);
                            el.h(px(h))
                                .opacity(0.4 + 0.6 * reveal)
                                .relative()
                                .top(px(-2.0 * (1.0 - reveal)))
                        },
                    )
                    .into_any_element()
            } else {
                frame.h(px(target_body_height)).into_any_element()
            };
            rendered
        } else {
            div().into_any_element()
        };
        // The toggle handlers capture `body_height` from the render closure
        // so the tween starts from the height the user actually saw.
        let click_toggle = cx.listener(move |this, _, _, cx| {
            let was_expanded = this.queue_expanded;
            let from = if was_expanded { body_height } else { 0.0 };
            let to = if was_expanded { 0.0 } else { body_height };
            this.queue_expanded = !was_expanded;
            this.begin_queue_disclosure_motion(from, to);
            cx.notify();
        });
        let key_toggle = cx.listener(move |this, event: &KeyDownEvent, _, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                cx.stop_propagation();
                let was_expanded = this.queue_expanded;
                let from = if was_expanded { body_height } else { 0.0 };
                let to = if was_expanded { 0.0 } else { body_height };
                this.queue_expanded = !was_expanded;
                this.begin_queue_disclosure_motion(from, to);
                cx.notify();
            }
        });
        div()
            .id("message-queue")
            .w_full()
            .min_w_0()
            // This is an overlay above the transcript, so it must paint an
            // opaque surface; otherwise transcript text remains visible
            // through the queue rows.
            .bg(theme.bg)
            .border_1()
            .border_b_0()
            .border_color(theme.border)
            .rounded_tl(px(12.0))
            .rounded_tr(px(12.0))
            .px_2()
            .py_1()
            .flex()
            .flex_col()
            .gap_2()
            .text_size(crate::typography::ui_rems(12.0))
            .child(
                div()
                    .id("message-queue-header")
                    .role(gpui::Role::Button)
                    .aria_label(if expanded {
                        "Collapse message queue"
                    } else {
                        "Expand message queue"
                    })
                    .focusable()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px(px(4.0))
                    .py(px(2.0))
                    .child(div().text_color(theme.text_muted).child(header))
                    // When the queue is paused with work waiting, the header carries the
                    // Resume control; it stops propagation so the header's own
                    // collapse toggle does not fire.
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(2.0))
                            .when(queue_resume_visible(&queue), |el| {
                                el.child(queue_row_action(
                                    "queue-resume".into(),
                                    "Resume the message queue",
                                    crate::icons::ARROW_RIGHT,
                                    theme.glass_hover(),
                                    theme.text_muted,
                                    {
                                        let composer = header_composer.clone();
                                        move |_, _, cx| {
                                            cx.stop_propagation();
                                            composer
                                                .update(cx, |this, cx| this.resume_queue(cx))
                                                .ok();
                                        }
                                    },
                                ))
                            })
                            .child(
                                crate::icons::icon(if expanded {
                                    crate::icons::ALT_ARROW_DOWN
                                } else {
                                    crate::icons::ALT_ARROW_RIGHT
                                })
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                            ),
                    )
                    .on_click(click_toggle)
                    .on_key_down(key_toggle),
            )
            .child(body)
            .into_any_element()
    }
}

/// One 26px icon button in a pending row (edit / delete). `on_click` runs
/// with `&mut App`; row handlers dispatch through the composer's weak handle.
fn queue_row_action(
    id: String,
    label: &'static str,
    icon_path: &'static str,
    hover_bg: gpui::Hsla,
    icon_color: gpui::Hsla,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(gpui::SharedString::from(id))
        .role(gpui::Role::Button)
        .aria_label(label)
        .focusable()
        .size(px(26.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Theme::CONTROL_RADIUS))
        .cursor_pointer()
        .hover(move |el| el.bg(hover_bg))
        .on_click(on_click)
        .child(
            crate::icons::icon(icon_path)
                .size(px(14.0))
                .text_color(icon_color),
        )
}

pub(super) struct ActionTooltip(pub gpui::SharedString);

impl Render for ActionTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .px_2()
            .py_1()
            .bg(theme.bg)
            .text_color(theme.text)
            .text_size(crate::typography::ui_rems(12.0))
            .child(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;

    #[gpui::test]
    fn deleting_the_last_pending_item_hides_a_paused_queue(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        cx.update(|_, cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| AppState::new());
        let composer = cx.new(|cx| Composer::new(state.clone(), cx));
        let queue: holt_proto::MessageQueue = serde_json::from_value(serde_json::json!({
            "pending": [{
                "messageId": "compact-1", "kind": "compact", "submittedAt": 0,
                "request": {
                    "prompt": "", "provider": "openai", "model": "openai/gpt-5.4",
                    "cwd": "/tmp"
                },
                "error": "There is nothing to compact"
            }],
            "paused": true
        }))
        .unwrap();
        state.update(cx, |state, _| state.message_queue = Some(queue));
        let height = std::rc::Rc::new(std::cell::Cell::new(px(0.0)));
        struct QueueView {
            composer: Entity<Composer>,
            height: std::rc::Rc<std::cell::Cell<gpui::Pixels>>,
        }
        impl Render for QueueView {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let height = self.height.clone();
                div()
                    .relative()
                    .w(px(600.0))
                    .child(
                        self.composer
                            .update(cx, |this, cx| this.render_message_queue(window, cx)),
                    )
                    .child(
                        gpui::canvas(
                            |_, _, _| (),
                            move |bounds, _, _, _| height.set(bounds.size.height),
                        )
                        .absolute()
                        .inset_0(),
                    )
            }
        }
        let view = cx.new(|_| QueueView {
            composer,
            height: height.clone(),
        });
        let draw_height = |cx: &mut gpui::VisualTestContext| {
            cx.draw(
                gpui::point(px(0.0), px(0.0)),
                gpui::size(px(600.0), px(400.0)),
                |_, _| view.clone().into_any_element(),
            );
        };
        draw_height(cx);
        assert!(height.get() > px(0.0));
        state.update(cx, |state, _| {
            state.message_queue.as_mut().unwrap().pending.clear()
        });
        draw_height(cx);
        assert_eq!(
            height.get(),
            px(0.0),
            "empty paused queue still occupies space"
        );
    }

    #[test]
    fn the_resume_control_shows_when_paused_with_work_waiting() {
        let queue = |paused: bool, prompts: &[&str]| -> holt_proto::MessageQueue {
            serde_json::from_value(serde_json::json!({
                "pending": prompts.iter().map(|p| serde_json::json!({
                    "messageId": format!("m-{p}"), "kind": "ordinary", "submittedAt": 0,
                    "request": {
                        "prompt": p, "provider": "openai", "model": "openai/gpt-5.4",
                        "cwd": "/tmp"
                    },
                })).collect::<Vec<_>>(),
                "paused": paused,
            }))
            .unwrap()
        };
        assert!(queue_resume_visible(&queue(true, &["a"])));
        assert!(
            !queue_resume_visible(&queue(false, &["a"])),
            "a running queue has nothing to resume"
        );
        assert!(
            !queue_resume_visible(&queue(true, &[])),
            "nothing parked: no control"
        );
    }

    #[test]
    fn queue_disclosure_motion_lands_exactly_on_its_target() {
        // Mirrors the sidebar disclosure test (shell/spaces.rs): once the
        // wall clock has passed the timeline + grace, `current()` snaps to
        // the target and `animating()` flips false — no leftover frames.
        let mut tween = QueueDisclosureMotion::new(1, 240.0, 0.0);
        tween.started = Instant::now() - motion::COLLAPSE.total().mul_f32(2.0);
        assert_eq!(tween.current(), 0.0);
        assert!(!tween.animating());
    }

    #[test]
    fn queue_disclosure_motion_reverses_without_a_snap() {
        // A second toggle mid-flight captures the current interpolated height
        // as the new `from`, so a half-expanded click that reverses lands on
        // the current visual height (no jump). The capture isn't bit-exact
        // because wall time advances between the two `current()` reads; a
        // sub-pixel tolerance is plenty for "no visible snap".
        let mut first = QueueDisclosureMotion::new(1, 0.0, 100.0);
        first.started = Instant::now() - motion::COLLAPSE.total().mul_f32(0.5);
        let mid = first.current();
        assert!(
            mid > 0.0 && mid < 100.0,
            "mid-flight value out of range: {mid}"
        );
        let previous = Some(first);
        let captured = previous
            .filter(|motion| motion.animating())
            .map(QueueDisclosureMotion::current)
            .unwrap_or(0.0);
        assert!(
            (captured - mid).abs() < 1.0,
            "reversal snap: {mid} vs {captured}"
        );
        // Critically: the captured height is still mid-flight (not at the
        // resting endpoint), so the reversal starts from where the user
        // sees the queue, not from 0 or 100.
        assert!(captured > 1.0 && captured < 99.0, "captured={captured}");
    }
}
