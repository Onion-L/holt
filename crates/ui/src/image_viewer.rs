//! The shared image viewer: a modal lightbox that opens from the composer's
//! staged chips and the Transcript's image references alike. One implementation
//! for both surfaces — fit-to-window first, then zoom (buttons, wheel,
//! trackpad pinch) and drag-to-pan over the ORIGINAL decode, never a
//! thumbnail.
//!
//! Geometry (fit/zoom/pan math) lives in pure functions at the bottom so the
//! state machine is testable without a window. Event isolation is the
//! modal's own `occlude()` (ADR-0013): wheel and pinch inside the viewer
//! never reach the Transcript list beneath. Clicking the image does nothing;
//! the close control, the empty backdrop, and Escape dismiss it, and the
//! owner restores focus on the `Closed` event.

use std::ops::Not as _;

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, MouseButton, Pixels, Render,
    ScrollDelta, ScrollWheelEvent, SharedString, Size, Window, anchored, deferred, div, img, point,
    prelude::*, px, size,
};

use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

/// Distance the image area keeps from the viewport edges, and extra room the
/// bottom toolbar reserves from the fit computation.
const VIEW_MARGIN: f32 = 24.0;
const TOOLBAR_SPACE: f32 = 64.0;
/// Zoom bounds: never smaller than fit (computed per frame), never past 16×.
const MAX_SCALE: f32 = 16.0;
/// Button zoom step (wheel zoom is continuous).
const ZOOM_STEP: f32 = 1.25;
/// A drag must exceed this many px before a press counts as a pan rather
/// than a click.
const DRAG_SLOP: f32 = 4.0;
/// While panning, at least this much of the image stays inside the viewport.
const TOUCH_PX: f32 = 48.0;

/// One image the viewer can navigate among, scoped to the originating
/// message or draft.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewerTarget {
    pub path: SharedString,
    pub label: SharedString,
}

pub enum ImageViewerEvent {
    /// The user dismissed the viewer (button, backdrop, or Escape) — the
    /// owner clears its field and restores focus.
    Closed,
}

enum LoadState {
    Loading,
    Ready(crate::images::ViewerPixels),
    Failed(SharedString),
}

#[derive(Clone, Copy)]
struct DragPan {
    start: gpui::Point<Pixels>,
    origin: gpui::Point<Pixels>,
}

/// The open viewer. Owned as an entity by whoever opened it (the composer or
/// the transcript); it renders itself as the top-most modal layer.
pub struct ImageViewer {
    state: Entity<AppState>,
    targets: Vec<ViewerTarget>,
    index: usize,
    /// Manual zoom scale; `None` = fit-to-window (recomputed per frame, so a
    /// window resize keeps both image and controls reachable).
    zoom: Option<f32>,
    /// Offset of the image center from the viewport center.
    pan: gpui::Point<Pixels>,
    drag: Option<DragPan>,
    /// Whether the in-flight press moved past [`DRAG_SLOP`] (click vs pan).
    drag_moved: bool,
    load: LoadState,
    /// Bumped on every image change; late loads check it before applying so
    /// a slow decode can never replace the image the user navigated to.
    load_gen: u64,
    source_fingerprint: Option<(u64, u128)>,
    load_task: Option<gpui::Task<()>>,
    watch_task: Option<gpui::Task<()>>,
    focus: FocusHandle,
}

