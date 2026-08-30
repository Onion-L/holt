//! Settings → Notifications: the session ping toggles — the completion/
//! question chime and the desktop banner ride the same status transitions
//! (`shell::on_state_changed`); this page flips their two `UiSettings` flags.
//!
//! The ShortcutsPage arrangement: the page holds a working copy, every flip
//! emits [`NotificationsEvent::Changed`], and the shell persists it. Nothing
//! here talks RPC — both flags are device-local UI settings.

use gpui::{App, ClickEvent, Context, EventEmitter, Window, div, prelude::*, px};

use crate::settings::widgets;
use crate::theme::Theme;

#[derive(Debug, Clone)]
pub enum NotificationsEvent {
    /// A toggle flipped — persist all three flags.
    Changed {
        sound: bool,
        desktop: bool,
        background_only: bool,
    },
}

pub struct NotificationsPage {
    sound: bool,
    desktop: bool,
    background_only: bool,
}

impl EventEmitter<NotificationsEvent> for NotificationsPage {}

impl NotificationsPage {
    pub fn new(sound: bool, desktop: bool, background_only: bool, _cx: &mut Context<Self>) -> Self {
        Self {
            sound,
            desktop,
            background_only,
        }
    }

    fn emit(&self, cx: &mut Context<Self>) {
        cx.emit(NotificationsEvent::Changed {
            sound: self.sound,
            desktop: self.desktop,
            background_only: self.background_only,
        });
    }
}

impl Render for NotificationsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let sound = self.sound;
        let desktop = self.desktop;
        let background_only = self.background_only;

        div()
            .id("notifications-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Notifications", None))
                    .child(
                        widgets::page_subtitle(
                            &theme,
                            "Choose how Holt pings you when a session needs attention.",
                        )
                        .max_w(px(640.0))
                        .line_height(px(20.0)),
                    )
                    .child(
                        div()
                            .mt(px(28.0))
                            .flex()
                            .flex_col()
                            .gap(px(10.0))
                            .child(widgets::field_label(&theme, "Delivery"))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(notification_row(
                                        &theme,
                                        "Sounds",
                                        "Chime when a run finishes or an agent asks a question.",
                                        sound,
                                        "notifications-sound-toggle",
                                        true,
                                        cx.listener(move |this, _, _, cx| {
                                            this.sound = !this.sound;
                                            this.emit(cx);
                                            cx.notify();
                                        }),
                                    ))
                                    .child(notification_row(
                                        &theme,
                                        "Desktop notifications",
                                        "Show a system banner when a session needs attention.",
                                        desktop,
                                        "notifications-desktop-toggle",
                                        true,
                                        cx.listener(move |this, _, _, cx| {
                                            this.desktop = !this.desktop;
                                            this.emit(cx);
                                            cx.notify();
                                        }),
                                    ))
                                    .child(
                                        notification_row(
                                            &theme,
                                            "Only when in the background",
                                            "Skip the banner while a Holt window is focused.",
                                            background_only,
                                            "notifications-background-toggle",
                                            desktop,
                                            cx.listener(move |this, _, _, cx| {
                                                this.background_only = !this.background_only;
                                                this.emit(cx);
                                                cx.notify();
                                            }),
                                        )
                                        .when(!desktop, |el| el.opacity(0.5)),
                                    ),
                            ),
                    ),
            )
    }
}

fn notification_row(
    theme: &Theme,
    title: &'static str,
    description: &'static str,
    on: bool,
    id: &'static str,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    widgets::flat_row()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(widgets::row_title(theme, title))
                .child(widgets::row_description(theme, description)),
        )
        .child(
            widgets::toggle_switch(theme, on)
                .id(id)
                .when(enabled, |el| el.cursor_pointer().on_click(on_click)),
        )
}
