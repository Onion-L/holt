//! The file tab's embedded image surface (ticket 08): a read-only zoom/pan
//! viewer for supported image files, living INSIDE the contents area — not
//! the modal lightbox. It is not a second viewer: the fit/zoom/pan geometry
//! is the lightbox's exact pure math ([`crate::image_viewer`]'s `fit_scale`,
//! `zoom_around`, `clamped_pan`) and the pixels arrive through the same
//! bounded decode pipeline with its format/size/limit guards
//! ([`crate::images::ViewerPixels`]). The bytes themselves come from the
//! workspace-FENCED read (`ReadWorkspaceImage`) — the owning FileViewer does
//! the loading, so staleness is judged by its generation guards before
//! anything reaches this surface.

use gpui::{
    AnyElement, Bounds, Context, EventEmitter, MouseButton, Pixels, Render, ScrollWheelEvent,
    SharedString, Size, Window, canvas, div, img, point, prelude::*, px, size,
};

use crate::image_viewer::{
    DRAG_SLOP, DragPan, ZOOM_STEP, clamped_pan, fit_scale, wheel_zoom_factor, zoom_around,
};
use crate::images::ViewerPixels;
use crate::theme::Theme;

/// The surface asks the owning viewer for another load (the Retry button
/// on a failed decode/read) or to hand the file to the OS's default
/// handler (the failed state's escape hatch — the bytes may be fine; this
/// viewer is just not the right tool for them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileImageSurfaceEvent {
    Retry,
    OpenExternal,
}

enum LoadState {
    Loading,
    Ready(ViewerPixels),
    Failed(SharedString),
}

pub struct FileImageSurface {
    load: LoadState,
    /// Manual zoom scale; `None` = fit-to-surface (recomputed per frame, so
    /// a pane resize keeps the image reachable).
    zoom: Option<f32>,
    /// Offset of the image center from the surface center (surface-local).
    pan: gpui::Point<Pixels>,
    drag: Option<DragPan>,
    /// The surface's own bounds, captured by the bounds probe's prepaint —
    /// gesture math runs between frames and reads the latest paint's box.
    last_bounds: Option<Bounds<Pixels>>,
}

impl EventEmitter<FileImageSurfaceEvent> for FileImageSurface {}

impl Default for FileImageSurface {
    fn default() -> Self {
        Self::new()
    }
}

impl FileImageSurface {
    pub fn new() -> Self {
        Self {
            load: LoadState::Loading,
            zoom: None,
            pan: point(px(0.0), px(0.0)),
            drag: None,
            last_bounds: None,
        }
    }

    /// A load started (or restarted): back to the loading state. Pixels and
    /// fit state from a previous image never bleed into the new one.
    pub fn begin_load(&mut self, cx: &mut Context<Self>) {
        self.discard_loaded();
        self.zoom = None;
        self.pan = point(px(0.0), px(0.0));
        self.drag = None;
        self.load = LoadState::Loading;
        cx.notify();
    }

    /// A fenced, generation-checked load answered: show these pixels at fit.
    pub fn set_pixels(&mut self, pixels: ViewerPixels, cx: &mut Context<Self>) {
        self.discard_loaded();
        self.zoom = None;
        self.pan = point(px(0.0), px(0.0));
        self.drag = None;
        self.load = LoadState::Ready(pixels);
        cx.notify();
    }

    /// The load failed: the cause is user-presentable; Retry re-asks.
    pub fn set_failed(&mut self, cause: SharedString, cx: &mut Context<Self>) {
        self.discard_loaded();
        self.load = LoadState::Failed(cause);
        cx.notify();
    }

    fn discard_loaded(&mut self) {
        if let LoadState::Ready(pixels) = &self.load {
            crate::images::discard(pixels.pixels.clone());
        }
    }

    fn natural(&self) -> Option<Size<f32>> {
        match &self.load {
            LoadState::Ready(pixels) => Some(size(pixels.width as f32, pixels.height as f32)),
            _ => None,
        }
    }

    /// Test probe: the loaded image's natural size, once pixels landed.
    #[cfg(test)]
    pub(crate) fn natural_size(&self) -> Option<(u32, u32)> {
        match &self.load {
            LoadState::Ready(pixels) => Some((pixels.width, pixels.height)),
            _ => None,
        }
    }

    /// Test probe: the failure cause, when the load failed.
    #[cfg(test)]
    pub(crate) fn failure_cause(&self) -> Option<String> {
        match &self.load {
            LoadState::Failed(cause) => Some(cause.to_string()),
            _ => None,
        }
    }

