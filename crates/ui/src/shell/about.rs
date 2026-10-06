//! About Holt: the app-menu dialog — logo, name, version — styled like the
//! update dialog (`popover::dialog_card` + `popover::modal`), so it follows
//! the theme instead of the platform's alert chrome.

use super::*;

impl Shell {
    pub(super) fn render_about_dialog(
        &mut self,
        theme: &Theme,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.about_dialog {
            return None;
        }
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(
                img(crate::app_icon::logo_image())
                    .flex_none()
                    .size(px(44.0))
                    .rounded(px(11.0))
                    .object_fit(gpui::ObjectFit::Contain),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(popover::dialog_title(theme, "Holt"))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(12.0))
                            .font_family(theme.font_mono.clone())
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!(
                                "Version {}",
                                env!("CARGO_PKG_VERSION")
                            ))),
                    ),
            );
        let footer = div().flex().flex_row().justify_end().child(
            popover::btn_primary(theme, "OK")
                .id("about-ok")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.about_dialog = false;
                    cx.notify();
                })),
        );
        let card = popover::dialog_card(theme)
            .debug_selector(|| "about-dialog".into())
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.about_dialog = false;
                cx.notify();
            }))
            .child(header)
            .child(div().mt(px(20.0)).child(footer))
            .into_any_element();
        Some(popover::modal("about-dialog", viewport, card))
    }
}