impl EventEmitter<ImageViewerEvent> for ImageViewer {}
impl Focusable for ImageViewer {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ImageViewer {
    /// Open on `targets[index]`, loading at a usable fit.
    pub fn open(
        state: Entity<AppState>,
        targets: Vec<ViewerTarget>,
        index: usize,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut viewer = Self {
            state,
            targets,
            index: 0,
            zoom: None,
            pan: point(px(0.0), px(0.0)),
            drag: None,
            drag_moved: false,
            load: LoadState::Loading,
            load_gen: 0,
            source_fingerprint: None,
            load_task: None,
            watch_task: None,
            focus: cx.focus_handle(),
        };
        viewer.select_index(index, cx);
        cx.on_release(|viewer, _cx| {
            if let LoadState::Ready(pixels) = &viewer.load {
                crate::images::discard(pixels.pixels.clone());
            }
        })
        .detach();
        viewer.watch_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                if this
                    .update(cx, |viewer, cx| {
                        if crate::images::fingerprint(&viewer.target().path)
                            != viewer.source_fingerprint
                        {
                            viewer.load_current(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
        viewer
    }

    fn engine(&self, cx: &App) -> Option<EngineHandle> {
        self.state.read(cx).engine().cloned()
    }

    pub fn target(&self) -> &ViewerTarget {
        &self.targets[self.index]
    }

    /// Load the target at `self.index` (open, navigate, or retry): reset to
    /// fit, center, and start a fresh bounded load. Late loads of a previous
    /// image are dropped by the generation check.
    fn load_current(&mut self, cx: &mut Context<Self>) {
        if let LoadState::Ready(pixels) = &self.load {
            crate::images::discard(pixels.pixels.clone());
        }
        self.zoom = None;
        self.pan = point(px(0.0), px(0.0));
        self.drag = None;
        self.load_gen += 1;
        let load_gen = self.load_gen;
        let Some(engine) = self.engine(cx) else {
            self.load = LoadState::Failed("Engine not connected.".into());
            cx.notify();
            return;
        };
        let path = self.target().path.clone();
        self.source_fingerprint = crate::images::fingerprint(&path);
        self.load = LoadState::Loading;
        cx.notify();
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result =
                crate::images::load_viewer_pixels(&engine, &path, cx.background_executor()).await;
            this.update(cx, |viewer, cx| {
                if viewer.load_gen != load_gen {
                    return; // the user moved on; a stale load never wins
                }
                if crate::images::fingerprint(&viewer.target().path) != viewer.source_fingerprint {
                    viewer.load_current(cx);
                    return;
                }
                viewer.load = match result {
                    Ok(pixels) => LoadState::Ready(pixels),
                    Err(cause) => LoadState::Failed(cause),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn select_index(&mut self, index: usize, cx: &mut Context<Self>) {
        self.index = index.min(self.targets.len().saturating_sub(1));
        self.load_current(cx);
    }

    fn navigate(&mut self, delta: i64, cx: &mut Context<Self>) {
        let next = self.index as i64 + delta;
        if next < 0 || next >= self.targets.len() as i64 {
            return;
        }
        self.select_index(next as usize, cx);
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        self.load_current(cx);
    }

    // -- zoom/pan (mutating wrappers over the pure math below) --

    fn natural(&self) -> Option<Size<f32>> {
        match &self.load {
            LoadState::Ready(pixels) => Some(size(pixels.width as f32, pixels.height as f32)),
            _ => None,
        }
    }

    fn scale(&self, viewport: Size<Pixels>) -> f32 {
        match self.zoom {
            Some(scale) => scale,
            None => fit_scale(viewport, self.natural()),
        }
    }

    fn apply_zoom(&mut self, factor: f32, anchor: gpui::Point<Pixels>, viewport: Size<Pixels>) {
        let Some(natural) = self.natural() else {
            return;
        };
        let current = self.scale(viewport);
        let min = fit_scale(viewport, Some(natural));
        let (scale, pan) = zoom_around(current, self.pan, anchor, factor, min, viewport, natural);
        self.zoom = Some(scale);
        self.pan = pan;
    }

    fn zoom_step(&mut self, factor: f32, window: &mut Window, cx: &mut Context<Self>) {
        let viewport = window.viewport_size();
        let center = point(viewport.width / 2.0, viewport.height / 2.0);
        self.apply_zoom(factor, center, viewport);
        cx.notify();
    }

    fn zoom_fit(&mut self, cx: &mut Context<Self>) {
        self.zoom = None;
        self.pan = point(px(0.0), px(0.0));
        cx.notify();
    }

    fn zoom_100(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let viewport = window.viewport_size();
        let center = point(viewport.width / 2.0, viewport.height / 2.0);
        let factor = 1.0 / self.scale(viewport).max(f32::EPSILON);
        self.apply_zoom(factor, center, viewport);
        cx.notify();
    }

    fn on_wheel(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        let factor = match event.delta {
            ScrollDelta::Pixels(delta) => (-delta.y.to_f64() as f32 / 400.0).exp(),
            ScrollDelta::Lines(delta) => (-delta.y * 0.08).exp(),
        };
        self.apply_zoom(factor, event.position, window.viewport_size());
        cx.stop_propagation();
        cx.notify();
    }

    fn on_pinch(&mut self, event: &gpui::PinchEvent, window: &mut Window, cx: &mut Context<Self>) {
        // `delta` is the magnification increment (e.g. 0.05 = +5%).
        let factor = 1.0 + event.delta.clamp(-0.9, 0.9);
        self.apply_zoom(factor, event.position, window.viewport_size());
        cx.stop_propagation();
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(ImageViewerEvent::Closed);
        cx.notify();
    }

    fn toolbar_button(
        id: &'static str,
        tooltip: &'static str,
        child: impl IntoElement,
        theme: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .role(gpui::Role::Button)
            .aria_label(tooltip)
            .focusable()
            .flex_none()
            .px(px(9.0))
            .h(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(8.0))
            .text_size(px(12.0))
            .text_color(theme.text)
            .bg(crate::theme::ink(0.05))
            .hover(|el| el.bg(crate::theme::ink(0.12)))
            .cursor_pointer()
            .tooltip(move |_, cx| cx.new(|_| ViewerTooltip(tooltip.into())).into())
            .child(child)
    }

    fn render_toolbar(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let multiple = self.targets.len() > 1;
        let label: SharedString = format!(
            "{}{}",
            self.target().label,
            if multiple {
                format!("  ·  {} of {}", self.index + 1, self.targets.len())
            } else {
                String::new()
            }
        )
        .into();
        let muted = if multiple {
            theme.text
        } else {
            theme.text_faint
        };
        div()
            .absolute()
            .left_0()
            .right_0()
            .bottom(px(VIEW_MARGIN))
            .flex()
            .justify_center()
            .px(px(12.0))
            .child(
                div()
                    .max_w_full()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .px(px(6.0))
                    .py(px(4.0))
                    .rounded(px(8.0))
                    .bg(theme.bg.opacity(0.95))
                    .border_1()
                    .border_color(crate::theme::hairline(0.14))
                    .child(
                        Self::toolbar_button(
                            "viewer-prev",
                            "Previous image (←)",
                            crate::icons::icon(crate::icons::ALT_ARROW_LEFT)
                                .size(px(14.0))
                                .text_color(muted),
                            theme,
                        )
                        .when(multiple, |el| {
                            el.on_click(cx.listener(|this, _, _, cx| this.navigate(-1, cx)))
                        })
                        .when(multiple.not(), |el| el.opacity(0.4)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .max_w(px(240.0))
                            .flex_shrink(1.0)
                            .px(px(6.0))
                            .overflow_hidden()
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(theme.text)
                            .child(label),
                    )
                    .child(
                        Self::toolbar_button(
                            "viewer-next",
                            "Next image (→)",
                            crate::icons::icon(crate::icons::ALT_ARROW_RIGHT)
                                .size(px(14.0))
                                .text_color(muted),
                            theme,
                        )
                        .when(multiple, |el| {
                            el.on_click(cx.listener(|this, _, _, cx| this.navigate(1, cx)))
                        })
                        .when(multiple.not(), |el| el.opacity(0.4)),
                    )
                    .child(div().w(px(8.0)))
                    .child(
                        Self::toolbar_button("viewer-zoom-out", "Zoom out (−)", "−", theme)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.zoom_step(1.0 / ZOOM_STEP, window, cx);
                            })),
                    )
                    .child(
                        Self::toolbar_button("viewer-zoom-in", "Zoom in (+)", "+", theme).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.zoom_step(ZOOM_STEP, window, cx);
                            }),
                        ),
                    )
                    .child(
                        Self::toolbar_button("viewer-fit", "Fit to window (0)", "Fit", theme)
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_fit(cx))),
                    )
                    .child(
                        Self::toolbar_button("viewer-100", "Actual size (1)", "1:1", theme)
                            .on_click(cx.listener(|this, _, window, cx| this.zoom_100(window, cx))),
                    )
                    .child(div().w(px(8.0)))
                    .child(
                        Self::toolbar_button(
                            "viewer-close",
                            "Close (Esc)",
                            crate::icons::icon(crate::icons::CLOSE)
                                .size(px(14.0))
                                .text_color(theme.text),
                            theme,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                    ),
            )
    }
}

/// A minimal centered tooltip (same look as the composer's ActionTooltip,
/// re-declared because that one is composer-internal).
pub(crate) struct ViewerTooltip(pub(crate) SharedString);

impl Render for ViewerTooltip {
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

impl Render for ImageViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::images::flush_evicted(Some(window), cx);
        let theme = Theme::of(cx).clone();
        let viewport = window.viewport_size();
        let vw = viewport.width;
        let vh = viewport.height;
        let scale = self.scale(viewport);
        let natural = self.natural();
        self.pan = clamped_pan(self.pan, viewport, natural, scale);

        // Scaled image size and top-left (centered + pan).
        let (origin, scaled) = match &natural {
            Some(natural) => {
                let w = px(natural.width * scale);
                let h = px(natural.height * scale);
                let origin = point(
                    vw / 2.0 + self.pan.x - w / 2.0,
                    vh / 2.0 + self.pan.y - h / 2.0,
                );
                (origin, size(w, h))
            }
            None => (point(px(0.0), px(0.0)), size(px(0.0), px(0.0))),
        };

        // The image surface fills the modal: every gesture lands here, and
        // the occluding root keeps them out of the Transcript beneath.
        let surface = div()
            .id("image-viewer-surface")
            .relative()
            .size_full()
            .overflow_hidden()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    this.drag = Some(DragPan {
                        start: event.position,
                        origin: this.pan,
                    });
                    this.drag_moved = false;
                    cx.notify();
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, cx| {
                    let Some(drag) = this.drag else {
                        return;
                    };
                    let dx = event.position.x - drag.start.x;
                    let dy = event.position.y - drag.start.y;
                    if dx.abs() <= px(DRAG_SLOP) && dy.abs() <= px(DRAG_SLOP) {
                        return;
                    }
                    this.drag_moved = true;
                    this.pan = clamped_pan(
                        point(drag.origin.x + dx, drag.origin.y + dy),
                        window.viewport_size(),
                        this.natural(),
                        this.scale(window.viewport_size()),
                    );
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| {
                    this.drag = None;
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                this.on_wheel(event, window, cx);
            }))
            .on_pinch(cx.listener(|this, event: &gpui::PinchEvent, window, cx| {
                this.on_pinch(event, window, cx);
            }))
            // Backdrop click closes — but a pan that happens to end without
            // movement outside the slop never does, and the image itself
            // stops propagation below.
            .on_click(cx.listener(|this, _, _, cx| {
                if this.drag_moved {
                    return;
                }
                this.close(cx);
            }));

        let surface = match &self.load {
            LoadState::Loading => surface.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(crate::loaders::mini_mono_spinner(
                        "image-viewer-loading",
                        3.0,
                        crate::theme::ink(0.5),
                        cx.entity_id(),
                        cx,
                    )),
            ),
            LoadState::Failed(cause) => surface.child(
                div()
                    .id("image-viewer-failed")
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(10.0))
                    .child(
                        crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                            .size(px(22.0))
                            .text_color(theme.text_faint),
                    )
                    .child(
                        div()
                            .max_w(px(480.0))
                            .text_size(px(13.0))
                            .text_color(theme.text_muted)
                            .text_center()
                            .child(cause.clone()),
                    )
                    .child(
                        div()
                            .id("viewer-retry")
                            .role(gpui::Role::Button)
                            .aria_label("Retry image preview")
                            .focusable()
                            .px(px(12.0))
                            .py(px(6.0))
                            .rounded(px(8.0))
                            .bg(crate::theme::ink(0.08))
                            .text_size(px(12.0))
                            .text_color(theme.text)
                            .cursor_pointer()
                            .hover(|el| el.bg(crate::theme::ink(0.14)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.retry(cx);
                            }))
                            .child("Retry"),
                    ),
            ),
            LoadState::Ready(pixels) => surface.child(
                div()
                    .id("image-viewer-image")
                    .absolute()
                    .left(origin.x)
                    .top(origin.y)
                    .w(scaled.width)
                    .h(scaled.height)
                    .rounded(px(4.0))
                    .shadow_2xl()
                    // The image never closes the viewer; its clicks stop here.
                    .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                    .child(
                        img(pixels.pixels.clone())
                            .w(scaled.width)
                            .h(scaled.height)
                            .object_fit(gpui::ObjectFit::Fill),
                    ),
            ),
        };

        deferred(
            anchored().position(point(px(0.0), px(0.0))).child(
                div()
                    .id("image-viewer")
                    .occlude()
                    .track_focus(&self.focus)
                    .w(viewport.width)
                    .h(viewport.height)
                    .bg(crate::popover::scrim_alpha(0.82))
                    .flex()
                    .flex_col()
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                        match event.keystroke.key.as_str() {
                            "escape" => {
                                cx.stop_propagation();
                                this.close(cx);
                            }
                            "left" => {
                                cx.stop_propagation();
                                this.navigate(-1, cx);
                            }
                            "right" => {
                                cx.stop_propagation();
                                this.navigate(1, cx);
                            }
                            "+" | "=" => {
                                cx.stop_propagation();
                                this.zoom_step(ZOOM_STEP, window, cx);
                            }
                            "-" => {
                                cx.stop_propagation();
                                this.zoom_step(1.0 / ZOOM_STEP, window, cx);
                            }
                            "0" => {
                                cx.stop_propagation();
                                this.zoom_fit(cx);
                            }
                            "1" => {
                                cx.stop_propagation();
                                this.zoom_100(window, cx);
                            }
                            _ => {}
                        }
                    }))
                    .child(surface)
                    .child(self.render_toolbar(&theme, cx)),
            ),
        )
        .priority(3)
        .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// Pure geometry — the seam the unit tests exercise.
