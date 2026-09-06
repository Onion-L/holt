//! Presentation and commands for the selected chat's engine-owned queue.

use gpui::{
    Context, Entity, Focusable, KeyDownEvent, Render, Subscription, Window, div, prelude::*, px,
};
use holt_rpc::methods;

use super::{Composer, ComposerInput, ComposerInputEvent};
use crate::theme::Theme;

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
        if queue.pending.is_empty() && !queue.paused && queue.error.is_none() {
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
                    .child(
                        crate::icons::icon(if expanded {
                            crate::icons::ALT_ARROW_DOWN
                        } else {
                            crate::icons::ALT_ARROW_RIGHT
                        })
                        .size(px(14.0))
                        .text_color(theme.text_muted),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.queue_expanded = !this.queue_expanded;
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            cx.stop_propagation();
                            this.queue_expanded = !this.queue_expanded;
                            cx.notify();
                        }
                    })),
            )
            .when(expanded && count > 0, |el| {
                el.child(
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
                                    .when(!editing, |el| {
                                        el.hover(|el| el.bg(theme.glass_hover()))
                                    })
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
                                                    el.child(
                                                        div().w_full().min_w_0().child(input),
                                                    )
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
                    .h(px(30.0 * count.min(4) as f32 + 8.0))
                    .w_full()
                    .min_w_0()
                    .occlude(),
                )
            })
            .when(expanded, |el| {
                el.when_some(error, |el, error| {
                    el.child(div().min_w_0().text_color(theme.danger).child(error))
                })
            })
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

pub(super) struct ActionTooltip(pub &'static str);

impl Render for ActionTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .px_2()
            .py_1()
            .bg(theme.bg)
            .text_color(theme.text)
            .text_size(crate::typography::ui_rems(12.0))
            .child(self.0)
    }
}