    fn scale(&self, viewport: Size<Pixels>) -> f32 {
        match self.zoom {
            Some(scale) => scale,
            None => fit_scale(viewport, self.natural()),
        }
    }

    fn viewport(&self) -> Size<Pixels> {
        self.last_bounds
            .map(|bounds| bounds.size)
            .unwrap_or_else(|| size(px(0.0), px(0.0)))
    }

    /// Surface-local anchor for gesture zooming.
    fn local(&self, position: gpui::Point<Pixels>) -> gpui::Point<Pixels> {
        match self.last_bounds {
            Some(bounds) => point(position.x - bounds.origin.x, position.y - bounds.origin.y),
            None => position,
        }
    }

    fn apply_zoom(&mut self, factor: f32, anchor: gpui::Point<Pixels>) {
        let Some(natural) = self.natural() else {
            return;
        };
        let viewport = self.viewport();
        let current = self.scale(viewport);
        let min = fit_scale(viewport, Some(natural));
        let (scale, pan) = zoom_around(current, self.pan, anchor, factor, min, viewport, natural);
        self.zoom = Some(scale);
        self.pan = pan;
    }

    fn on_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        self.apply_zoom(wheel_zoom_factor(&event.delta), self.local(event.position));
        cx.stop_propagation();
        cx.notify();
    }

    fn on_pinch(&mut self, event: &gpui::PinchEvent, cx: &mut Context<Self>) {
        let factor = 1.0 + event.delta.clamp(-0.9, 0.9);
        self.apply_zoom(factor, self.local(event.position));
        cx.stop_propagation();
        cx.notify();
    }

    fn zoom_step(&mut self, factor: f32, cx: &mut Context<Self>) {
        let viewport = self.viewport();
        let center = point(viewport.width / 2.0, viewport.height / 2.0);
        self.apply_zoom(factor, center);
        cx.notify();
    }

    fn zoom_fit(&mut self, cx: &mut Context<Self>) {
        self.zoom = None;
        self.pan = point(px(0.0), px(0.0));
        cx.notify();
    }

    fn zoom_100(&mut self, cx: &mut Context<Self>) {
        let viewport = self.viewport();
        let center = point(viewport.width / 2.0, viewport.height / 2.0);
        let factor = 1.0 / self.scale(viewport).max(f32::EPSILON);
        self.apply_zoom(factor, center);
        cx.notify();
    }

    /// A small square toolbar chip; the label variant carries the zoom %.
    fn chip(
        id: &'static str,
        tooltip: &'static str,
        child: impl IntoElement,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .debug_selector(|| id.to_string())
            .role(gpui::Role::Button)
            .aria_label(tooltip)
            .flex_none()
            .size(px(26.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .text_size(px(11.0))
            .cursor_pointer()
            .hover(|el| el.bg(crate::theme::ink(0.10)))
            .tooltip(move |_, cx| {
                cx.new(|_| crate::image_viewer::ViewerTooltip(tooltip.into()))
                    .into()
            })
            .child(child)
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let zoom_label = format!("{:.0}%", self.scale(self.viewport()) * 100.0);
        let glyph = |name| {
            crate::icons::icon(name)
                .size(px(14.0))
                .text_color(theme.text_muted)
        };
        div()
            .id("file-image-toolbar")
            .debug_selector(|| "file-image-toolbar".into())
            .absolute()
            .left_0()
            .right_0()
            .bottom(px(12.0))
            .flex()
            .justify_center()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .px(px(2.0))
                    .py(px(2.0))
                    .rounded(px(8.0))
                    .bg(crate::theme::ink(0.06))
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        Self::chip(
                            "file-image-zoom-out",
                            "Zoom out",
                            glyph(crate::icons::WINDOW_MINIMIZE),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.zoom_step(1.0 / ZOOM_STEP, cx);
                        })),
                    )
                    .child(
                        Self::chip(
                            "file-image-100",
                            "Actual size",
                            div().text_color(theme.text_muted).child(zoom_label),
                        )
                        .w(px(44.0))
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.zoom_100(cx);
                        })),
                    )
                    .child(
                        Self::chip("file-image-zoom-in", "Zoom in", glyph(crate::icons::PLUS))
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.zoom_step(ZOOM_STEP, cx);
                            })),
                    )
                    .child(
                        Self::chip(
                            "file-image-fit",
                            "Fit to pane",
                            div().text_color(theme.text_muted).child("Fit"),
                        )
                        .w(px(36.0))
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.zoom_fit(cx);
                        })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for FileImageSurface {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::images::flush_evicted(Some(window), cx);
        let theme = Theme::of(cx).clone();

        // The bounds probe: a zero-interaction canvas that covers the
        // surface exactly (absolute + full size), recording — and, on
        // change, re-notifying — its box. This frame's gesture math and the
        // next frame's layout read it.
        let entity = cx.entity();
        let probe = canvas(
            move |bounds, _, cx| {
                entity.update(cx, |surface, cx| {
                    if surface.last_bounds != Some(bounds) {
                        surface.last_bounds = Some(bounds);
                        cx.notify();
                    }
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        let viewport = self.viewport();
        let scale = self.scale(viewport);
        let natural = self.natural();
        self.pan = clamped_pan(self.pan, viewport, natural, scale);

        // Scaled image size and top-left, surface-local (centered + pan).
        let origin = match (&natural, viewport.width > px(0.0)) {
            (Some(natural), true) => {
                let w = px(natural.width * scale);
                let h = px(natural.height * scale);
                point(
                    viewport.width / 2.0 + self.pan.x - w / 2.0,
                    viewport.height / 2.0 + self.pan.y - h / 2.0,
                )
            }
            _ => point(px(0.0), px(0.0)),
        };

        let mut surface = div()
            .id("file-image-surface")
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
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                let Some(drag) = this.drag else {
                    return;
                };
                let dx = event.position.x - drag.start.x;
                let dy = event.position.y - drag.start.y;
                if dx.abs() <= px(DRAG_SLOP) && dy.abs() <= px(DRAG_SLOP) {
                    return;
                }
                this.pan = clamped_pan(
                    point(drag.origin.x + dx, drag.origin.y + dy),
                    this.viewport(),
                    this.natural(),
                    this.scale(this.viewport()),
                );
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| {
                    this.drag = None;
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                this.on_wheel(event, cx);
            }))
            .on_pinch(cx.listener(|this, event: &gpui::PinchEvent, _, cx| {
                this.on_pinch(event, cx);
            }))
            .child(probe);

        surface = match &self.load {
            LoadState::Loading => surface.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(crate::loaders::mini_mono_spinner(
                        "file-image-loading",
                        3.0,
                        crate::theme::ink(0.5),
                        cx.entity_id(),
                        cx,
                    )),
            ),
            LoadState::Failed(cause) => surface.child(
                div()
                    .id("file-image-failed")
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(10.0))
                    .p(px(16.0))
                    .child(
                        crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                            .size(px(22.0))
                            .text_color(theme.text_faint),
                    )
                    .child(
                        div()
                            .max_w(px(360.0))
                            .text_size(px(12.5))
                            .text_color(theme.text_muted)
                            .text_center()
                            .child(cause.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .id("file-image-retry")
                                    .debug_selector(|| "file-image-retry".into())
                                    .role(gpui::Role::Button)
                                    .aria_label("Retry image preview")
                                    .px(px(12.0))
                                    .py(px(6.0))
                                    .rounded(px(8.0))
                                    .bg(crate::theme::ink(0.08))
                                    .text_size(px(12.0))
                                    .text_color(theme.text)
                                    .cursor_pointer()
                                    .hover(|el| el.bg(crate::theme::ink(0.14)))
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.stop_propagation();
                                        cx.emit(FileImageSurfaceEvent::Retry);
                                    }))
                                    .child("Retry"),
                            )
                            .child(
                                div()
                                    .id("file-image-open-external")
                                    .debug_selector(|| "file-image-open-external".into())
                                    .role(gpui::Role::Button)
                                    .aria_label("Open externally")
                                    .h(px(28.0))
                                    .px(px(10.0))
                                    .rounded(px(6.0))
                                    .border_1()
                                    .border_color(theme.border)
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .cursor_pointer()
                                    .hover(|el| el.bg(crate::theme::wash(0.06)))
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.stop_propagation();
                                        cx.emit(FileImageSurfaceEvent::OpenExternal);
                                    }))
                                    .child(
                                        crate::icons::icon(crate::icons::ARROW_UP_RIGHT)
                                            .size(px(12.0))
                                            .text_color(theme.text_muted),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(12.0))
                                            .text_color(theme.text)
                                            .child("Open externally"),
                                    ),
                            ),
                    ),
            ),
            LoadState::Ready(pixels) => {
                if viewport.width > px(0.0) && natural.is_some() {
                    surface
                        .child(
                            div()
                                .id("file-image-picture")
                                .absolute()
                                .left(origin.x)
                                .top(origin.y)
                                .w(px(natural.map(|n| n.width * scale).unwrap_or(0.0)))
                                .h(px(natural.map(|n| n.height * scale).unwrap_or(0.0)))
                                .rounded(px(4.0))
                                .shadow_2xl()
                                .child(
                                    img(pixels.pixels.clone())
                                        .w(px(natural.map(|n| n.width * scale).unwrap_or(0.0)))
                                        .h(px(natural.map(|n| n.height * scale).unwrap_or(0.0)))
                                        .object_fit(gpui::ObjectFit::Fill),
                                ),
                        )
                        .child(self.render_toolbar(cx))
                } else {
                    // Bounds not measured yet: the next frame (the probe
                    // notifies) paints at a real fit.
                    surface
                }
            }
        };
        surface
    }
}

