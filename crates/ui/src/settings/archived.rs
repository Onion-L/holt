//! Settings → Archived (feature-inventory §1.5): archived chats, with
//! Unarchive (Mutate setChatArchived false).

use gpui::{
    AnyElement, Context, Entity, SharedString, Subscription, Task, Window, div, prelude::*, px,
};

use holt_proto::Chat;
use holt_rpc::methods;

use crate::state::AppState;
use crate::theme::Theme;

/// Archived rows in sidebar (recency) order. Pure.
pub fn archived_chats(chats: &[Chat]) -> Vec<&Chat> {
    chats.iter().filter(|c| c.archived).collect()
}

pub struct ArchivedPage {
    state: Entity<AppState>,
    error: Option<SharedString>,
    /// Chat with an in-flight unarchive (button shows working state).
    busy: Option<String>,
    /// A Clear-all pass is running (`clear_task` owns the loop; kept apart
    /// from `task` — assigning over a Task cancels it, and an Unarchive
    /// click must not kill a clear mid-batch).
    clearing: bool,
    /// The Clear-all confirm dialog is up.
    confirm_clear: bool,
    /// Row index under the pointer — drives the original's `group-hover`
    /// Unarchive reveal (`opacity-0 group-hover:opacity-100`).
    hovered: Option<usize>,
    task: Option<Task<()>>,
    clear_task: Option<Task<()>>,
    _observe: Subscription,
}

