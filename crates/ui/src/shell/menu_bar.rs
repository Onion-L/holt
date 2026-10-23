//! The Windows menu bar: File/Edit/View/Help dropdowns in the dedicated
//! titlebar strip (the Kimi-desktop layout). macOS uses the native menu bar
//! (`crate::app_menus`) and Linux keeps the overlay cluster — nothing here
//! renders off Windows. Every row dispatches an EXISTING action: shell
//! actions bubble to the root's `on_action` handlers, the app-level verbs
//! (Quit, Appearance, Zoom) land on the global handlers `app_menus::init`
//! registers, and the Edit verbs reach the focused composer input.
//!
//! Focus: opening a menu moves focus to its card (the keyboard dispatch
//! path); the previously focused element is remembered and restored before a
//! row dispatches, so Edit's Cut/Copy/Paste land on the composer again.

use super::*;
use crate::settings::{ShortcutId, display_combo};

/// The top-level menus, in strip order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum MenuId {
    File,
    Edit,
    View,
    Help,
}

const MENUS: [MenuId; 4] = [MenuId::File, MenuId::Edit, MenuId::View, MenuId::Help];

impl MenuId {
    fn label(self) -> &'static str {
        match self {
            MenuId::File => "File",
            MenuId::Edit => "Edit",
            MenuId::View => "View",
            MenuId::Help => "Help",
        }
    }
}

/// One menu row. `shortcut` is the DISPLAY string ("Ctrl+Z"); a customizable
/// shortcut reads the live keymap, so a user-cleared binding shows no hint.
pub(super) enum MenuRow {
    Item {
        label: &'static str,
        shortcut: Option<String>,
        action: Box<dyn Action>,
    },
    Separator,
}

impl MenuRow {
    fn item(label: &'static str, shortcut: Option<String>, action: impl Action) -> Self {
        MenuRow::Item {
            label,
            shortcut,
            action: Box::new(action),
        }
    }
}

/// The open menu's state, held by `Shell::menu_bar`.
pub(super) struct MenuBarState {
    /// Which menu is up.
    menu: MenuId,
    /// Keyboard highlight — an index into the menu's rows. `None` until ↑↓
    /// is pressed, so no row wears a wash on open without a hover.
    active: Option<usize>,
    /// The card's focus handle — the keyboard dispatch path while open.
    focus: gpui::FocusHandle,
    /// Focus captured when the bar opened; restored before a row dispatches.
    prior_focus: Option<gpui::FocusHandle>,
}

