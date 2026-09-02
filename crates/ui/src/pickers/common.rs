//! Shared picker chrome: the trigger/footer chip styles, popover frames,
//! the shared search box, retry rows, the model-list scrollbar interactions,
//! and the overlay attachment helpers.

use gpui::{AnyElement, Context, KeyDownEvent, SharedString, div, prelude::*, px};

use crate::motion;
use crate::popover::{self, Loadable};
use crate::theme::Theme;

use super::PickerKind;
use super::Pickers;

impl Pickers {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn trigger_chip(
        &self,
        kind: PickerKind,
        label: SharedString,
        set: bool,
        chip_icon: Option<(&'static str, Option<gpui::Hsla>)>,
        // The chip never collapses while identity resolves (user report):
        // `icon_loading` swaps the brand slot for the pixel-glyph loader
        // (provider unknown), `label_loading` swaps the text for a ghost bar
        // (model unknown).
        icon_loading: bool,
        label_loading: bool,
        suffix: Option<(SharedString, Option<gpui::Hsla>)>,
        configure_provider: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id: &'static str = match kind {
            PickerKind::Branch => "picker-branch",
            PickerKind::Checkout => "picker-checkout",
            PickerKind::ProviderModel => "picker-model",
            PickerKind::Space => "picker-space",
        };
        let open = self.open_kind() == Some(kind);
        // Ghost pill (holt composer/styles.tsx `pill`): `h-8 rounded-lg px-2.5
        // gap-1.5 text-[12px] font-medium text-muted-foreground`, icons size-4,
        // hover/open wash — no border, no caret; the actions row stays quiet.
        div()
            .id(id)
            .h(px(32.0))
            .max_w(px(248.0))
            // Shrinkable under row pressure — four footer chips share one
            // line; without min_w_0 they overflowed and painted overlapped.
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .rounded(px(8.0))
            .text_size(crate::typography::ui_rems(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            // holt composer/styles.tsx `pill`: `transition-colors` — the wash
            // and text brighten fade over 150ms.
            .text_color(motion::hover_blend(
                id,
                if set {
                    theme.text.opacity(0.9)
                } else {
                    theme.text_muted
                },
                theme.text,
            ))
            .bg(if open {
                theme.element_hover
            } else {
                motion::hover_blend(id, gpui::transparent_black(), theme.element_hover)
            })
            .on_hover(motion::hover_listener(id))
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.open.note_trigger_press_matching(|open| *open == kind)
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                if configure_provider {
                    window.dispatch_action(Box::new(crate::shell::OpenSettings), cx);
                } else {
                    this.toggle(kind, window, cx);
                }
            }))
            .when(icon_loading, |el| {
                el.child(div().flex_none().child(crate::loaders::mini_glyph_spinner(
                    "picker-chip-loader",
                    2.0,
                    theme.glyph,
                    cx.entity_id(),
                    cx,
                )))
            })
            .when_some(
                (!icon_loading).then_some(chip_icon).flatten(),
                |el, (path, tint)| {
                    el.child(
                        crate::icons::icon(path)
                            .size(px(16.0))
                            .text_color(tint.unwrap_or(theme.text_muted)),
                    )
                },
            )
            .when(label_loading, |el| {
                el.child(popover::skeleton_bar(56.0, cx.entity_id(), cx))
            })
            .when(!label_loading, |el| {
                el.child(div().min_w_0().truncate().child(label))
            })
            // The effort half of the combined model+effort chip: muted, no
            // icon — one button, two tones. `tint` overrides the muted tone.
            .when_some(suffix, |el, (suffix, tint)| {
                el.child(
                    div()
                        .flex_none()
                        .text_color(tint.unwrap_or(theme.text_muted.opacity(0.7)))
                        .child(suffix),
                )
            })
    }

    /// A footer-row trigger (t3code ghost `Button size="xs"`): leading icon,
    /// truncating label, trailing chevron — smaller and quieter than the
    /// in-pill chips.
    pub(super) fn footer_chip(
        &self,
        kind: PickerKind,
        id: &'static str,
        icon_path: &'static str,
        label: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let open = self.open_kind() == Some(kind);
        div()
            .id(id)
            .h(px(20.0))
            .max_w(px(280.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .text_size(crate::typography::ui_rems(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(motion::hover_blend(
                id,
                theme.text_muted.opacity(0.7),
                theme.text.opacity(0.8),
            ))
            .bg(if open {
                theme.element_hover
            } else {
                motion::hover_blend(id, gpui::transparent_black(), theme.element_hover)
            })
            .on_hover(motion::hover_listener(id))
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.open.note_trigger_press_matching(|open| *open == kind)
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.toggle(kind, window, cx)))
            .child(
                crate::icons::icon(icon_path)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(div().min_w_0().truncate().child(label))
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.5)),
            )
    }

    /// A read-only footer label (locked sessions — t3code's
    /// `resolveLockedWorkspaceLabel` span).
    pub(super) fn footer_label(
        icon_path: &'static str,
        label: SharedString,
        theme: &Theme,
    ) -> gpui::Div {
        div()
            .h(px(20.0))
            // Two of these share one row (checkout, ref): cap each early and
            // let them SHRINK (`min_w_0`) — without it the clusters
            // overflowed into each other and the labels painted overlapped
            // (user report).
            .max_w(px(160.0))
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .text_size(crate::typography::ui_rems(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted.opacity(0.6))
            .child(
                crate::icons::icon(icon_path)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.6)),
            )
            .child(div().min_w_0().truncate().child(label))
    }

    pub(super) fn popover_frame(
        &self,
        width: f32,
        content: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        popover::popover_card(&theme)
            .w(px(width))
            // holt caps its tallest picker at min(640px, 75vh).
            .max_h(px(640.0))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }

    /// [`Self::popover_frame`] without the p-1 inset — the provider/model
    /// picker's rail + list panes bleed to the card edge (holt
    /// provider-model-picker.tsx `className="w-80 p-0"`).
    pub(super) fn popover_frame_flush(
        &self,
        width: f32,
        content: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        popover::popover_card_flush(&theme)
            .w(px(width))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }

    pub(super) fn search_box(&self, theme: &Theme) -> AnyElement {
        popover::search_input_frame(theme, self.search.clone().into_any_element())
            .into_any_element()
    }

    pub(super) fn retry_row(
        &self,
        id: &'static str,
        message: &str,
        kind: PickerKind,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        popover::error_row(theme, message)
            .child(
                div()
                    .id(id)
                    .px(px(Theme::SPACE_SM))
                    .py(px(3.0))
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .border_1()
                    .border_color(theme.border)
                    .text_color(theme.text)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| match kind {
                        PickerKind::Branch | PickerKind::Checkout => this.ensure_refs(true, cx),
                        PickerKind::ProviderModel => {
                            this.providers = Loadable::Idle;
                            this.models.clear();
                            this.catalog_rev += 1;
                            this.ensure_providers(false, cx);
                        }
                        // Projects load nothing; no retry surface exists.
                        PickerKind::Space => {}
                    }))
                    .child(SharedString::from("Retry")),
            )
            .into_any_element()
    }

    /// The virtualized list's plain scroll handle (bounds/offset for the
    /// floating scrollbar; `UniformList` tracks it internally).
    pub(super) fn model_scroll_base(&self) -> gpui::ScrollHandle {
        self.model_scroll.0.borrow().base_handle.clone()
    }

    pub(super) fn on_model_list_hover(
        &mut self,
        hovered: &bool,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if self.model_bar.set_list_hovered(*hovered) {
            cx.notify();
        }
    }

    fn on_model_scrollbar_hover(
        &mut self,
        hovered: &bool,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if self.model_bar.set_bar_hovered(*hovered) {
            cx.notify();
        }
    }

    fn on_model_scrollbar_mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let scroll = self.model_scroll_base();
        if !self.model_bar.begin_press(&scroll, event.position.y) {
            return;
        }
        window.focus(&self.focus, cx);
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn on_model_scrollbar_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<popover::MenuScrollbarDrag>,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let scroll = self.model_scroll_base();
        if self.model_bar.drag_to(&scroll, event.event.position.y) {
            cx.notify();
        }
    }

    fn on_model_scrollbar_mouse_up(
        &mut self,
        _event: &gpui::MouseUpEvent,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        self.model_bar.end_press();
        cx.notify();
    }

    pub(super) fn render_model_scrollbar(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let metrics = self.model_bar.metrics(&self.model_scroll_base())?;
        Some(
            self.model_bar
                .render_rail(theme, metrics)?
                .id("model-scrollbar")
                .on_hover(cx.listener(Self::on_model_scrollbar_hover))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(Self::on_model_scrollbar_mouse_down),
                )
                .on_drag(popover::MenuScrollbarDrag, |_, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| popover::MenuScrollbarDragGhost)
                })
                .on_mouse_up_out(
                    gpui::MouseButton::Left,
                    cx.listener(Self::on_model_scrollbar_mouse_up),
                )
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(Self::on_model_scrollbar_mouse_up),
                )
                .into_any_element(),
        )
    }
}

/// Attach the (single) open popover overlay to its trigger chip.
pub(super) fn attach_overlay(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
    closing: Option<std::time::Instant>,
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip.child(popover::anchored_menu_above(id, element, closing));
    }
    chip
}

/// [`attach_overlay`] with the menu RIGHT-ALIGNED to the trigger (t3code
/// `align="end"` — right-edge triggers like the ref picker open leftward).
pub(super) fn attach_overlay_end(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
    closing: Option<std::time::Instant>,
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip
            .relative()
            .child(popover::anchored_menu_above_end(id, element, closing));
    }
    chip
}