impl Drop for FileImageSurface {
    fn drop(&mut self) {
        self.discard_loaded();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn pixels(w: u32, h: u32) -> ViewerPixels {
        ViewerPixels {
            pixels: std::sync::Arc::new(gpui::RenderImage::new(smallvec::smallvec![
                image::Frame::new(image::RgbaImage::new(w, h))
            ])),
            width: w,
            height: h,
        }
    }

    /// A failure shows its cause and offers both escape hatches: the retry
    /// (the owner re-loads) and the external open (the OS's handler — the
    /// same action every other unsupported file offers).
    #[gpui::test]
    fn failures_surface_their_cause_with_retry_and_external_open(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let (surface, cx) = cx.add_window_view(|_, _| FileImageSurface::new());
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        cx.update(|_, cx| {
            cx.subscribe(&surface, move |_, event: &FileImageSurfaceEvent, _| {
                seen.borrow_mut().push(event.clone());
            })
            .detach();
        });
        surface.update(cx, |surface, cx| {
            surface.set_failed("Image file could not be read: nope".into(), cx);
        });
        cx.run_until_parked();
        surface.read_with(cx, |surface, _| {
            assert!(matches!(&surface.load, LoadState::Failed(cause) if cause.contains("nope")));
            // A failed surface has nothing to zoom or pan.
            assert_eq!(surface.natural(), None);
        });
        let retry = cx.debug_bounds("file-image-retry").expect("retry rendered");
        cx.simulate_click(retry.center(), Default::default());
        let external = cx
            .debug_bounds("file-image-open-external")
            .expect("external open rendered");
        cx.simulate_click(external.center(), Default::default());
        cx.run_until_parked();
        assert_eq!(
            *events.borrow(),
            vec![
                FileImageSurfaceEvent::Retry,
                FileImageSurfaceEvent::OpenExternal,
            ],
            "both failure escape hatches reach the owning viewer"
        );
    }

    /// Replacement pixels reset fit and pan (a reload never inherits the
    /// previous image's zoom state), and the loaded state answers natural
    /// size for the shared geometry.
    #[gpui::test]
    fn replacement_pixels_reset_the_view_state(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let surface = cx.new(|_| FileImageSurface::new());
        surface.update(cx, |surface, cx| {
            surface.set_pixels(pixels(2000, 1000), cx);
            surface.zoom = Some(4.0);
            surface.pan = point(px(120.0), px(-80.0));
        });
        surface.update(cx, |surface, cx| {
            surface.set_pixels(pixels(4, 2), cx);
        });
        surface.read_with(cx, |surface, _| {
            assert_eq!(surface.zoom, None);
            assert_eq!(surface.pan, point(px(0.0), px(0.0)));
            assert_eq!(surface.natural(), Some(size(4.0, 2.0)));
        });
    }

    /// The bounds probe records the surface's own box during prepaint, so a
    /// rendered surface knows its viewport (one notify-delayed frame later).
    #[gpui::test]
    fn the_bounds_probe_measures_the_surface(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let (surface, cx) = cx.add_window_view(|_, _| FileImageSurface::new());
        cx.simulate_resize(size(px(600.0), px(400.0)));
        cx.run_until_parked();
        surface.read_with(cx, |surface, _| {
            let bounds = surface.last_bounds.expect("probe recorded bounds");
            assert_eq!(bounds.size, size(px(600.0), px(400.0)));
        });
    }
}