/// One menu's rows, freshly boxed per call (actions are not `Clone` beyond
/// `boxed_clone`). Pure data — the test walks it.
fn menu_rows(menu: MenuId, keymap: &crate::settings::KeymapConfig) -> Vec<MenuRow> {
    use crate::app_menus;
    // A customizable shortcut shows the live combo. An empty combo means the
    // DEFAULT binding is live for these ids (`apply_keymap`'s
    // `valid_or_default` falls back), so the hint falls back too.
    let custom = |id: ShortcutId| {
        let combo = keymap.get(id);
        let combo = if combo.is_empty() {
            id.default_combo()
        } else {
            combo
        };
        Some(display_combo(combo))
    };
    let fixed = |text: &str| Some(text.to_string());
    match menu {
        MenuId::File => vec![
            MenuRow::item("New Session", custom(ShortcutId::NewSession), NewSession),
            MenuRow::item("Settings…", fixed("Ctrl+,"), OpenSettings),
            MenuRow::Separator,
            // The shell's own action: the Exit row must pass the lifecycle
            // gate (unsaved drafts, running terminals) the way the caption
            // button does, so it never dispatches `app_menus::Quit` — on
            // Windows `cx.quit()` exits without asking.
            MenuRow::item("Exit", None, QuitApp),
        ],
        MenuId::Edit => vec![
            MenuRow::item("Undo", fixed("Ctrl+Z"), crate::composer::Undo),
            MenuRow::item("Redo", fixed("Ctrl+Shift+Z"), crate::composer::Redo),
            MenuRow::Separator,
            MenuRow::item("Cut", fixed("Ctrl+X"), crate::composer::Cut),
            MenuRow::item("Copy", fixed("Ctrl+C"), crate::composer::Copy),
            MenuRow::item("Paste", fixed("Ctrl+V"), crate::composer::Paste),
            MenuRow::Separator,
            MenuRow::item("Select All", fixed("Ctrl+A"), crate::composer::SelectAll),
        ],
        MenuId::View => vec![
            MenuRow::item("Appearance: System", None, app_menus::AppearanceSystem),
            MenuRow::item("Appearance: Light", None, app_menus::AppearanceLight),
            MenuRow::item("Appearance: Dark", None, app_menus::AppearanceDark),
            MenuRow::Separator,
            MenuRow::item(
                "Toggle Sidebar",
                custom(ShortcutId::ToggleSidebar),
                ToggleSidebar,
            ),
            MenuRow::item(
                "Toggle Terminal",
                custom(ShortcutId::ToggleTerminal),
                crate::terminal::panel::ToggleTerminal,
            ),
            MenuRow::item(
                "Toggle Changes",
                custom(ShortcutId::ToggleChanges),
                ToggleChanges,
            ),
            MenuRow::Separator,
            MenuRow::item("Zoom In", fixed("Ctrl+="), app_menus::ZoomIn),
            MenuRow::item("Zoom Out", fixed("Ctrl+-"), app_menus::ZoomOut),
        ],
        MenuId::Help => vec![MenuRow::item("About Holt", None, app_menus::About)],
    }
}

/// Row indices that can be highlighted and activated (separators skip).
fn activatable_rows(menu: MenuId, keymap: &crate::settings::KeymapConfig) -> Vec<usize> {
    menu_rows(menu, keymap)
        .iter()
        .enumerate()
        .filter_map(|(ix, row)| matches!(row, MenuRow::Item { .. }).then_some(ix))
        .collect()
}

impl Shell {
    /// The trigger row: one text button per menu. While any menu is open,
    /// hovering a neighbour switches to it (classic menubar behaviour).
    pub(super) fn render_menu_bar(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let triggers = MENUS
            .into_iter()
            .map(|menu| self.render_menu_trigger(menu, theme, cx))
            .collect::<Vec<_>>();
        div()
            .flex_none()
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .children(triggers)
            .into_any_element()
    }