// ---------------------------------------------------------------------------

/// Scale that shows the WHOLE image inside the fit area, never upscaling
/// past actual size (small images display complete at 1:1).
fn fit_scale(viewport: Size<Pixels>, natural: Option<Size<f32>>) -> f32 {
    let Some(natural) = natural else { return 1.0 };
    let area_w = (viewport.width - px(VIEW_MARGIN * 2.0)).max(px(1.0));
    let area_h = (viewport.height - px(VIEW_MARGIN * 2.0 + TOOLBAR_SPACE)).max(px(1.0));
    let w = natural.width.max(1.0);
    let h = natural.height.max(1.0);
    1.0f32.min((area_w / w).as_f32()).min((area_h / h).as_f32())
}

/// Zoom by `factor` keeping the image point under `anchor` stationary, with
/// the scale clamped to [min_scale, MAX_SCALE] and the pan clamped so the
/// image cannot wander out of reach.
fn zoom_around(
    scale: f32,
    pan: gpui::Point<Pixels>,
    anchor: gpui::Point<Pixels>,
    factor: f32,
    min_scale: f32,
    viewport: Size<Pixels>,
    natural: Size<f32>,
) -> (f32, gpui::Point<Pixels>) {
    let new_scale = (scale * factor).clamp(min_scale, MAX_SCALE);
    let ratio = new_scale / scale.max(f32::EPSILON);
    let viewport_center = point(viewport.width / 2.0, viewport.height / 2.0);
    // The image center moves so that its offset from the anchor scales with
    // the image: the point under the cursor stays under the cursor.
    let center = point(pan.x + viewport_center.x, pan.y + viewport_center.y);
    let offset = point((center.x - anchor.x) * ratio, (center.y - anchor.y) * ratio);
    let new_center = point(anchor.x + offset.x, anchor.y + offset.y);
    let new_pan = point(
        new_center.x - viewport_center.x,
        new_center.y - viewport_center.y,
    );
    let new_pan = clamped_pan(new_pan, viewport, Some(natural), new_scale);
    (new_scale, new_pan)
}