impl ArchivedPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            state,
            error: None,
            busy: None,
            clearing: false,
            confirm_clear: false,
            hovered: None,
            task: None,
            clear_task: None,
            _observe: observe,
        }
    }

    fn unarchive(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.busy = Some(chat_id.clone());
        self.error = None;
        let params = serde_json::json!({
            "op": "setChatArchived",
            "chatId": chat_id,
            "archived": false,
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, params).await;
            this.update(cx, |page, cx| {
                page.busy = None;
                if let Err(err) = result {
                    page.error = Some(format!("Unarchive failed: {err}").into());
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Delete every archived chat, one Mutate each, stopping at the first
    /// failure. The watch frames shrink the list as the engine confirms.
    fn clear_all(&mut self, chat_ids: Vec<String>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.confirm_clear = false;
        self.clearing = true;
        self.error = None;
        self.clear_task = Some(cx.spawn(async move |this, cx| {
            let mut failure = None;
            for chat_id in chat_ids {
                let params = serde_json::json!({ "op": "deleteChat", "chatId": chat_id });
                if let Err(err) = engine.client().call(methods::MUTATE, params).await {
                    failure = Some(err.to_string());
                    break;
                }
            }
            this.update(cx, |page, cx| {
                page.clearing = false;
                if let Some(err) = failure {
                    page.error = Some(format!("Clear failed: {err}").into());
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

impl Render for ArchivedPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        let now = chrono::Utc::now();
        let rows: Vec<Chat> = {
            let state = self.state.read(cx);
            archived_chats(&state.chats).into_iter().cloned().collect()
        };
        let busy = self.busy.clone();
        let count = rows.len();

        let items: Vec<AnyElement> = rows
            .into_iter()
            .enumerate()
            .map(|(ix, chat)| {
                let title: SharedString = chat
                    .title
                    .clone()
                    .unwrap_or_else(|| "Untitled session".into())
                    .into();
                let time_ago: SharedString = crate::state::format_time_ago(
                    chat.last_message_at.unwrap_or(chat.created_at),
                    now,
                )
                .into();
                let location: Option<SharedString> =
                    crate::state::chat_location(&chat).map(Into::into);
                let is_busy = busy.as_deref() == Some(chat.id.as_str());
                let row_hovered = self.hovered == Some(ix);
                let chat_id = chat.id.clone();
                // holt settings.archived.tsx row: archive tile, medium title
                // + tabular time, quiet location meta, Unarchive.
                div()
                    .id(("archived-row", ix))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .rounded(px(8.0))
                    .px(px(12.0))
                    .py(px(8.0))
                    .hover(|s| s.bg(crate::theme::ink(0.03)))
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered {
                            this.hovered = Some(ix);
                        } else if this.hovered == Some(ix) {
                            this.hovered = None;
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_none()
                            .size(px(32.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                crate::icons::icon(crate::icons::ARCHIVE_MINIMALISTIC)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted.opacity(0.6)),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(crate::typography::ui_rems(13.0))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(theme.text)
                                            .child(title),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(crate::typography::ui_rems(11.0))
                                            .text_color(theme.text_muted.opacity(0.5))
                                            .child(time_ago),
                                    ),
                            )
                            .child({
                                // location at the line's own tone (holt: a
                                // plain span inheriting
                                // `text-muted-foreground/55`).
                                div()
                                    .mt(px(2.0))
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(6.0))
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(theme.text_muted.opacity(0.55))
                                    .when_some(location, |meta, location| {
                                        meta.child(div().min_w_0().truncate().child(location))
                                    })
                            }),
                    )
                    .child(
                        // Hidden until the row is hovered (holt `opacity-0
                        // group-hover:opacity-100`); hover fill is the solid
                        // accent tone (`hover:bg-accent`).
                        div()
                            .id(("unarchive", ix))
                            .flex_none()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .px(px(10.0))
                            .py(px(4.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border)
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .opacity(if row_hovered || is_busy { 1.0 } else { 0.0 })
                            .when(is_busy, |el| el.opacity(0.4))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.surface_raised).text_color(theme.text))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.unarchive(chat_id.clone(), cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::ARCHIVE_UP_MINIMALISTIC)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from(if is_busy {
                                "Unarchiving…"
                            } else {
                                "Unarchive"
                            })),
                    )
                    .into_any_element()
            })
            .collect();

        let body: AnyElement = if items.is_empty() {
            // Centered empty state (holt settings.archived.tsx).
            div()
                .mt(px(96.0))
                .flex()
                .flex_col()
                .items_center()
                .text_center()
                .text_color(theme.text_muted.opacity(0.5))
                .child(
                    // `opacity-40` on top of the inherited muted/50 — an
                    // effectively ~20% glyph (holt settings.archived.tsx).
                    crate::icons::icon(crate::icons::ARCHIVE_MINIMALISTIC)
                        .size(px(28.0))
                        .text_color(theme.text_muted.opacity(0.2)),
                )
                .child(
                    div()
                        .mt(px(12.0))
                        .text_size(crate::typography::ui_rems(14.0))
                        .child(SharedString::from("Nothing archived")),
                )
                .child(
                    div()
                        .mt(px(4.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted.opacity(0.4))
                        .child(SharedString::from(
                            "Right-click a session in the sidebar to archive it.",
                        )),
                )
                .into_any_element()
        } else {
            div()
                .mt(px(24.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .children(items)
                .into_any_element()
        };

        // Clear-all lives on the headline row, right-anchored, and only when
        // there is something to clear (holt settings.archived.tsx has no
        // counterpart — this page is the only surface archived sessions have).
        let clear_button = div()
            .id("clear-archived")
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.danger.opacity(0.25))
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.danger_muted)
            .cursor_pointer()
            .hover(|s| s.bg(theme.danger.opacity(0.08)).text_color(theme.danger))
            .when(self.clearing, |el| el.opacity(0.4))
            .on_click(cx.listener(|this, _, _, cx| {
                if this.clearing {
                    return;
                }
                this.confirm_clear = true;
                cx.notify();
            }))
            .child(
                crate::icons::icon(crate::icons::TRASH_BIN_MINIMALISTIC)
                    .size(px(14.0))
                    .text_color(theme.danger_muted),
            )
            .child(SharedString::from(if self.clearing {
                "Clearing…"
            } else {
                "Clear all"
            }));

        // The confirm mirrors the sidebar's delete-session dialog: destructive,
        // permanent, counted.
        let confirm_dialog = self.confirm_clear.then(|| {
            let copy = if count == 1 {
                "1 archived session will be permanently deleted. This can\u{2019}t be undone."
                    .to_string()
            } else {
                format!(
                    "{count} archived sessions will be permanently deleted. This can\u{2019}t be undone."
                )
            };
            let card = crate::popover::dialog_card(&theme)
                .child(crate::popover::dialog_title(
                    &theme,
                    "Clear archived sessions?",
                ))
                .child(
                    div().mt(px(6.0)).child(crate::popover::dialog_body(
                        &theme,
                        copy,
                    )),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            crate::popover::btn_ghost(&theme, "Cancel", "clear-archived-cancel")
                                .id("clear-archived-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.confirm_clear = false;
                                    cx.notify();
                                })),
                        )
                        .child(
                            crate::popover::btn_danger(&theme, "Clear all")
                                .id("clear-archived-confirm")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let ids: Vec<String> = this
                                        .state
                                        .read(cx)
                                        .chats
                                        .iter()
                                        .filter(|chat| chat.archived)
                                        .map(|chat| chat.id.clone())
                                        .collect();
                                    this.clear_all(ids, cx);
                                })),
                        ),
                )
                .into_any_element();
            crate::popover::modal("clear-archived-dialog", window.viewport_size(), card)
        });

        div()
            .id("archived-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap(px(16.0))
                            .child(widgets::page_header(
                                &theme,
                                "Archived sessions",
                                (count > 0).then_some(count),
                            ))
                            .when(count > 0, |el| el.child(clear_button)),
                    )
                    .child(widgets::page_subtitle(
                        &theme,
                        "Hidden from the sidebar until you unarchive them. Clearing permanently deletes every session here.",
                    ))
                    .when_some(self.error.clone(), |el, message| {
                        el.child(
                            widgets::error_strip(&theme, message)
                                .id("archived-error")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .child(body)
                    .children(confirm_dialog),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn chat(id: &str, archived: bool) -> Chat {
        Chat {
            id: id.into(),
            device_id: "d".into(),
            title: None,
            archived,
            cwd: None,
            branch: None,
            checkout_id: None,
            source_context: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            space_id: None,
            last_seen_at: None,
            room_gen: None,
        }
    }

    #[test]
    fn only_archived_rows_show() {
        let chats = vec![chat("a", false), chat("b", true), chat("c", true)];
        let rows = archived_chats(&chats);
        let ids: Vec<&str> = rows.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["b", "c"]);
    }
}