    fn render_menu_trigger(
        &mut self,
        menu: MenuId,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self
            .menu_bar
            .as_open()
            .is_some_and(|state| state.menu == menu);
        let fade_key = format!("menu-bar-{}", menu.label());
        let hover_fade = motion::hover_listener(fade_key.clone());
        let mut trigger = div()
            .id(format!("menu-bar-{}", menu.label()))
            .h_full()
            .flex_none()
            .flex()
            .items_center()
            .px(px(10.0))
            .cursor_pointer()
            .text_size(crate::typography::ui_rems(12.5))
            .text_color(if open { theme.text } else { theme.text_muted })
            .bg(if open {
                theme.glass_hover()
            } else {
                motion::hover_blend(
                    &fade_key,
                    theme.glass_hover().opacity(0.0),
                    theme.glass_hover(),
                )
            })
            .on_hover(cx.listener(move |this, hovered, window, cx| {
                hover_fade(hovered, window, cx);
                if *hovered {
                    this.switch_menu_bar(menu, window, cx);
                }
            }))
            // Same occlusion + click-swallowing as the window-control
            // buttons: the trigger sits inside the strip's drag region, so
            // its rect is carved out of the drag/zoom surface.
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, _| {
                    window.prevent_default();
                    this.menu_bar
                        .note_trigger_press_matching(|state| state.menu == menu);
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                // A press that found THIS menu open closes it (the card's
                // mouse-down-out already began the close) — never reopen.
                if this.menu_bar.take_press_was_open() {
                    this.close_menu_bar(cx);
                } else {
                    this.open_menu_bar(menu, window, cx);
                }
            }))
            .child(menu.label());

        // The card stays mounted through its exit animation, still anchored
        // to its own trigger.
        if self.menu_bar.get().is_some_and(|state| state.menu == menu) {
            let closing = self.menu_bar.closing_since();
            let card = self.render_menu_card(theme, cx);
            trigger = trigger.relative().child(popover::anchored_menu_below_gap(
                format!("menu-bar-card-{}", menu.label()),
                card,
                closing,
                4.0,
            ));
        }
        trigger.into_any_element()
    }

    /// The dropdown card: one row per entry, separators between groups.
    fn render_menu_card(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some((menu, active, focus)) = self
            .menu_bar
            .get()
            .map(|state| (state.menu, state.active, state.focus.clone()))
        else {
            return div().into_any_element();
        };
        let rows = menu_rows(menu, &self.settings.keymap)
            .into_iter()
            .enumerate()
            .map(|(ix, row)| match row {
                MenuRow::Separator => popover::menu_separator().into_any_element(),
                MenuRow::Item {
                    label, shortcut, ..
                } => popover::menu_row_nav(
                    theme,
                    false,
                    active == Some(ix),
                    format!("menu-bar-row-{}-{ix}", menu.label()),
                )
                .id(("menu-bar-row", ix))
                .debug_selector(move || format!("menu-bar-row-{}-{ix}", menu.label()))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.activate_menu_bar_row(ix, window, cx);
                }))
                .child(div().flex_1().min_w_0().truncate().child(label))
                .when_some(shortcut, |el, shortcut| {
                    el.child(
                        div()
                            .flex_none()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_faint)
                            .child(shortcut),
                    )
                })
                .into_any_element(),
            })
            .collect::<Vec<_>>();

        popover::popover_card(theme)
            .w(px(220.0))
            .track_focus(&focus)
            .on_key_down(
                cx.listener(|this, event, window, cx| this.menu_bar_key(event, window, cx)),
            )
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_menu_bar(cx)))
            .child(div().flex().flex_col().gap(px(2.0)).children(rows))
            .into_any_element()
    }

    fn open_menu_bar(&mut self, menu: MenuId, window: &mut Window, cx: &mut Context<Self>) {
        // The card takes focus for keyboard navigation; remember what held
        // it so an activated row dispatches back there (the Edit verbs).
        let prior_focus = window.focused(cx);
        let focus = cx.focus_handle();
        self.menu_bar.open(MenuBarState {
            menu,
            active: None,
            focus: focus.clone(),
            prior_focus,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Hover-switch between menus while one is open. The ORIGINAL prior
    /// focus is kept across the switch — overwriting it with the outgoing
    /// card's handle would dead-end the Edit dispatch.
    fn switch_menu_bar(&mut self, menu: MenuId, window: &mut Window, cx: &mut Context<Self>) {
        let prior_focus = match self.menu_bar.as_open() {
            Some(state) if state.menu != menu => state.prior_focus.clone(),
            _ => return,
        };
        let focus = cx.focus_handle();
        self.menu_bar.open(MenuBarState {
            menu,
            active: None,
            focus: focus.clone(),
            prior_focus,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Close the menu through the exit animation (no-op when it isn't open).
    pub(super) fn close_menu_bar(&mut self, cx: &mut Context<Self>) {
        if self.menu_bar.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.menu_bar);
            cx.notify();
        }
    }

    fn activate_menu_bar_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((menu, prior_focus)) = self
            .menu_bar
            .as_open()
            .map(|state| (state.menu, state.prior_focus.clone()))
        else {
            return;
        };
        let rows = menu_rows(menu, &self.settings.keymap);
        let action = match rows.get(row) {
            Some(MenuRow::Item { action, .. }) => action.boxed_clone(),
            _ => return,
        };
        self.close_menu_bar(cx);
        // Restore focus BEFORE dispatching: `Window::dispatch_action`
        // captures the focused node up front (deferring only the dispatch
        // itself), so the Edit verbs land on the composer again.
        if let Some(focus) = prior_focus {
            window.focus(&focus, cx);
        }
        window.dispatch_action(action, cx);
    }

    /// Card keys: ↑↓ navigate the activatable rows, ⏎ dispatches the
    /// highlighted one, esc closes.
    fn menu_bar_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The card stays mounted (and focused) through its exit animation —
        // keys must not drive a dying card.
        if !self.menu_bar.is_open() {
            return;
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => self.close_menu_bar(cx),
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                let Some(menu) = self.menu_bar.as_open().map(|state| state.menu) else {
                    return;
                };
                let items = activatable_rows(menu, &self.settings.keymap);
                if let Some(state) = self.menu_bar.open_mut() {
                    // `active` is an absolute row index; step within the
                    // activatable subsequence so separators never highlight.
                    let position = state
                        .active
                        .and_then(|ix| items.iter().position(|&item| item == ix));
                    state.active =
                        popover::menu_step(position, items.len(), delta).map(|pos| items[pos]);
                }
                cx.notify();
            }
            popover::MenuKey::Enter | popover::MenuKey::ModEnter => {
                if let Some(row) = self.menu_bar.as_open().and_then(|state| state.active) {
                    self.activate_menu_bar_row(row, window, cx);
                }
            }
            popover::MenuKey::Backspace | popover::MenuKey::Other => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action_names(menu: MenuId) -> Vec<&'static str> {
        menu_rows(menu, &crate::settings::KeymapConfig::default())
            .iter()
            .filter_map(|row| match row {
                MenuRow::Item { action, .. } => Some(action.name()),
                MenuRow::Separator => None,
            })
            .collect()
    }

    #[test]
    fn menu_tables_dispatch_the_existing_actions() {
        use crate::app_menus;
        assert_eq!(
            action_names(MenuId::File),
            [NewSession.name(), OpenSettings.name(), QuitApp.name()]
        );
        assert_eq!(
            action_names(MenuId::Edit),
            [
                crate::composer::Undo.name(),
                crate::composer::Redo.name(),
                crate::composer::Cut.name(),
                crate::composer::Copy.name(),
                crate::composer::Paste.name(),
                crate::composer::SelectAll.name(),
            ]
        );
        assert_eq!(
            action_names(MenuId::View),
            [
                app_menus::AppearanceSystem.name(),
                app_menus::AppearanceLight.name(),
                app_menus::AppearanceDark.name(),
                ToggleSidebar.name(),
                crate::terminal::panel::ToggleTerminal.name(),
                ToggleChanges.name(),
                app_menus::ZoomIn.name(),
                app_menus::ZoomOut.name(),
            ]
        );
        assert_eq!(action_names(MenuId::Help), [app_menus::About.name()]);
    }

    #[test]
    fn separators_are_never_activatable() {
        let keymap = crate::settings::KeymapConfig::default();
        for menu in MENUS {
            let rows = menu_rows(menu, &keymap);
            let items = activatable_rows(menu, &keymap);
            for (ix, row) in rows.iter().enumerate() {
                assert_eq!(
                    matches!(row, MenuRow::Item { .. }),
                    items.contains(&ix),
                    "{menu:?} row {ix}"
                );
            }
        }
    }

    #[test]
    fn cleared_customizable_shortcuts_fall_back_to_the_default_hint() {
        // `apply_keymap` treats an empty combo as "use the default" for these
        // ids, so the menu hint must show the default the binding lives on.
        let keymap = crate::settings::KeymapConfig {
            toggle_sidebar: String::new(),
            ..Default::default()
        };
        let rows = menu_rows(MenuId::View, &keymap);
        let sidebar = rows.iter().find_map(|row| match row {
            MenuRow::Item {
                label, shortcut, ..
            } if *label == "Toggle Sidebar" => Some(shortcut),
            _ => None,
        });
        let expected = display_combo(ShortcutId::ToggleSidebar.default_combo());
        assert_eq!(sidebar, Some(&Some(expected)));
    }
}
