//! Presentation and commands for the selected chat's engine-owned queue.

use gpui::{Context, KeyDownEvent, Render, Window, div, prelude::*, px};
use holt_rpc::methods;

use super::Composer;
use crate::theme::Theme;

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

    pub(super) fn render_message_queue(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let state = self.state.read(cx);
        let Some(queue) = state.message_queue.as_ref() else {
            return div().into_any_element();
        };
        if queue.pending.is_empty() && !queue.paused && queue.error.is_none() {
            return div().into_any_element();
        }
        let theme = Theme::of(cx);
        let pending = queue.pending.clone();
        let count = pending.len();
        let error = queue
            .error
            .clone()
            .or_else(|| pending.first().and_then(|item| item.error.clone()));
        let header = if queue.paused {
            format!("Queue paused ({count})")
        } else {
            format!("Queued ({count})")
        };
        div()
            .id("message-queue")
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .px_2()
            .py_1()
            .text_size(crate::typography::ui_rems(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(div().text_color(theme.text_muted).child(header))
                    .when(queue.paused, |el| {
                        el.child(
                            div()
                                .id("continue-message-queue")
                                .role(gpui::Role::Button)
                                .aria_label("Continue message queue")
                                .focusable()
                                .px_2()
                                .py_1()
                                .rounded(px(Theme::CONTROL_RADIUS))
                                .border_1()
                                .border_color(theme.border)
                                .text_color(theme.text)
                                .hover(|el| el.bg(theme.glass_hover()))
                                .focus(|el| el.border_color(theme.border_strong))
                                .on_click(cx.listener(|this, _, _, cx| this.continue_queue(cx)))
                                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                        cx.stop_propagation();
                                        this.continue_queue(cx);
                                    }
                                }))
                                .child("Continue"),
                        )
                    }),
            )
            .when(count > 0, |el| {
                el.child(
                    gpui::uniform_list("pending-messages", count, move |range, _, cx| {
                        let theme = Theme::of(cx);
                        range
                            .map(|ix| {
                                let item = &pending[ix];
                                div()
                                    .id(gpui::SharedString::from(item.message_id.clone()))
                                    .h(px(40.0))
                                    .w_full()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .justify_center()
                                    .border_b_1()
                                    .border_color(theme.border)
                                    .child(
                                        div()
                                            .truncate()
                                            .text_color(theme.text)
                                            .child(item.request.prompt.clone()),
                                    )
                                    .child(
                                        div()
                                            .truncate()
                                            .text_color(theme.text_faint)
                                            .child(item.request.model.clone()),
                                    )
                            })
                            .collect::<Vec<_>>()
                    })
                    .h(px(40.0 * count.min(4) as f32))
                    .w_full()
                    .min_w_0()
                    .occlude(),
                )
            })
            .when_some(error, |el, error| {
                el.child(div().min_w_0().text_color(theme.danger).child(error))
            })
            .into_any_element()
    }

    fn continue_queue(&mut self, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let (Some(engine), Some(chat_id)) = (state.engine().cloned(), state.selected_chat.clone())
        else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Err(error) = engine
                .client()
                .call(
                    methods::CONTINUE_MESSAGE_QUEUE,
                    serde_json::json!({"chatId": chat_id}),
                )
                .await
            {
                this.update(cx, |this, cx| {
                    this.failure = Some(format!("Could not continue the queue: {error}").into());
                    this.failure_key = Some(chat_id);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }
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
