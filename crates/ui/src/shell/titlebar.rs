//! Titlebar chrome: geometry constants, cluster/caption rendering, and the
//! platform caption buttons. Child module of `shell` so it renders straight
//! off `Shell`'s private state.

use super::*;

fn titlebar_new_session_alpha(is_chat_route: bool, has_selected_chat: bool) -> f32 {
    if is_chat_route && has_selected_chat {
        1.0
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------
// Traffic-light-aware titlebar layout (feature-inventory §1.1)
// ---------------------------------------------------------------------------

/// Where the top-left window-control cluster starts, in px from the window's
/// left edge (holt window-controls.tsx: `left: fullscreen ? 12 : 88`). The
/// frameless hiddenInset chrome puts the macOS traffic lights at {14,15};
/// fullscreen hides them and the cluster reclaims the inset.
pub fn titlebar_cluster_start(fullscreen: bool) -> f32 {
    if fullscreen { 12.0 } else { 88.0 }
}

/// Width of the spacer ahead of the control cluster for a strip that already
/// carries `container_pad` px of its own left padding. macOS only — on
/// Linux/Windows there are no traffic lights and the cluster hugs the edge.
pub fn titlebar_spacer_width(is_macos: bool, fullscreen: bool, container_pad: f32) -> f32 {
    if !is_macos {
        return 0.0;
    }
    (titlebar_cluster_start(fullscreen) - container_pad).max(0.0)
}

/// Within-group rhythm for Back/Forward.
pub const TITLEBAR_CONTROL_GAP: f32 = 2.0;
/// Structural separation between titlebar groups: sidebar, navigation,
/// transcript identity, and trailing actions.
pub const TITLEBAR_GROUP_GAP: f32 = Theme::SPACE_SM;
/// Breathing room between the navigation cluster and transcript identity.
pub const TITLEBAR_IDENTITY_GAP: f32 = Theme::SPACE_MD;
/// A 28px action centered in the 38px titlebar with its 2px downward optical
/// shift lands 6px from the top; use the same inset at the trailing edge.
pub const TITLEBAR_ACTION_EDGE_INSET: f32 = 6.0;
/// Width of the persistent top-left button cluster itself: a 24px sidebar
/// trigger, an 8px group gap, then two 24px history buttons on a 2px rhythm.
pub const CLUSTER_BUTTONS_WIDTH: f32 = 24.0 * 3.0 + TITLEBAR_GROUP_GAP + TITLEBAR_CONTROL_GAP;
/// Extra width consumed when the collapsed-sidebar New Session action joins
/// the left controls as its own group.
pub const TITLEBAR_ACTION_SLOT_WIDTH: f32 = TITLEBAR_GROUP_GAP + 24.0;
/// Horizontal inset owned by the titlebar control row itself. Keep this value
/// paired with [`Self::titlebar_spacer`]: using a different number for the
/// spacer shifts every control while leaving the declared cluster geometry
/// unchanged.
const TITLEBAR_CLUSTER_PAD: f32 = 10.0;

/// Width of a row of `count` Linux caption buttons, drawn at the cluster's
/// own 24px-button / 2px-gap rhythm.
pub fn caption_buttons_width(count: usize) -> f32 {
    if count == 0 {
        return 0.0;
    }
    count as f32 * 24.0 + (count as f32 - 1.0) * 2.0
}

/// Where the cluster's first button starts, from the window's left edge.
/// `linux_left_captions` is the number of caption buttons holt draws at the
/// top-left on Linux (GNOME `close:…` layouts) — the app cluster follows them
/// at the shared 2px rhythm.
pub fn cluster_buttons_start(is_macos: bool, fullscreen: bool, linux_left_captions: usize) -> f32 {
    if is_macos {
        titlebar_cluster_start(fullscreen)
    } else if linux_left_captions > 0 {
        10.0 + caption_buttons_width(linux_left_captions) + 2.0
    } else {
        10.0
    }
}

/// Left clearance a full-bleed header (collapsed sidebar) needs so its content
/// starts past the overlay cluster, given the header's own `container_pad`.
pub fn cluster_clearance(
    is_macos: bool,
    fullscreen: bool,
    linux_left_captions: usize,
    container_pad: f32,
) -> f32 {
    (cluster_buttons_start(is_macos, fullscreen, linux_left_captions) + CLUSTER_BUTTONS_WIDTH + 8.0
        - container_pad)
        .max(0.0)
}

impl Shell {
    /// The animated spacer clearing the macOS traffic lights ahead of a
    /// titlebar control cluster. Fullscreen toggles tween the cluster start
    /// over 200ms ease-out ([`RESIZE`]; reduced motion snaps).
    /// `None` off macOS — no phantom flex child.
    pub(super) fn titlebar_spacer(&self, container_pad: f32) -> Option<AnyElement> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let fullscreen = self.fullscreen.unwrap_or(false);
        // The tween runs in cluster-start coordinates; the spacer is that
        // minus the container's own padding.
        let start = self.eval_tween(self.titlebar_tween, titlebar_cluster_start(fullscreen));
        let width = (start - container_pad).max(0.0);
        Some(div().flex_none().h_full().w(px(width)).into_any_element())
    }

    /// The header's content row with the animated left inset — the native port
    /// of holt __root.tsx `transition-[padding-left] duration-200 ease-out` +
    /// `style={{ paddingLeft: headerInset }}`: on sidebar toggles (and macOS
    /// fullscreen flips) the SAME element's padding tweens, so the title
    /// glides to its new x-position. Route changes SNAP: the tween is killed
    /// by every route transition (holt remounts the keyed header variants —
    /// instant swap, zero horizontal motion).
    /// Where unified-titlebar content (tabs / the settings label) starts: past
    /// the traffic lights + control cluster, riding the fullscreen inset tween.
    pub(super) fn title_bar_content_start(&self) -> f32 {
        let fullscreen = self.fullscreen.unwrap_or(false);
        let is_macos = cfg!(target_os = "macos");
        let cluster = self.eval_tween(
            self.titlebar_tween,
            cluster_buttons_start(is_macos, fullscreen, self.linux_left_caption_count()),
        );
        cluster + CLUSTER_BUTTONS_WIDTH + TITLEBAR_IDENTITY_GAP
    }

    /// The unified window titlebar: chat → the session tab strip; settings →
    /// the section label. Full-width on the glass shell; the traffic lights
    /// and control cluster overlay its left end.
    pub(super) fn render_title_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        match self.route {
            Route::Chat => self.render_session_title_bar(cx),
            Route::Settings(_) => {
                let inner = div()
                    .size_full()
                    .flex()
                    .items_center()
                    .pt(px(Theme::TITLEBAR_TOP_PAD))
                    .pl(px(self.title_bar_content_start()))
                    .pr(px(self.titlebar_right_pad(TITLEBAR_ACTION_EDGE_INSET)));
                let bar = div().h(px(Theme::TITLEBAR_HEIGHT)).flex_none().child(inner);
                self.titlebar_drag_region("settings-header-titlebar", bar, cx)
                    .into_any_element()
            }
        }
    }

    /// Make a titlebar strip drag the window — zed's platform-titlebar
    /// pattern (holt's `.drag` region): mark it a [`WindowControlArea::Drag`]
    /// (macOS app-owned titlebar), hand the drag to the compositor once the
    /// pointer moves with the button down, and double-click zooms.
    pub(super) fn titlebar_drag_region(
        &self,
        id: &'static str,
        el: gpui::Div,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        el.id(id)
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.titlebar_should_move = false))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_should_move = false),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_should_move = true),
            )
            // Hand the drag to the compositor only while the button is
            // actually held (`pressed_button` guard): on macOS
            // `start_window_move` runs AppKit's NATIVE drag session
            // (`performWindowDragWithEvent:`), and AppKit resolves a quick
            // second click inside that session as a titlebar double-click —
            // system zoom — natively, beyond gpui's reach. Without the guard a
            // stale `titlebar_should_move` (armed by a down whose bubble was
            // later stopped) would start that session from a mere hover move
            // between the two clicks of a double-click.
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, _| {
                    if this.titlebar_should_move && event.pressed_button == Some(MouseButton::Left)
                    {
                        this.titlebar_should_move = false;
                        window.start_window_move();
                    }
                }),
            )
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    if cfg!(target_os = "macos") {
                        // Native titlebar double-click action (zoom/minimize
                        // per system preference).
                        window.titlebar_double_click();
                    } else {
                        window.zoom_window();
                    }
                }
            })
    }

    /// The ONE top-left window-control cluster (sidebar toggle + back/forward —
    /// holt window-controls.tsx): rendered once, in a paint-only overlay layer
    /// pinned at the window's top-left, ABOVE the sidebar and headers. The
    /// sidebar width animates *beneath* it, so the buttons keep their element
    /// identity and never move or remount on collapse/expand; only the
    /// fullscreen traffic-light inset tweens (the animated spacer). The
    /// container has no id/listeners — everything between the buttons falls
    /// through to the titlebar drag strips below.
    pub(super) fn render_titlebar_cluster(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let can_back = self.nav.can_back();
        let can_forward = self.nav.can_forward();
        // The titlebar is the single owner of the new-session action in both
        // sidebar states. Hide it on the new-session canvas: opening another
        // blank canvas from an already blank canvas has no effect and used to
        // leave two competing + placements across the responsive variants.
        let plus_alpha = self.titlebar_plus_alpha(cx);
        let show_plus = plus_alpha > 0.01;
        div()
            .absolute()
            .top_0()
            .left_0()
            .h(px(Theme::TITLEBAR_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .pt(px(Theme::TITLEBAR_TOP_PAD))
            .px(px(TITLEBAR_CLUSTER_PAD))
            .children(self.titlebar_spacer(TITLEBAR_CLUSTER_PAD))
            // Left-side Linux captions (GNOME `close:…` layouts): the
            // root-level caption overlay owns the buttons; the cluster row
            // just starts past them, at the shared 2px rhythm.
            .children((self.linux_left_caption_count() > 0).then(|| {
                div()
                    .flex_none()
                    .h_full()
                    .w(px(caption_buttons_width(self.linux_left_caption_count())))
            }))
            .child(window_control_button(
                "toggle-sidebar",
                icons::SIDEBAR_MINIMALISTIC_LEFT,
                &theme,
                cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)),
            ))
            .child(
                div()
                    .ml(px(TITLEBAR_GROUP_GAP))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(TITLEBAR_CONTROL_GAP))
                    .child(nav_history_button(
                        "nav-back",
                        icons::ARROW_LEFT,
                        can_back,
                        &theme,
                        cx.listener(|this, _, _, cx| this.navigate_back(cx)),
                    ))
                    .child(nav_history_button(
                        "nav-forward",
                        icons::ARROW_RIGHT,
                        can_forward,
                        &theme,
                        cx.listener(|this, _, _, cx| this.navigate_forward(cx)),
                    )),
            )
            .children(show_plus.then(|| {
                div()
                    .flex_none()
                    .ml(px(TITLEBAR_GROUP_GAP))
                    .opacity(plus_alpha)
                    .child(window_control_button(
                        "titlebar-new-session",
                        icons::PLUS,
                        &theme,
                        cx.listener(|this, _, _, cx| this.open_new_session(cx)),
                    ))
            }))
            .into_any_element()
    }

    /// The titlebar owns new-session creation regardless of sidebar state. It
    /// is useful only while an existing session is selected.
    pub(super) fn titlebar_plus_alpha(&self, cx: &App) -> f32 {
        titlebar_new_session_alpha(
            matches!(self.route, Route::Chat),
            self.state.read(cx).selected_chat.is_some(),
        )
    }

    /// Native Windows caption controls integrated into Holt's unified
    /// titlebar. `WindowControlArea` maps these hit targets to HTMINBUTTON,
    /// HTMAXBUTTON, and HTCLOSE, so Windows owns their behavior (including
    /// Snap Layouts) while GPUI renders the system Segoe caption glyphs.
    pub(super) fn render_windows_caption_controls(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<AnyElement> {
        if !cfg!(target_os = "windows") {
            return None;
        }

        let theme = Theme::of(cx);
        let (maximize_id, maximize_glyph) = if window.is_maximized() {
            ("window-restore", "\u{e923}")
        } else {
            ("window-maximize", "\u{e922}")
        };
        Some(
            div()
                .id("windows-window-controls")
                .absolute()
                .top_0()
                .right_0()
                .h(px(Theme::TITLEBAR_HEIGHT))
                .flex()
                .flex_row()
                .font_family("Segoe Fluent Icons")
                .child(windows_caption_button(
                    "window-minimize",
                    "\u{e921}",
                    WindowControlArea::Min,
                    theme,
                    false,
                ))
                .child(windows_caption_button(
                    maximize_id,
                    maximize_glyph,
                    WindowControlArea::Max,
                    theme,
                    false,
                ))
                .child(windows_caption_button(
                    "window-close",
                    "\u{e8bb}",
                    WindowControlArea::Close,
                    theme,
                    true,
                ))
                .into_any_element(),
        )
    }

    /// Which caption buttons holt itself must draw on Linux: under
    /// client-side decorations (the Wayland default) nobody else will —
    /// without these the window has NO minimize/maximize/close at all.
    /// Server-side decorations (X11 WMs, KDE with SSD) already draw real
    /// buttons, so `None` there. The desktop's layout (GNOME's
    /// `button-layout` gsetting via `cx.button_layout()`) decides side and
    /// order — min/max/close on the right by default; controls the
    /// compositor can't do (e.g. minimize on some Wayland compositors) drop
    /// out, close always stays.
    #[cfg(target_os = "linux")]
    pub(super) fn resolve_linux_captions(
        window: &Window,
        cx: &App,
    ) -> Option<gpui::WindowButtonLayout> {
        use gpui::{MAX_BUTTONS_PER_SIDE, WindowButton, WindowButtonLayout};
        if !matches!(
            window.window_decorations(),
            gpui::Decorations::Client { .. }
        ) {
            return None;
        }
        let layout = cx
            .button_layout()
            .unwrap_or_else(WindowButtonLayout::linux_default);
        let supported = window.window_controls();
        let filter_side = |side: [Option<WindowButton>; MAX_BUTTONS_PER_SIDE]| {
            let mut out = [None; MAX_BUTTONS_PER_SIDE];
            let mut i = 0;
            for button in side.into_iter().flatten() {
                let keep = match button {
                    WindowButton::Minimize => supported.minimize,
                    WindowButton::Maximize => supported.maximize,
                    WindowButton::Close => true,
                };
                if keep {
                    out[i] = Some(button);
                    i += 1;
                }
            }
            out
        };
        let layout = WindowButtonLayout {
            left: filter_side(layout.left),
            right: filter_side(layout.right),
        };
        (layout.left[0].is_some() || layout.right[0].is_some()).then_some(layout)
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn resolve_linux_captions(
        _window: &Window,
        _cx: &App,
    ) -> Option<gpui::WindowButtonLayout> {
        None
    }

    pub(super) fn linux_left_caption_count(&self) -> usize {
        self.linux_captions
            .map_or(0, |l| l.left.iter().flatten().count())
    }

    pub(super) fn linux_right_caption_count(&self) -> usize {
        self.linux_captions
            .map_or(0, |l| l.right.iter().flatten().count())
    }

    /// Right padding titlebar content needs to clear the platform's caption
    /// controls (native Windows cluster / holt-drawn Linux buttons).
    pub(super) fn titlebar_right_pad(&self, base: f32) -> f32 {
        titlebar_right_padding(
            cfg!(target_os = "windows"),
            self.linux_right_caption_count(),
            base,
        )
    }

    /// Holt-drawn Linux caption controls, one overlay per populated side.
    /// Shell-level chrome like the Windows cluster: mounted at the root so
    /// they stay above the splash and the error gate.
    pub(super) fn render_linux_caption_controls(
        &self,
        window: &Window,
        cx: &App,
    ) -> Vec<AnyElement> {
        let Some(layout) = self.linux_captions else {
            return Vec::new();
        };
        let theme = Theme::of(cx);
        let is_maximized = window.is_maximized();
        // Ids can be per-button (not per-side): the layout parser dedups, so
        // a button never appears on both sides at once.
        let strip = |buttons: &[Option<gpui::WindowButton>]| {
            div()
                .absolute()
                .top_0()
                .h(px(Theme::TITLEBAR_HEIGHT))
                .flex()
                .flex_row()
                .items_center()
                .pt(px(Theme::TITLEBAR_TOP_PAD))
                .gap(px(2.0))
                .px(px(10.0))
                .children(buttons.iter().flatten().map(|button| {
                    match button {
                        gpui::WindowButton::Minimize => linux_caption_button(
                            "window-minimize",
                            icons::WINDOW_MINIMIZE,
                            false,
                            theme,
                            |_, window, _| window.minimize_window(),
                        )
                        .into_any_element(),
                        gpui::WindowButton::Maximize => {
                            let (id, icon_path) = if is_maximized {
                                ("window-restore", icons::WINDOW_RESTORE)
                            } else {
                                ("window-maximize", icons::WINDOW_MAXIMIZE)
                            };
                            linux_caption_button(id, icon_path, false, theme, |_, window, _| {
                                window.zoom_window()
                            })
                            .into_any_element()
                        }
                        gpui::WindowButton::Close => linux_caption_button(
                            "window-close",
                            icons::CLOSE,
                            true,
                            theme,
                            |_, window, _| window.remove_window(),
                        )
                        .into_any_element(),
                    }
                }))
        };
        let mut out = Vec::new();
        if layout.left[0].is_some() {
            out.push(strip(&layout.left).left_0().into_any_element());
        }
        if layout.right[0].is_some() {
            out.push(strip(&layout.right).right_0().into_any_element());
        }
        out
    }
}

/// The gate's faint grid backdrop (holt styles.css `.bg-grid`):
/// 44px hairlines at white 3.5%, with the radial mask approximated by edge
/// gradients back into the page background (gpui has no mask-image).
pub(super) fn grid_backdrop(theme: &Theme) -> AnyElement {
    let line = crate::theme::hairline(0.035);
    let bg = theme.bg;
    const STEP: f32 = 44.0;
    const SPAN: f32 = 2640.0;
    let verticals = (1..(SPAN / STEP) as usize).map(|i| {
        div()
            .absolute()
            .left(px(i as f32 * STEP))
            .top_0()
            .bottom_0()
            .w(px(1.0))
            .bg(line)
    });
    let horizontals = (1..((SPAN * 0.75) / STEP) as usize).map(|i| {
        div()
            .absolute()
            .top(px(i as f32 * STEP))
            .left_0()
            .right_0()
            .h(px(1.0))
            .bg(line)
    });
    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .children(verticals)
        .children(horizontals)
        // Mask approximation: fade the grid back into the background toward
        // the window edges (the original masks to an ellipse at 50% / 40%).
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(120.0))
                .bg(gpui::linear_gradient(
                    180.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
                .h(px(260.0))
                .bg(gpui::linear_gradient(
                    0.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(px(200.0))
                .bg(gpui::linear_gradient(
                    90.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(200.0))
                .bg(gpui::linear_gradient(
                    270.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .into_any_element()
}

/// A size-6 icon button for the titlebar strip (holt window-controls.tsx:
/// `grid size-6 place-items-center rounded-md text-muted-foreground`).
fn window_control_button(
    id: &'static str,
    icon_path: &'static str,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let muted = theme.text_muted;
    let fade_key = format!("window-control-{id}");
    div()
        .id(id)
        .size(px(24.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        // holt window-controls.tsx: `transition-colors` — the wash fades.
        .bg(motion::hover_blend(
            &fade_key,
            theme.glass_hover().opacity(0.0),
            theme.glass_hover(),
        ))
        .on_hover(motion::hover_listener(fade_key))
        // Buttons in/over a titlebar drag strip must be EXCLUDED from the
        // strip's event surface entirely. `.occlude()` (gpui
        // `HitboxBehavior::BlockMouse`) makes the window hit-test STOP at the
        // button, so every `is_hovered`-guarded strip listener — the
        // mouse-down that arms the drag, the mouse-move that hands AppKit a
        // native drag session (`performWindowDragWithEvent:`, whose second
        // quick click zooms NATIVELY on macOS), and the `click_count == 2`
        // zoom handler — never fires with the pointer over a button. It also
        // removes the button's rect from the native Drag control-area
        // hit-test on Windows/Linux. The click-level stop_propagation is
        // zed's ButtonLike belt on top. Double-click on EMPTY strip space
        // still zooms — nothing occludes it there.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(icon(icon_path).size(px(16.0)).text_color(muted))
}

const WINDOWS_CAPTION_BUTTON_WIDTH: f32 = 36.0;
const WINDOWS_CAPTION_WIDTH: f32 = WINDOWS_CAPTION_BUTTON_WIDTH * 3.0;

/// Right padding for titlebar content: past the native Windows caption
/// cluster, or past holt's own Linux caption buttons (10px edge inset +
/// the button row) when the layout puts any on the right.
fn titlebar_right_padding(is_windows: bool, linux_right_captions: usize, base: f32) -> f32 {
    base + if is_windows {
        WINDOWS_CAPTION_WIDTH
    } else if linux_right_captions > 0 {
        10.0 + caption_buttons_width(linux_right_captions)
    } else {
        0.0
    }
}

/// A Windows-owned caption target using the same system glyphs and native
/// non-client hit-test areas as GPUI/Zed's platform titlebar.
fn windows_caption_button(
    id: &'static str,
    glyph: &'static str,
    area: WindowControlArea,
    theme: &Theme,
    close: bool,
) -> impl IntoElement {
    let (hover_bg, hover_fg, active_bg, active_fg) = if close {
        let red: gpui::Hsla = gpui::rgb(0xe81123).into();
        (
            red,
            gpui::white(),
            red.opacity(0.8),
            gpui::white().opacity(0.8),
        )
    } else {
        (
            theme.glass_hover(),
            theme.text,
            theme.glass_hover().opacity(0.7),
            theme.text,
        )
    };
    div()
        .id(id)
        .w(px(WINDOWS_CAPTION_BUTTON_WIDTH))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .text_size(crate::typography::ui_rems(10.0))
        .text_color(theme.text)
        .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
        .active(move |style| style.bg(active_bg).text_color(active_fg))
        .occlude()
        .window_control_area(area)
        .child(glyph)
}

/// A Linux caption button in holt's own cluster style (24px, rounded-6,
/// 16px linear icon). gpui's `WindowControlArea` hit-testing is inert on
/// Linux, so unlike the Windows cluster these carry explicit click handlers
/// (`minimize_window` / `zoom_window` / `remove_window`), the same calls
/// zed's Linux titlebar makes. `occlude` + `prevent_default` keep them out
/// of the drag strip's event surface (see [`window_control_button`]).
fn linux_caption_button(
    id: &'static str,
    icon_path: &'static str,
    close: bool,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let (muted, hover_bg, hover_fg) = if close {
        let red: gpui::Hsla = gpui::rgb(0xe81123).into();
        (theme.text_muted, red, gpui::white())
    } else {
        (theme.text_muted, theme.glass_hover(), theme.text)
    };
    div()
        .id(id)
        // gpui svgs don't inherit the div's text color — recolor the glyph
        // on hover through the group instead (zed's WindowControl idiom).
        .group("linux-caption-button")
        .size(px(24.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(move |style| style.bg(hover_bg))
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(
            icon(icon_path)
                .size(px(16.0))
                .text_color(muted)
                .group_hover("linux-caption-button", move |style| {
                    style.text_color(hover_fg)
                }),
        )
}

/// A titlebar history button (holt window-controls.tsx): enabled it is a
/// normal window-control button; disabled it dims to 35% opacity and ignores
/// the pointer (`disabled:pointer-events-none disabled:opacity-35`).
fn nav_history_button(
    id: &'static str,
    icon_path: &'static str,
    enabled: bool,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    if !enabled {
        return div()
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            // Even disabled it reads as a control — occlude so double-clicks
            // on it don't fall through to the titlebar strip's zoom handler.
            .occlude()
            .child(
                icon(icon_path)
                    .size(px(16.0))
                    .text_color(theme.text_muted.opacity(0.35)),
            )
            .into_any_element();
    }
    window_control_button(id, icon_path, theme, on_click).into_any_element()
}

/// A size-7 icon button for the main-panel header (holt __root.tsx:
/// `grid size-7 place-items-center rounded-md text-muted-foreground`).
pub(super) fn header_icon_button(
    id: &'static str,
    icon_path: &'static str,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let muted = theme.text_muted;
    let fade_key = format!("header-icon-{id}");
    div()
        .id(id)
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        // holt __root.tsx header buttons: `transition-colors`.
        .bg(motion::hover_blend(
            &fade_key,
            crate::theme::wash(0.0),
            crate::theme::wash(0.11),
        ))
        .on_hover(motion::hover_listener(fade_key))
        // Same occlusion + click-swallowing as [`window_control_button`]: this
        // button sits inside the chat header's titlebar drag region, so its
        // rect must be carved out of the strip's drag/double-click surface.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(icon(icon_path).size(px(16.0)).text_color(muted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_action_lives_in_the_titlebar_only_when_useful() {
        assert_eq!(titlebar_new_session_alpha(true, true), 1.0);
        assert_eq!(titlebar_new_session_alpha(true, false), 0.0);
        assert_eq!(titlebar_new_session_alpha(false, true), 0.0);
        assert_eq!(titlebar_new_session_alpha(false, false), 0.0);
    }

    #[test]
    fn titlebar_cluster_matches_holt_window_controls() {
        // holt window-controls.tsx: `left: fullscreen ? 12 : 88` — the
        // cluster clears the {14,15} traffic lights, and reclaims the inset
        // when fullscreen hides them.
        assert_eq!(titlebar_cluster_start(false), 88.0);
        assert_eq!(titlebar_cluster_start(true), 12.0);
        assert_eq!(TITLEBAR_CONTROL_GAP, 2.0);
        assert_eq!(TITLEBAR_GROUP_GAP, Theme::SPACE_SM);
        assert_eq!(TITLEBAR_IDENTITY_GAP, Theme::SPACE_MD);
        assert_eq!(CLUSTER_BUTTONS_WIDTH, 82.0);
        assert_eq!(TITLEBAR_ACTION_SLOT_WIDTH, 32.0);
        assert_eq!(TITLEBAR_ACTION_EDGE_INSET, 6.0);
    }

    #[test]
    fn titlebar_spacer_selects_per_platform_and_fullscreen() {
        // macOS, lights visible: spacer fills up to the 88px cluster start.
        assert_eq!(titlebar_spacer_width(true, false, 10.0), 78.0);
        assert_eq!(titlebar_spacer_width(true, false, 12.0), 76.0);
        assert_eq!(titlebar_spacer_width(true, false, 26.0), 62.0);
        // macOS fullscreen: the inset animates away (clamped at zero when the
        // strip's own padding already exceeds the 12px cluster start).
        assert_eq!(titlebar_spacer_width(true, true, 10.0), 2.0);
        assert_eq!(titlebar_spacer_width(true, true, 26.0), 0.0);
        // Linux / Windows: never any inset.
        assert_eq!(titlebar_spacer_width(false, false, 10.0), 0.0);
        assert_eq!(titlebar_spacer_width(false, true, 10.0), 0.0);
        assert_eq!(
            TITLEBAR_CLUSTER_PAD + titlebar_spacer_width(true, false, TITLEBAR_CLUSTER_PAD),
            titlebar_cluster_start(false),
            "the rendered row padding and spacer must land on the declared cluster start"
        );
    }

    #[test]
    fn windows_caption_controls_reserve_titlebar_space() {
        assert_eq!(titlebar_right_padding(true, 0, 16.0), 124.0);
        assert_eq!(titlebar_right_padding(false, 0, 16.0), 16.0);
    }

    #[test]
    fn linux_caption_controls_reserve_titlebar_space() {
        // 24px buttons on the cluster's 2px rhythm.
        assert_eq!(caption_buttons_width(0), 0.0);
        assert_eq!(caption_buttons_width(1), 24.0);
        assert_eq!(caption_buttons_width(3), 76.0);
        // Right-side captions (the Linux default: minimize,maximize,close):
        // content pads past the 10px edge inset + the button row.
        assert_eq!(titlebar_right_padding(false, 3, 16.0), 16.0 + 10.0 + 76.0);
        // GNOME-vanilla ":close" — a single right button.
        assert_eq!(titlebar_right_padding(false, 1, 16.0), 16.0 + 10.0 + 24.0);
        // Left-side captions ("close:…" layouts) shift the app cluster right
        // by the button row + one 2px gap.
        assert_eq!(cluster_buttons_start(false, false, 0), 10.0);
        assert_eq!(cluster_buttons_start(false, false, 1), 10.0 + 24.0 + 2.0);
        assert_eq!(cluster_buttons_start(false, false, 3), 10.0 + 76.0 + 2.0);
        // macOS ignores the Linux caption count entirely.
        assert_eq!(cluster_buttons_start(true, false, 3), 88.0);
    }

    #[test]
    fn cluster_clearance_clears_the_overlay_buttons() {
        // Linux: buttons at 10..92; a 16px-padded header needs 84 more px to
        // put content at 92 + 8 breathing room.
        assert_eq!(cluster_clearance(false, false, 0, 16.0), 84.0);
        assert_eq!(cluster_clearance(false, false, 0, 10.0), 90.0);
        // Linux with a left-side close caption: everything shifts one slot.
        assert_eq!(cluster_clearance(false, false, 1, 16.0), 84.0 + 26.0);
        // macOS: buttons start at the 88px traffic-light cluster start.
        assert_eq!(
            cluster_clearance(true, false, 0, 16.0),
            88.0 + CLUSTER_BUTTONS_WIDTH + 8.0 - 16.0
        );
        // macOS fullscreen: cluster reclaims the inset (starts at 12).
        assert_eq!(
            cluster_clearance(true, true, 0, 16.0),
            12.0 + CLUSTER_BUTTONS_WIDTH + 8.0 - 16.0
        );
    }
}