/// Keep at least a sliver ([`TOUCH_PX`]) of the image inside the viewport so
/// it can always be dragged back and the controls stay reachable.
fn clamped_pan(
    pan: gpui::Point<Pixels>,
    viewport: Size<Pixels>,
    natural: Option<Size<f32>>,
    scale: f32,
) -> gpui::Point<Pixels> {
    let Some(natural) = natural else {
        return point(px(0.0), px(0.0));
    };
    let half_w = px(natural.width * scale / 2.0);
    let half_h = px(natural.height * scale / 2.0);
    let limit_x = (half_w - viewport.width / 2.0 + px(TOUCH_PX)).max(px(0.0));
    let limit_y = (half_h - viewport.height / 2.0 + px(TOUCH_PX)).max(px(0.0));
    point(
        pan.x.clamp(-limit_x, limit_x),
        pan.y.clamp(-limit_y, limit_y),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp(w: f32, h: f32) -> Size<Pixels> {
        size(px(w), px(h))
    }

    fn nat(w: f32, h: f32) -> Option<Size<f32>> {
        Some(size(w, h))
    }

    #[test]
    fn fit_shows_the_complete_image_without_upscaling() {
        // 2000×1000 in an 800×600 window: width is the constraint.
        let scale = fit_scale(vp(800.0, 600.0), nat(2000.0, 1000.0));
        assert!((scale - (800.0 - 48.0) / 2000.0).abs() < 1e-6);
        // A tiny image never blows up past 1:1.
        assert_eq!(fit_scale(vp(800.0, 600.0), nat(20.0, 10.0)), 1.0);
        // Nothing loaded: 1:1.
        assert_eq!(fit_scale(vp(800.0, 600.0), None), 1.0);
        // Proportions hold: a height-constrained image fits by height, and
        // the toolbar's space is reserved from the fit area.
        let scale = fit_scale(vp(800.0, 600.0), nat(400.0, 1200.0));
        assert!((scale - (600.0 - 48.0 - 64.0) / 1200.0).abs() < 1e-6);
    }

    #[test]
    fn zoom_around_the_cursor_keeps_that_point_fixed() {
        let viewport = vp(1000.0, 800.0);
        let natural = nat(2000.0, 1600.0).unwrap();
        let anchor = point(px(700.0), px(300.0));
        let (scale, pan) = zoom_around(
            1.0,
            point(px(0.0), px(0.0)),
            anchor,
            2.0,
            0.1,
            viewport,
            natural,
        );
        assert_eq!(scale, 2.0);
        // center_after = anchor + (center_before - anchor) * ratio.
        let center_before = point(px(500.0), px(400.0));
        let expected_center = point(
            anchor.x + (center_before.x - anchor.x) * 2.0,
            anchor.y + (center_before.y - anchor.y) * 2.0,
        );
        let center_after = point(pan.x + viewport.width / 2.0, pan.y + viewport.height / 2.0);
        assert_eq!(center_after, expected_center);
        // The image point under the anchor is unchanged: (anchor - top_left)
        // scales by the ratio, and top_left moved by the same center shift.
        let top_left_before = point(center_before.x - px(1000.0), center_before.y - px(800.0));
        let top_left_after = point(center_after.x - px(2000.0), center_after.y - px(1600.0));
        assert!(
            ((anchor.x - top_left_after.x) - (anchor.x - top_left_before.x) * 2.0).abs() < px(0.01)
        );
        assert!(
            ((anchor.y - top_left_after.y) - (anchor.y - top_left_before.y) * 2.0).abs() < px(0.01)
        );
    }

    #[test]
    fn zoom_clamps_to_the_scale_bounds() {
        let viewport = vp(1000.0, 800.0);
        let natural = nat(500.0, 400.0).unwrap();
        let origin = point(px(0.0), px(0.0));
        let (scale, _) = zoom_around(
            15.0,
            origin,
            point(px(0.0), px(0.0)),
            10.0,
            0.2,
            viewport,
            natural,
        );
        assert_eq!(scale, 16.0);
        let (scale, _) = zoom_around(
            0.2,
            origin,
            point(px(0.0), px(0.0)),
            0.01,
            0.2,
            viewport,
            natural,
        );
        assert_eq!(scale, 0.2);
    }

    #[test]
    fn pan_stays_within_reach_of_the_viewport() {
        // A huge zoomed image can be dragged but never lost: at least
        // TOUCH_PX of it stays inside the viewport.
        let viewport = vp(1000.0, 800.0);
        let natural = nat(4000.0, 3000.0).unwrap();
        let pan = clamped_pan(
            point(px(100_000.0), px(-100_000.0)),
            viewport,
            Some(natural),
            4.0,
        );
        assert_eq!(pan.x, px(4000.0 * 4.0 / 2.0 - 500.0 + TOUCH_PX));
        assert_eq!(pan.y, px(-(3000.0 * 4.0 / 2.0 - 400.0 + TOUCH_PX)));
        // A small image never pans at all — fit already shows it whole.
        let pan = clamped_pan(point(px(50.0), px(50.0)), viewport, nat(400.0, 300.0), 1.0);
        assert_eq!(pan, point(px(0.0), px(0.0)));
    }
}
