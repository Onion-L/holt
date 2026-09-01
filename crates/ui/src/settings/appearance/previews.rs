//! Theme preview miniatures: mode scenes, palette chips, and the bar skeleton they are painted from.

use super::*;

pub(super) fn bar(fraction: f32, tone: Hsla) -> gpui::Div {
    div()
        .h(px(5.0))
        .w(gpui::relative(fraction))
        .rounded(px(3.0))
        .bg(tone)
}

pub(super) fn miniature(theme: &Theme) -> AnyElement {
    let line = theme.text.opacity(0.22);
    let strong = theme.text.opacity(0.34);
    let r = px(widgets::OPTION_CARD_RADIUS);
    let root = div()
        .size_full()
        .flex()
        .flex_row()
        .bg(theme.surface)
        .rounded(r);
    root.child(
        div()
            .w(px(44.0))
            .h_full()
            .flex_none()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .px(px(8.0))
            .pt(px(14.0))
            .child(bar(0.70, strong))
            .child(bar(1.0, line))
            .child(bar(0.85, line))
            .child(bar(1.0, line)),
    )
    .child(
        div()
            .flex_1()
            .min_w_0()
            .my(px(8.0))
            .mr(px(8.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.bg)
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .p(px(10.0))
            .child(bar(0.62, strong))
            .child(bar(0.88, line))
            .child(bar(0.76, line))
            .child(bar(0.52, line)),
    )
    .into_any_element()
}

pub(super) fn scene_row(dot: Hsla, line: Hsla) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(4.0))
        .child(div().size(px(4.0)).flex_none().rounded_full().bg(dot))
        .child(div().h(px(4.0)).flex_1().rounded_full().bg(line))
}

/// A miniature app window painted in one theme: sidebar, content bars, a
/// small popup, and a bottom pill with the accent dot. Deterministic in the
/// theme alone so the System card can split two of them down the middle.
pub(super) fn mode_scene(theme: &Theme) -> AnyElement {
    let line = theme.text.opacity(0.20);
    let strong = theme.text.opacity(0.30);
    div()
        .size_full()
        .flex()
        .flex_row()
        .bg(theme.bg)
        .child(
            div()
                .w(gpui::relative(0.30))
                .h_full()
                .flex_none()
                .bg(theme.surface)
                .border_r_1()
                .border_color(theme.border)
                .flex()
                .flex_col()
                .gap(px(7.0))
                .px(px(7.0))
                .pt(px(10.0))
                .child(
                    div()
                        .h(px(10.0))
                        .w(gpui::relative(0.75))
                        .rounded_full()
                        .border_1()
                        .border_color(theme.border),
                )
                .child(bar(0.85, strong))
                .child(bar(1.0, line))
                .child(bar(0.90, line)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .h_full()
                .relative()
                .flex()
                .flex_col()
                .gap(px(7.0))
                .p(px(9.0))
                .child(
                    div().flex().flex_row().justify_end().child(
                        div()
                            .h(px(9.0))
                            .w(gpui::relative(0.30))
                            .rounded_full()
                            .bg(strong),
                    ),
                )
                .child(bar(0.45, strong))
                .child(bar(0.62, line))
                .child(bar(0.52, line))
                .child(
                    div()
                        .absolute()
                        .top(px(18.0))
                        .right(px(9.0))
                        .w(px(52.0))
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.surface_raised)
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .p(px(6.0))
                        .child(scene_row(theme.success, strong))
                        .child(scene_row(theme.accent, line))
                        .child(scene_row(theme.warning, line)),
                )
                .child(
                    div()
                        .mt_auto()
                        .h(px(15.0))
                        .w_full()
                        .flex_none()
                        .rounded_full()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.surface)
                        .flex()
                        .items_center()
                        .px(px(5.0))
                        .child(
                            div()
                                .h(px(4.0))
                                .w(gpui::relative(0.38))
                                .rounded_full()
                                .bg(line),
                        )
                        .child(
                            div()
                                .ml_auto()
                                .size(px(9.0))
                                .flex_none()
                                .rounded_full()
                                .bg(theme.accent),
                        ),
                ),
        )
        .into_any_element()
}

/// The mode card's preview: a [`mode_scene`] in the matching appearance. For
/// System the dark scene is laid out at full width but clipped to the right
/// half, so the split reads as one continuous window.
pub(super) fn mode_preview(mode: AppearanceMode, light: &Theme, dark: &Theme) -> AnyElement {
    match mode {
        AppearanceMode::Light => mode_scene(light),
        AppearanceMode::Dark => mode_scene(dark),
        AppearanceMode::System => div()
            .size_full()
            .relative()
            .child(mode_scene(light))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right_0()
                    .w_1_2()
                    .overflow_hidden()
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .right_0()
                            .w(gpui::relative(2.0))
                            .child(mode_scene(dark)),
                    ),
            )
            .into_any_element(),
    }
}

pub(super) fn palette_preview(theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .w(px(30.0))
        .h(px(18.0))
        .rounded(px(5.0))
        .overflow_hidden()
        .border_1()
        .border_color(theme.border)
        .flex()
        .child(div().w_1_3().h_full().bg(theme.surface))
        .child(div().w_1_3().h_full().bg(theme.bg))
        .child(div().w_1_3().h_full().bg(theme.accent))
}
