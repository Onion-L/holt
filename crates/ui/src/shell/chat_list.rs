//! The sidebar's session list: chat rows, resort glide, settings nav, and the
//! chat context menu. Child module of `shell` so it renders straight off
//! `Shell`'s private state.

use super::*;

/// Sidebar resort glide (feature-inventory §1.6): 260ms
/// `cubic-bezier(0.22,1,0.36,1)` per-row translate, the View Transitions
/// equivalent.
pub const RESORT: MotionSpec = MotionSpec::new(260, motion::EASE_RESORT);

/// FLIP diff for a keyed list: given the previously rendered order and the new
/// order (key + row height), return each surviving key's paint-only start
/// offset `old_y - new_y` (only keys whose position actually moved). `gap` is
/// the flex gap between rows. Pure — drives the sidebar resort glide.
pub fn resort_offsets(
    old: &[(String, f32)],
    new: &[(String, f32)],
    gap: f32,
) -> std::collections::HashMap<String, f32> {
    let mut old_y = std::collections::HashMap::new();
    let mut y = 0.0_f32;
    for (key, height) in old {
        old_y.insert(key.as_str(), y);
        y += height + gap;
    }
    let mut offsets = std::collections::HashMap::new();
    let mut y = 0.0_f32;
    for (key, height) in new {
        if let Some(prev) = old_y.get(key.as_str()) {
            let dy = prev - y;
            if dy.abs() > 0.5 {
                offsets.insert(key.clone(), dy);
            }
        }
        y += height + gap;
    }
    offsets
}

/// The dragged session-row payload (sidebar manual reorder).
pub(super) struct ChatRowDrag {
    chat_id: String,
    title: SharedString,
}

/// Live sidebar reorder: the dragged chat and the insertion slot
/// (`0..=rows`) under the pointer — `None` while it is off the list.
pub(super) struct SidebarRowDragState {
    chat_id: String,
    slot: Option<usize>,
    /// Last drag-move position: the edge autoscroll re-reads it each tick,
    /// since a pointer held still at the edge sends no further moves.
    pointer: Point<Pixels>,
    /// (chat id, height, pinned) per row as last rendered.
    rows: std::rc::Rc<Vec<(String, f32, bool)>>,
    /// Pending edge-autoscroll tick; dropped (cancelled) with the drag.
    scroll_task: Option<Task<()>>,
}

/// Ghost card following the pointer while a session row drags.
struct ChatRowGhost {
    title: SharedString,
}

impl Render for ChatRowGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .h(px(32.0))
            .w(px(200.0))
            .px(px(Theme::SPACE_SM))
            .flex()
            .items_center()
            .rounded(px(8.0))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(crate::typography::ui_rems(13.0))
            .text_color(theme.text)
            .opacity(0.9)
            .child(div().truncate().child(self.title.clone()))
    }
}

/// Top padding above the first sidebar row; drop math measures from below it.
const SIDEBAR_LIST_TOP_INSET: f32 = 4.0;

/// Insertion slot (`0..=heights.len()`) for a pointer `y` px below the list's
/// first row: the first row whose midpoint sits below the pointer.
pub fn row_drop_slot(heights: &[f32], gap: f32, y: f32) -> usize {
    let mut top = 0.0;
    for (ix, height) in heights.iter().enumerate() {
        if y < top + height / 2.0 {
            return ix;
        }
        top += height + gap;
    }
    heights.len()
}

/// Move `chat_id` to insertion `slot` of the on-screen `visible` list and
/// write the result into `order` — the every-space list the manual sort is
/// stored against. Under a space filter `order` also holds other spaces'
/// chats, so the move anchors on the dragged row's new on-screen neighbour.
/// `None` when the drop leaves the row where it was.
pub fn reorder_manual(
    mut order: Vec<String>,
    mut visible: Vec<String>,
    chat_id: &str,
    slot: usize,
) -> Option<Vec<String>> {
    let from = visible.iter().position(|id| id == chat_id)?;
    if slot == from || slot == from + 1 {
        return None;
    }
    let id = visible.remove(from);
    let to = if slot > from { slot - 1 } else { slot };
    visible.insert(to, id.clone());
    order.retain(|other| *other != id);
    let position = |anchor: &String| order.iter().position(|other| other == anchor);
    let at = match to.checked_sub(1) {
        Some(prev) => position(&visible[prev]).map(|ix| ix + 1),
        None => visible.get(1).and_then(position),
    }
    .unwrap_or(0);
    order.insert(at, id);
    Some(order)
}

/// Height changes do not constitute a list reorder. In particular, sidebar
/// disclosures animate their own height and must not also trigger FLIP offsets
/// on every following keyed section.
fn sidebar_key_order_changed(old: &[(String, f32)], new: &[(String, f32)]) -> bool {
    old.len() != new.len()
        || old
            .iter()
            .zip(new)
            .any(|((old_key, _), (new_key, _))| old_key != new_key)
}

/// Exact active-session row height. Provider identity lives on the title line
/// and the Working glyph lives in the status corner, so neither adds a third
/// line. Once either metadata view option is on, line 3 is RESERVED on every
/// row — a chat without a branch or PR leaves its strip empty rather than
/// collapsing, so all rows share one fixed height. The 16px strip fits the
/// tallest child (the PR badge); the branch text is 14px inside it. Only with
/// both options hidden do rows go compact and omit the line entirely.
/// Keeping this calculation beside the renderer's metrics prevents disclosure
/// clips when view options alter the row structure.
pub(super) fn chat_row_height(metadata_line: bool) -> f32 {
    if metadata_line { 63.0 } else { 45.0 }
}
/// Flex gap between sidebar list items.
pub(super) const SIDEBAR_LIST_GAP: f32 = 2.0;
/// Provider/title geometry for the active multi-line cards: identity close
/// on the standard 8px rhythm.
pub(super) const SIDEBAR_ACTIVE_HARNESS_ICON_SIZE: f32 = 13.0;
pub(super) const SIDEBAR_ACTIVE_HARNESS_TITLE_GAP: f32 = Theme::SPACE_SM;

/// Ramp height of the sidebar's scroll-edge fade (the gpui
/// [`gpui::EdgeFade`] scope — per-primitive, so text fades per glyph).
pub(super) const SIDEBAR_GLASS_FADE_BAND: f32 = 24.0;

impl Shell {
    pub(super) fn render_sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // The sidebar is part of the resolved theme. A second fixed-Holt
        // palette here made imported families look split in half and froze
        // activity/glyph personality independently of the selected variant.
        let theme = Theme::of(cx).clone();
        let inner: AnyElement = match self.route {
            Route::Settings(section) => self.render_settings_nav(section, &theme, cx),
            // The Chat manager keeps the session sidebar — it manages what
            // that sidebar lists.
            Route::Chat | Route::ChatManager | Route::Scheduled => {
                self.render_chat_sidebar(&theme, cx)
            }
        };
        let target = self.sidebar_target();
        // Transparent — the sidebar sits directly on the frost shell; the main
        // card's own border provides the separation. The content row spans the
        // full window height (the titlebar overlays it), so the column pads
        // itself below the chrome.
        self.pane_container(
            self.sidebar_tween,
            target,
            div()
                .h_full()
                .pt(px(Theme::TITLEBAR_HEIGHT))
                .child(inner)
                .into_any_element(),
        )
    }

    /// Settings-mode sidebar (holt settings-sidebar.tsx): window-control
    /// strip, "Settings" heading, icon section rows styled like session rows,
    /// and a Back row pinned to the bottom.
    pub(super) fn render_settings_nav(
        &mut self,
        section: SettingsSection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let section_icon = |item: SettingsSection| match item {
            SettingsSection::Providers => icons::KEY_MINIMALISTIC,
            SettingsSection::Appearance => icons::TUNING,
            SettingsSection::Shortcuts => icons::KEYBOARD,
            SettingsSection::Skills => icons::WIDGET,
            SettingsSection::Mcp => icons::CUBE,
            SettingsSection::Usage => icons::CHART_COLUMN,
            SettingsSection::General => icons::SETTINGS_MINIMALISTIC,
            SettingsSection::Archived => icons::ARCHIVE_MINIMALISTIC,
        };
        // Match the user's dragged sidebar width — the pane container clips to
        // it, so a hardcoded default here left hover washes stopping short of
        // the sidebar's right edge (user-reported).
        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .px(px(Theme::SPACE_SM))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px(px(Theme::SPACE_SM))
                            .pt(px(12.0))
                            .pb(px(4.0))
                            .text_size(crate::typography::ui_rems(11.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from("Settings")),
                    )
                    .child(div().flex().flex_col().gap(px(2.0)).children(
                        SettingsSection::ALL.into_iter().map(|item| {
                            let selected = item == section;
                            div()
                                .id(SharedString::from(format!("settings-nav-{}", item.label())))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .rounded(px(8.0))
                                .px(px(Theme::SPACE_SM))
                                .py(px(6.0))
                                .text_size(crate::typography::ui_rems(13.0))
                                .when(selected, |el| {
                                    // Same tokens as the main sidebar's session
                                    // rows — the two sidebars must feel alike.
                                    el.bg(crate::theme::glass_selected_bg())
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                })
                                .text_color(if selected {
                                    theme.text
                                } else {
                                    theme.text_muted
                                })
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.glass_hover()).text_color(theme.text))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.open_settings(item, cx)),
                                )
                                .child(
                                    icon(section_icon(item))
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from(item.label()))
                        }),
                    )),
            )
            // Back pinned to the bottom (holt settings-sidebar.tsx).
            .child(
                div().px(px(Theme::SPACE_SM)).pb(px(12.0)).child(
                    div()
                        .id("settings-back")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(8.0))
                        .px(px(Theme::SPACE_SM))
                        .py(px(6.0))
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.glass_hover()).text_color(theme.text))
                        .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx)))
                        .child(
                            // AltArrowLeft chevron (holt settings-sidebar.tsx),
                            // not the straight history arrow.
                            icon(icons::ALT_ARROW_LEFT)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Back")),
                ),
            )
            .into_any_element()
    }

    /// One session row: context + status on line one, provider + title on line
    /// two, and source metadata below. Working uses the live thread glyph in
    /// the status corner. Click selects; right-click — or the hover "…" at
    /// the row's right-middle — opens the context menu.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_chat_row(
        &self,
        id: String,
        title: SharedString,
        time_ago: SharedString,
        space_name: SharedString,
        branch: Option<SharedString>,
        change_request: Option<holt_proto::ChangeRequestSummary>,
        provider: Option<holt_proto::ProviderId>,
        status: holt_proto::ChatIndicator,
        selected: bool,
        pinned: bool,
        // This row's jump combo while the hint overlay is up. It takes the
        // corner outright — above hover and above the status word — so all
        // nine chips appear together instead of leaving a hole on whichever
        // row is busy or under the pointer.
        jump_label: Option<SharedString>,
        // A Routine's folded run row: its run count (the row is the latest).
        routine_runs: Option<usize>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Activity, not position (t3code Sidebar): status is a small colored
        // word + glyph in the row's top-right corner — Working animates the
        // composer-strip spinner, Done wears a check; Idle rows show the
        // relative time instead. The corner never swaps on hover any more;
        // the hover affordance is the "…" at the row's right-middle.
        // Send-truth overrides: a send unadopted past the grace window is
        // FAILED (explicit, with the transcript's retry affordance); a send
        // whose delivery path is degraded is QUEUED, not Working — the
        // pending pill tells the truth instead of faking a spinner.
        let (queued, undelivered) = {
            let now = Utc::now();
            let state = self.state.read(cx);
            (
                state.send_queued(&id, now),
                state.send_undelivered(&id, now),
            )
        };
        let status_color = if undelivered {
            theme.danger
        } else if queued {
            theme.warning
        } else {
            spaces::status_dot_color(status, theme)
        };
        let status_label: Option<&'static str> = if undelivered {
            Some("Failed")
        } else if queued {
            Some("Queued")
        } else {
            match status {
                holt_proto::ChatIndicator::Working => Some("Working"),
                holt_proto::ChatIndicator::AwaitingInput => Some("Input"),
                holt_proto::ChatIndicator::Errored => Some("Failed"),
                holt_proto::ChatIndicator::Completed => Some("Done"),
                holt_proto::ChatIndicator::Idle => None,
            }
        };
        // Line 3's strip is reserved whenever EITHER metadata view option is
        // on — the caller blanks `branch`/`change_request` per the same
        // settings, so a row without data here simply leaves its strip empty
        // and every row keeps one fixed height.
        let reserve_metadata =
            self.settings.sidebar_show_branch || self.settings.sidebar_show_pull_request;
        let queued = queued && !undelivered;
        let working = status == holt_proto::ChatIndicator::Working && !queued && !undelivered;
        let corner_body: AnyElement = if let Some(label) = jump_label {
            // The jump hint replaces the status/time corner while the modifier
            // is held, cut to the sidebar PR badge's exact cloth
            // (`pull_request_badge`, Sidebar surface): pinned 16px, px 4,
            // rounded 4, borderless 0.08-fill with 0.85 text of one tone —
            // neutral here — and the label in the badge's mono at 10 MEDIUM.
            // Any other geometry reads as a second badge system on the row.
            {
                let tone = theme.text_muted;
                div()
                    .h(px(16.0))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .px(px(4.0))
                    .rounded(px(4.0))
                    .bg(tone.opacity(0.08))
                    .text_size(crate::typography::ui_rems(10.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(tone.opacity(0.85))
                    .font_family(theme.font_mono.clone())
                    .child(label)
                    .into_any_element()
            }
        } else {
            match status_label {
                Some(label) => {
                    // Glyph slot: Working wears the preset's animated pixel
                    // glyph beside its label, Done wears the check, and the
                    // remaining statuses use a compact dot.
                    let glyph: AnyElement = if status == holt_proto::ChatIndicator::Completed {
                        icon(icons::CHECK)
                            .size(px(11.0))
                            .flex_none()
                            .text_color(status_color)
                            .into_any_element()
                    } else if working {
                        loaders::mini_glyph_spinner(
                            format!("chat-working-{id}"),
                            2.0,
                            theme.glyph,
                            cx.entity_id(),
                            cx,
                        )
                        .into_any_element()
                    } else {
                        div()
                            .size(px(6.0))
                            .flex_none()
                            .rounded_full()
                            .bg(status_color)
                            .into_any_element()
                    };
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .child(glyph)
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(10.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(status_color)
                                .child(SharedString::from(label)),
                        )
                        .into_any_element()
                }
                None => div()
                    .text_size(crate::typography::ui_rems(10.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(time_ago.clone())
                    .into_any_element(),
            }
        };
        // One stable wrapper across both states (identity keeps the hover
        // from flickering as the content swaps); the swap is driven by the
        // ROW's hover (user request — corner-only felt undiscoverable), but
        // archiving only clicks on the pill itself, so the row's own click
        // stays the selector.
        let corner: AnyElement = div()
            .id(SharedString::from(format!("chat-corner-{id}")))
            .flex_none()
            // Pin the corner to line 1's text height so the archive pill
            // (taller, padded) overflows vertically instead of growing the
            // row — the swap must not shift the card's content.
            // NO occlude: the ROW's hover drives the swap, and an
            // occluding corner un-hovered the row underneath it —
            // pill mounts, steals the pointer, row un-hovers, pill
            // unmounts, repeat (user-reported flicker). The hit box's
            // stop_propagation click is separation enough.
            .h(px(14.0))
            .flex()
            .items_center()
            .child(corner_body)
            .into_any_element();
        let (hover, text) = (theme.glass_hover(), theme.text);
        let selected_wash = crate::theme::glass_selected_bg();
        let subline = theme.text_muted.opacity(0.5);
        let select_id = id.clone();
        let menu_id = id.clone();
        // Hover fades over transition-colors (holt session-row.tsx) — both
        // the wash and the title brighten ride the same 150ms blend.
        let fade_key = format!("chat-row-{id}");
        // A pinned row reads like the active one (user request): same wash,
        // same steady-state brightness — placement plus emphasis only.
        let active_like = selected || pinned;
        let rest_bg = if active_like {
            selected_wash
        } else {
            crate::theme::wash(0.0)
        };
        // A selected row must NOT drift toward the hover wash: in dark the two
        // fills are identical so the blend is a no-op, but light's hover sits
        // below its near-opaque selected fill, and blending toward it visibly
        // dimmed the active row under the pointer (user report).
        let hover_bg = if active_like { selected_wash } else { hover };
        let rest_text = if active_like { text } else { text.opacity(0.8) };
        div()
            .id(SharedString::from(format!("chat-{id}")))
            .relative()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .text_color(motion::hover_blend(&fade_key, rest_text, text))
            .bg(motion::hover_blend(&fade_key, rest_bg, hover_bg))
            // No selection ring (user request) — the wash alone marks the
            // active row.
            // Row hover drives the wash blend (one listener — gpui allows a
            // single hover listener per element); the "…" reads the same
            // fade below.
            .on_hover(motion::hover_listener(fade_key.clone()))
            // Hover "…" at the row's right-middle: the same context menu a
            // right-click opens. Its opacity rides the row's hover fade; the
            // click stops propagation so the row never activates. No
            // occlude — an occluding button would un-hover the row under
            // the pointer and flicker (see the old archive pill).
            .child({
                let more_selector = id.clone();
                let more_menu = id.clone();
                let dots_group: SharedString = format!("chat-more-hit-{id}").into();
                div()
                    .id(SharedString::from(format!("chat-more-{id}")))
                    .debug_selector(move || format!("chat-more-{more_selector}"))
                    .absolute()
                    .right(px(6.0))
                    .top(px(0.0))
                    .bottom(px(0.0))
                    .w(px(18.0))
                    .group(dots_group.clone())
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .opacity(motion::hover_t(&fade_key))
                    .on_click(
                        cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                            cx.stop_propagation();
                            let gpui::ClickEvent::Mouse(click) = event else {
                                return;
                            };
                            this.open_chat_menu(more_menu.clone(), click.up.position, window, cx);
                        }),
                    )
                    // The column is the hit target (the old pill taught us
                    // tight targets beside an open-on-click card get missed);
                    // the pill inside carries the hover paint. The icon keeps
                    // an explicit color — gpui's Svg paints only with its OWN
                    // text color, it never inherits one.
                    .child(
                        div()
                            .size(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .group_hover(dots_group.clone(), |s| s.bg(crate::theme::wash(0.12)))
                            .child(
                                icon(icons::MENU_DOTS)
                                    .size(px(12.0))
                                    .flex_none()
                                    .text_color(theme.text_muted)
                                    .group_hover(dots_group.clone(), |s| s.text_color(theme.text)),
                            ),
                    )
            })
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_chat(select_id.clone(), cx);
            }))
            .on_drag(
                ChatRowDrag {
                    chat_id: id.clone(),
                    title: title.clone(),
                },
                |payload, _point, _, cx| {
                    let title = payload.title.clone();
                    cx.stop_propagation();
                    cx.new(|_| ChatRowGhost { title })
                },
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_chat_menu(menu_id.clone(), event.position, window, cx);
                }),
            )
            // Line 1: project, status word / time-ago right.
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.0))
                            .line_height(px(14.0))
                            .text_color(subline)
                            .child(space_name),
                    )
                    .child(div().text_color(subline).child(corner)),
            )
            // Line 2: the front identity slot rides with the title instead
            // of floating as unrelated metadata below it. Pinned rows front
            // the pin itself (glossary "Pinned (a chat)"); unpinned rows
            // keep the provider brand.
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(SIDEBAR_ACTIVE_HARNESS_TITLE_GAP))
                    .when(pinned, |el| {
                        let pin_selector = id.clone();
                        el.child(
                            div()
                                .id(SharedString::from(format!("chat-pin-{id}")))
                                .debug_selector(move || format!("chat-pin-{pin_selector}"))
                                .flex_none()
                                .child(
                                    icon(icons::PIN)
                                        .size(px(SIDEBAR_ACTIVE_HARNESS_ICON_SIZE))
                                        .text_color(theme.text_muted),
                                ),
                        )
                    })
                    .when(!pinned && routine_runs.is_some(), |el| {
                        el.child(
                            icon(icons::CLOCK_CIRCLE)
                                .size(px(SIDEBAR_ACTIVE_HARNESS_ICON_SIZE))
                                .flex_none()
                                .text_color(subline.opacity(0.8)),
                        )
                    })
                    .when(!pinned && routine_runs.is_none(), |el| {
                        el.when_some(
                            provider
                                .as_ref()
                                .and_then(crate::pickers::provider_brand_icon),
                            |el, mark| {
                                el.child(mark.render(
                                    px(SIDEBAR_ACTIVE_HARNESS_ICON_SIZE),
                                    subline.opacity(0.8),
                                ))
                            },
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            // The hover-revealed "…" menu button floats
                            // over the row's right edge (right 6 + 18 hit
                            // target): the title truncates clear of it —
                            // or the run count does.
                            .when(!routine_runs.is_some_and(|runs| runs > 1), |el| {
                                el.pr(px(24.0))
                            })
                            .text_size(crate::typography::ui_rems(13.0))
                            .line_height(px(17.0))
                            .child(title),
                    )
                    .when_some(routine_runs.filter(|runs| *runs > 1), |el, runs| {
                        let count_selector = id.clone();
                        el.child(
                            div()
                                .id(SharedString::from(format!("chat-runs-{id}")))
                                .debug_selector(move || format!("chat-runs-{count_selector}"))
                                .flex_none()
                                // Clear of the hover "…" like the title.
                                .mr(px(24.0))
                                .text_size(crate::typography::ui_rems(11.0))
                                .text_color(subline)
                                .child(SharedString::from(format!("\u{d7}{runs}"))),
                        )
                    }),
            )
            // Line 3 is reserved whitespace once either metadata view option
            // is on (fixed-height rows), pinned to the PR badge's 16px; it is
            // omitted only when both options are hidden.
            .when(reserve_metadata, |row| {
                row.child(
                    div()
                        .w_full()
                        .h(px(16.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .when_some(branch, |el, branch| {
                            el.child(
                                icon(icons::GIT_BRANCH)
                                    .size(px(11.0))
                                    .flex_none()
                                    .text_color(subline),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .line_height(px(14.0))
                                    .text_color(subline)
                                    .child(branch),
                            )
                        })
                        // Stable invisible spring keeps the optional PR badge
                        // pinned right without changing no-PR paint.
                        .child(div().flex_1().min_w_0())
                        .when_some(change_request, |el, summary| {
                            el.child(crate::change_requests::pull_request_badge(
                                format!("chat-pr-{id}").into(),
                                summary,
                                crate::change_requests::ChangeRequestBadgeSurface::Sidebar,
                                theme,
                            ))
                        }),
                )
            })
            .into_any_element()
    }

    /// Chat-mode sidebar (spaces overhaul): window-control strip, the Spaces
    /// section (folder rows, add-space), the global Active sessions
    /// list, the notice strip, and the UserMenu (§1.6).
    /// The global connection line. `None` while healthy (`Connected`) or on
    /// local profiles (`Disabled`) — and the engine's degrade grace means it
    /// only exists during REAL outages, never join/wake blips. No surface,
    /// no border (v0.2.12 feedback): a bare spinner + faint caption while
    /// reconnecting; an amber dot only when the OS says offline. The
    /// transport error belongs in logs, not the sidebar.
    pub(super) fn render_connection_pill(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        use holt_proto::ConnectivityState as S;
        let conn = self.state.read(cx).connectivity.clone();
        let (label, glyph): (SharedString, AnyElement) = match conn.state {
            S::Disabled | S::Connected => return None,
            S::Offline => (
                "Offline — sends are saved".into(),
                div()
                    .size(px(5.0))
                    .rounded_full()
                    .bg(theme.warning)
                    .into_any_element(),
            ),
            S::Reconnecting => (
                "Reconnecting…".into(),
                loaders::mini_mono_spinner(
                    "connection-spinner",
                    2.0,
                    theme.text_muted,
                    cx.entity_id(),
                    cx,
                )
                .into_any_element(),
            ),
        };
        Some(
            crate::motion::fade_in(
                "connection-pill",
                div()
                    .id("connection-pill")
                    .mx(px(Theme::SPACE_SM + 4.0))
                    .mb(px(Theme::SPACE_SM))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(glyph)
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_faint)
                            .child(label),
                    ),
            )
            .into_any_element(),
        )
    }

    pub(super) fn render_chat_sidebar(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Keyed rows: (stable key, estimated height, element) — the key + height
        // list drives the §1.6 resort FLIP diff below (attention-bucket
        // promotions glide; cleared rows just go).
        let keyed: Vec<(String, f32, AnyElement)> = self.render_active_rows(theme, cx);

        // A drag released off the list never reaches `on_drop`.
        if self.sidebar_drag.is_some() && !cx.has_active_drag() {
            self.sidebar_drag = None;
        }
        // (chat id, height, pinned) per row, top to bottom — the drop math's
        // geometry, and pinned rows only reorder among themselves.
        let drag_rows: std::rc::Rc<Vec<(String, f32, bool)>> = {
            let state = self.state.read(cx);
            std::rc::Rc::new(
                keyed
                    .iter()
                    .filter_map(|(key, height, _)| {
                        let id = key.strip_prefix("c:")?;
                        let pinned = state.chat_row(id).is_some_and(|chat| chat.pinned);
                        Some((id.to_string(), *height, pinned))
                    })
                    .collect(),
            )
        };
        let dragging = self
            .sidebar_drag
            .as_ref()
            .map(|d| format!("c:{}", d.chat_id));
        // The insertion line's y, hidden on slots that would leave the row
        // where it is.
        let drop_line_y = self.sidebar_drag.as_ref().and_then(|drag| {
            let slot = drag.slot?;
            let from = drag_rows
                .iter()
                .position(|(id, _, _)| *id == drag.chat_id)?;
            if slot == from || slot == from + 1 {
                return None;
            }
            let above: f32 = drag_rows[..slot].iter().map(|(_, h, _)| h).sum();
            Some(above + SIDEBAR_LIST_GAP * slot as f32 - SIDEBAR_LIST_GAP / 2.0)
        });

        // Resort glide (§1.6 View Transitions parity): when the ORDER of a live
        // list changes (new activity resort, grouping flip), surviving rows
        // glide from their old y to the new one — layout is already at the new
        // position; the offset is a paint-only relative inset animated to 0
        // over 260ms cubic-bezier(0.22,1,0.36,1). New rows fade in; removals
        // just go (matching the original). First fill and chat switches (which
        // don't reorder) never animate.
        let order: Vec<(String, f32)> = keyed.iter().map(|(k, h, _)| (k.clone(), *h)).collect();
        if self.sidebar_prev_order != order {
            let key_order_changed = sidebar_key_order_changed(&self.sidebar_prev_order, &order);
            if !self.sidebar_prev_order.is_empty() {
                // A disclosure already animates its own body height. Applying
                // FLIP offsets when only keyed heights change double-counts
                // that movement, leaving gaps and momentary overlaps between
                // the first group, following groups, and Archived.
                let offsets = if key_order_changed {
                    resort_offsets(&self.sidebar_prev_order, &order, SIDEBAR_LIST_GAP)
                } else {
                    std::collections::HashMap::new()
                };
                let prev_keys: std::collections::HashSet<&str> = self
                    .sidebar_prev_order
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect();
                let new_keys: std::collections::HashSet<String> = order
                    .iter()
                    .filter(|(k, _)| !prev_keys.contains(k.as_str()))
                    .map(|(k, _)| k.clone())
                    .collect();
                if key_order_changed && (!offsets.is_empty() || !new_keys.is_empty()) {
                    self.resort_epoch += 1;
                    self.sidebar_resort = offsets;
                    self.sidebar_new_keys = new_keys;
                }
            }
            self.sidebar_prev_order = order;
        }
        let epoch = self.resort_epoch;
        let order_keys: Vec<String> = keyed.iter().map(|(key, _, _)| key.clone()).collect();
        let list_items: Vec<AnyElement> = keyed
            .into_iter()
            .map(|(key, _, element)| {
                if let Some(dy) = self.sidebar_resort.get(&key).copied() {
                    let id = SharedString::from(format!("resort-{epoch}-{key}"));
                    div()
                        .child(element)
                        .with_animation(id, RESORT.animation(), move |el, t| {
                            el.relative().top(px(dy * (1.0 - t)))
                        })
                        .into_any_element()
                } else if self.sidebar_new_keys.contains(&key) {
                    let id = SharedString::from(format!("row-in-{epoch}-{key}"));
                    motion::fade_quick(id, div().child(element)).into_any_element()
                } else {
                    element
                }
            })
            .zip(order_keys)
            .map(|(element, key)| {
                if dragging.as_deref() == Some(key.as_str()) {
                    div().opacity(0.4).child(element).into_any_element()
                } else {
                    element
                }
            })
            .collect();

        // Bottom-of-sidebar settings entry: same row recipe as the settings
        // sidebar's Back row (px-8/py-6, rounded-8, 13px) so the two feel
        // identical.
        let settings_row = self.render_sidebar_settings_row(theme, cx);

        // The space filter lives ABOVE the scroll region (fixed) so its
        // dropdown can float without being clipped by the list's overflow.
        let filter_row = self.render_spaces_filter(theme, cx);

        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            // (No titlebar strip: the unified window titlebar spans the whole
            // window above this column.)
            .child(filter_row)
            // The (filtered) Sessions list scrolls inside an EdgeFade scope —
            // a true per-glyph gradient at active overflow edges. Glass-safe
            // (no painted overlay can fade content over see-through blur) and
            // equivalent on opaque themes: alpha→0 reveals the surface tone
            // underneath, same as the gradient overlays it replaced. Overflow
            // is read at PAINT time via the scroll handle — render-time gating
            // rode the previous frame's offset, so the last frame of a content
            // shrink (row archived while scrolled) left a phantom fade stuck
            // over an unscrollable list (user report).
            .child(
                crate::edge_fade::edge_faded(
                    SIDEBAR_GLASS_FADE_BAND,
                    true,
                    true,
                    div().relative().flex_1().min_h_0().child(
                        div()
                            .id("sidebar-lists")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.sidebar_scroll)
                            .px(px(Theme::SPACE_SM))
                            .flex()
                            .flex_col()
                            // No "Sessions" header (user request) — the list
                            // is the whole column; a little air stands in.
                            .pt(px(SIDEBAR_LIST_TOP_INSET))
                            // The scroller (not the row column) is the drop
                            // target so the empty space under the last row
                            // still takes a drop; y reads in content
                            // coordinates (offset.y is negative when scrolled).
                            .on_drag_move::<ChatRowDrag>(cx.listener(
                                move |this, event: &gpui::DragMoveEvent<ChatRowDrag>, _, cx| {
                                    let chat_id = &event.drag(cx).chat_id;
                                    let pointer = event.event.position;
                                    match &mut this.sidebar_drag {
                                        Some(drag) if drag.chat_id == *chat_id => {
                                            drag.pointer = pointer;
                                            drag.rows = drag_rows.clone();
                                        }
                                        _ => {
                                            this.sidebar_drag = Some(SidebarRowDragState {
                                                chat_id: chat_id.clone(),
                                                slot: None,
                                                pointer,
                                                rows: drag_rows.clone(),
                                                scroll_task: None,
                                            })
                                        }
                                    }
                                    this.refresh_sidebar_drag_slot(cx);
                                    this.schedule_sidebar_drag_scroll(cx);
                                },
                            ))
                            .on_drop(cx.listener(|this, payload: &ChatRowDrag, _, cx| {
                                let slot = this.sidebar_drag.take().and_then(|d| d.slot);
                                if let Some(slot) = slot {
                                    this.drop_sidebar_row(&payload.chat_id, slot, cx);
                                }
                                cx.notify();
                            }))
                            .child(if !list_items.is_empty() {
                                div()
                                    .relative()
                                    .flex()
                                    .flex_col()
                                    .gap(px(SIDEBAR_LIST_GAP))
                                    .children(list_items)
                                    .when_some(drop_line_y, |el, y| {
                                        el.child(
                                            div()
                                                .absolute()
                                                .left(px(4.0))
                                                .right(px(4.0))
                                                .top(px(y - 1.0))
                                                .h(px(2.0))
                                                .rounded(px(1.0))
                                                .bg(theme.accent),
                                        )
                                    })
                                    .into_any_element()
                            } else {
                                div()
                                    .px(px(Theme::SPACE_SM))
                                    .pb(px(Theme::SPACE_SM))
                                    .text_size(crate::typography::ui_rems(12.0))
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from("No sessions yet"))
                                    .into_any_element()
                            }),
                    ),
                )
                .fade_overflow_y(&self.sidebar_scroll),
            )
            // Global connection pill (durable-by-design UI truth): appears
            // whenever the edge posture is degraded; hidden while healthy —
            // appearing IS the signal.
            .when_some(self.render_connection_pill(theme, cx), |el, pill| {
                el.child(pill)
            })
            // Holt notices are rendered as top-right overlays in
            // `render_overlays`, so the sidebar column itself stays a pure
            // nav surface with no in-flow feedback strip.
            .child(
                div()
                    .px(px(Theme::SPACE_SM))
                    .pb(px(12.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .child(div().flex_1().child(settings_row))
                    .child(self.render_chat_manager_button(theme, cx))
                    .when_some(self.render_update_button(theme, cx), |el, button| {
                        el.child(button)
                    }),
            )
            .into_any_element()
    }

    /// Re-derive the drop slot from the pointer and the scroller's live
    /// bounds/offset — after a pointer move or an autoscroll step. Content
    /// y: offset.y is negative when scrolled.
    fn refresh_sidebar_drag_slot(&mut self, cx: &mut Context<Self>) {
        let bounds = self.sidebar_scroll.bounds();
        let offset_y = f32::from(self.sidebar_scroll.offset().y);
        let Some(drag) = self.sidebar_drag.as_mut() else {
            return;
        };
        let slot = bounds
            .contains(&drag.pointer)
            .then(|| {
                let pinned = drag.rows.iter().find(|(id, _, _)| *id == drag.chat_id)?.2;
                let y = f32::from(drag.pointer.y)
                    - f32::from(bounds.top())
                    - offset_y
                    - SIDEBAR_LIST_TOP_INSET;
                let heights: Vec<f32> = drag.rows.iter().map(|(_, h, _)| *h).collect();
                let pinned_count = drag.rows.iter().take_while(|row| row.2).count();
                let (lo, hi) = if pinned {
                    (0, pinned_count)
                } else {
                    (pinned_count, drag.rows.len())
                };
                Some(row_drop_slot(&heights, SIDEBAR_LIST_GAP, y).clamp(lo, hi))
            })
            .flatten();
        if drag.slot != slot {
            drag.slot = slot;
            cx.notify();
        }
    }

    /// Edge autoscroll while a row drags: the transcript's selection ramp
    /// (`selection_scroll_step`), ticking until the pointer leaves the edge
    /// band, the list hits its end, or the drag ends.
    fn schedule_sidebar_drag_scroll(&mut self, cx: &mut Context<Self>) {
        let bounds = self.sidebar_scroll.bounds();
        let Some(drag) = self.sidebar_drag.as_mut() else {
            return;
        };
        let pointer = drag.pointer;
        let over_column = pointer.x >= bounds.left() && pointer.x <= bounds.right();
        if drag.scroll_task.is_some()
            || !over_column
            || crate::transcript::selection_scroll_step(bounds, pointer) == 0.0
        {
            return;
        }
        drag.scroll_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(
                    crate::transcript::SELECTION_SCROLL_TICK_MS,
                ))
                .await;
            let _ = this.update(cx, |this, cx| this.step_sidebar_drag_scroll(cx));
        }));
    }

    fn step_sidebar_drag_scroll(&mut self, cx: &mut Context<Self>) {
        if !cx.has_active_drag() {
            self.sidebar_drag = None;
            cx.notify();
            return;
        }
        let Some(drag) = self.sidebar_drag.as_mut() else {
            return;
        };
        drag.scroll_task = None;
        let step =
            crate::transcript::selection_scroll_step(self.sidebar_scroll.bounds(), drag.pointer);
        let mut offset = self.sidebar_scroll.offset();
        let max = f32::from(self.sidebar_scroll.max_offset().y);
        let next = (f32::from(offset.y) - step).clamp(-max, 0.0);
        if next == f32::from(offset.y) {
            return;
        }
        offset.y = px(next);
        self.sidebar_scroll.set_offset(offset);
        self.refresh_sidebar_drag_slot(cx);
        cx.notify();
        self.schedule_sidebar_drag_scroll(cx);
    }

    /// Commit a session-row drop: the on-screen move lands in the stored
    /// manual order, and the sidebar switches to Manual sort.
    fn drop_sidebar_row(&mut self, chat_id: &str, slot: usize, cx: &mut Context<Self>) {
        let Some(order) = reorder_manual(
            self.sidebar_order_for(None, cx),
            self.sidebar_visible_order(cx),
            chat_id,
            slot,
        ) else {
            return;
        };
        self.settings.sidebar_order = order;
        self.settings.sidebar_sort = SidebarSort::Manual;
        self.schedule_save(cx);
    }

    /// The Chat manager entry, sitting right of the settings row: an icon
    /// button wearing the selected wash while the manager page is up.
    pub(super) fn render_chat_manager_button(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = matches!(self.route, Route::ChatManager);
        div()
            .id("sidebar-chat-manager")
            .flex_none()
            .size(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .when(selected, |el| el.bg(crate::theme::glass_selected_bg()))
            .hover(|style| style.bg(theme.glass_hover()))
            .on_click(cx.listener(|this, _, _, cx| this.open_chat_manager(cx)))
            .tooltip(|_, cx| {
                cx.new(|_| crate::popover::TextTooltip("Chat manager".into()))
                    .into()
            })
            .tooltip_show_delay(std::time::Duration::from_millis(350))
            // gpui's Svg paints only with its OWN text color, never inherits.
            .child(icon(icons::INBOX).size(px(15.0)).text_color(if selected {
                theme.text
            } else {
                theme.text_muted
            }))
            .into_any_element()
    }

    /// Bottom-of-sidebar settings entry: a bare row (gear + label) that opens
    /// the settings page directly. Styled exactly like the settings sidebar's
    /// Back row — same padding, height, and hover.
    pub(super) fn render_sidebar_settings_row(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id("sidebar-settings")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .text_size(crate::typography::ui_rems(13.0))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|style| style.bg(theme.glass_hover()).text_color(theme.text))
            .on_click(
                cx.listener(|this, _, _, cx| this.open_settings(SettingsSection::General, cx)),
            )
            .child(
                icon(icons::SETTINGS_MINIMALISTIC)
                    .size(px(16.0))
                    .text_color(theme.text_muted),
            )
            .child(SharedString::from("Settings"))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- sidebar resort FLIP diff (§1.6) ----

    fn keys(list: &[(&str, f32)]) -> Vec<(String, f32)> {
        list.iter().map(|(k, h)| (k.to_string(), *h)).collect()
    }

    #[test]
    fn sidebar_chat_height_reserves_the_metadata_line() {
        // Fixed height while either metadata view option is on: a branch
        // (14px) or a PR badge (16px) lands in the reserved strip instead of
        // growing its own row.
        assert_eq!(chat_row_height(false), 45.0);
        assert_eq!(chat_row_height(true), 63.0);
    }

    #[test]
    fn sidebar_provider_geometry_reflects_row_hierarchy() {
        assert_eq!(SIDEBAR_ACTIVE_HARNESS_TITLE_GAP, Theme::SPACE_SM);
    }

    #[test]
    fn sidebar_height_change_is_not_a_reorder() {
        let open = keys(&[("first-group", 105.0), ("second-group", 240.0)]);
        let collapsed = keys(&[("first-group", 40.0), ("second-group", 240.0)]);
        assert!(!sidebar_key_order_changed(&open, &collapsed));

        let reordered = keys(&[("second-group", 240.0), ("first-group", 40.0)]);
        assert!(sidebar_key_order_changed(&collapsed, &reordered));
    }

    #[test]
    fn resort_offsets_empty_when_order_unchanged() {
        let order = keys(&[("a", 29.0), ("b", 29.0), ("c", 45.0)]);
        assert!(resort_offsets(&order, &order, 2.0).is_empty());
    }

    #[test]
    fn resort_offsets_activity_moves_row_to_top() {
        // c (bottom, y=62) jumps to top: c glides down-from-above? No — c's
        // old y is 62, new y is 0 → starts +62 below… offset = old - new = +62,
        // painted at +62 decaying to 0 (a glide UP into place). a and b shift
        // down by c's height + gap (31).
        let old = keys(&[("a", 29.0), ("b", 29.0), ("c", 29.0)]);
        let new = keys(&[("c", 29.0), ("a", 29.0), ("b", 29.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        assert_eq!(offsets.get("c"), Some(&62.0));
        assert_eq!(offsets.get("a"), Some(&-31.0));
        assert_eq!(offsets.get("b"), Some(&-31.0));
    }

    #[test]
    fn resort_offsets_respect_heights_and_gap() {
        // Tall row (45px) swaps with a short one (29px).
        let old = keys(&[("tall", 45.0), ("short", 29.0)]);
        let new = keys(&[("short", 29.0), ("tall", 45.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        // short: old y 47 → new y 0; tall: old y 0 → new y 31.
        assert_eq!(offsets.get("short"), Some(&47.0));
        assert_eq!(offsets.get("tall"), Some(&-31.0));
    }

    #[test]
    fn resort_offsets_ignore_added_and_removed_keys() {
        let old = keys(&[("a", 29.0), ("gone", 29.0), ("b", 29.0)]);
        let new = keys(&[("new", 29.0), ("a", 29.0), ("b", 29.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        // "new" has no old position (fades in instead); "gone" just goes.
        assert!(!offsets.contains_key("new"));
        assert!(!offsets.contains_key("gone"));
        // a: old 0 → new 31 (pushed down by the insert); b: 62 → 62 (gone's
        // slot replaced by "new" of equal height — no move, no entry).
        assert_eq!(offsets.get("a"), Some(&-31.0));
        assert_eq!(offsets.get("b"), None);
    }

    #[test]
    fn resort_glide_spec_matches_original() {
        // §1.6: 260ms cubic-bezier(0.22, 1, 0.36, 1).
        assert_eq!(RESORT.duration_ms, 260);
        assert_eq!(RESORT.curve, motion::EASE_RESORT);
    }

    // ---- sidebar manual reorder ----

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn drop_slot_splits_rows_at_their_midpoints() {
        let heights = [45.0, 61.0, 45.0];
        assert_eq!(row_drop_slot(&heights, 2.0, -10.0), 0);
        assert_eq!(row_drop_slot(&heights, 2.0, 22.0), 0);
        assert_eq!(row_drop_slot(&heights, 2.0, 23.0), 1);
        // Row 1 spans 47..108, midpoint 77.5.
        assert_eq!(row_drop_slot(&heights, 2.0, 77.0), 1);
        assert_eq!(row_drop_slot(&heights, 2.0, 78.0), 2);
        assert_eq!(row_drop_slot(&heights, 2.0, 500.0), 3);
    }

    #[test]
    fn reorder_moves_down_and_up() {
        let list = ids(&["a", "b", "c", "d"]);
        assert_eq!(
            reorder_manual(list.clone(), list.clone(), "a", 3),
            Some(ids(&["b", "c", "a", "d"]))
        );
        assert_eq!(
            reorder_manual(list.clone(), list.clone(), "d", 0),
            Some(ids(&["d", "a", "b", "c"]))
        );
        assert_eq!(
            reorder_manual(list.clone(), list.clone(), "b", 4),
            Some(ids(&["a", "c", "d", "b"]))
        );
    }

    #[test]
    fn reorder_on_either_edge_of_the_row_is_a_no_op() {
        let list = ids(&["a", "b", "c"]);
        assert_eq!(reorder_manual(list.clone(), list.clone(), "b", 1), None);
        assert_eq!(reorder_manual(list.clone(), list.clone(), "b", 2), None);
    }

    #[test]
    fn filtered_reorder_keeps_other_spaces_in_place() {
        // x* belong to another space and are filtered off screen.
        let order = ids(&["a", "x1", "b", "x2", "c"]);
        let visible = ids(&["a", "b", "c"]);
        // c to the top: anchors before a.
        assert_eq!(
            reorder_manual(order.clone(), visible.clone(), "c", 0),
            Some(ids(&["c", "a", "x1", "b", "x2"]))
        );
        // a between b and c: anchors after b.
        assert_eq!(
            reorder_manual(order, visible, "a", 2),
            Some(ids(&["x1", "b", "a", "x2", "c"]))
        );
    }
}
