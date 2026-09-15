//! Native menu bar + app-level window actions (macOS-first).
//!
//! holt never called `cx.set_menus`, so on macOS `NSApp.mainMenu` stayed nil:
//! no app menu, no ⌘Q quit, and nothing for the auto-hidden system menu bar to
//! reveal on hover (gpui only calls `setMainMenu_` from `set_menus` —
//! gpui_macos/src/platform.rs `fn set_menus`). Structure ported from zed's
//! `crates/zed/src/zed/app_menus.rs` and the gpui `set_menus.rs` example at the
//! pinned rev (f14fea9bf3c9).
//!
//! Wiring: [`init`] registers the global action handlers (run once at boot),
//! [`bind_keys`] installs the fixed application shortcuts (re-run by
//! `shell::apply_keymap`, which clears every binding first), and
//! [`app_menus`] builds the menu bar handed to `cx.set_menus` in `run_app`.

use gpui::{App, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType, Window, actions};

use crate::appearance::{self, AppearanceMode};
use crate::composer;
use crate::shell;

actions!(
    holt,
    [
        About,
        Quit,
        Hide,
        HideOthers,
        ShowAll,
        Minimize,
        Zoom,
        CloseWindow,
        AppearanceSystem,
        AppearanceLight,
        AppearanceDark,
        ZoomIn,
        ZoomOut,
    ]
);

/// Register the global handlers backing the menu bar and its shortcuts. Call
/// once at boot, before `cx.set_menus`.
pub fn init(cx: &mut App) {
    cx.on_action(quit);
    // Application-menu verbs — gpui wraps NSApp `hide` / `hideOtherApplications`
    // / `unhideAllApplications` (zed registers the same trio in
    // crates/zed/src/zed.rs `init`).
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    // Window verbs route to the active window. holt is single-window, so a
    // global handler suffices where zed registers these per-workspace
    // (crates/zed/src/zed.rs `register_action(Minimize/Zoom)`).
    cx.on_action(|_: &Minimize, cx| with_active_window(cx, |window, _| window.minimize_window()));
    cx.on_action(|_: &Zoom, cx| with_active_window(cx, |window, _| window.zoom_window()));
    cx.on_action(|_: &CloseWindow, cx| {
        if let Some(handle) = cx.active_window() {
            // Deferred: this handler runs inside the dispatching window's
            // update (see `quit`), where updating that same window re-enters
            // gpui's guard and fails with "window not found". The effect loop
            // flushes after the dispatch, with the window slot restored.
            cx.defer(move |cx| {
                let _ = handle.update(cx, |_, window, cx| {
                    crate::terminal::lifecycle::request_close(window, cx, false)
                });
            });
        }
    });
    // Interface zoom (⌘+/⌘-): steps the persisted UI font size catalog; the
    // active window's rem basis is re-set inside the update and
    // `refresh_windows` repaints the rest.
    cx.on_action(|_: &ZoomIn, cx| {
        with_active_window(cx, |window, cx| {
            crate::typography::zoom_in(window, cx);
        })
    });
    cx.on_action(|_: &ZoomOut, cx| {
        with_active_window(cx, |window, cx| {
            crate::typography::zoom_out(window, cx);
        })
    });
    // Appearance. Each verb persists and repaints every window; see
    // `appearance::set_mode`.
    cx.on_action(|_: &AppearanceSystem, cx| appearance::set_mode(AppearanceMode::System, cx));
    cx.on_action(|_: &AppearanceLight, cx| appearance::set_mode(AppearanceMode::Light, cx));
    cx.on_action(|_: &AppearanceDark, cx| appearance::set_mode(AppearanceMode::Dark, cx));
}

/// Run `f` on the active window's `&mut Window` (plus the `&mut App` seen
/// inside that update), deferred to the end of the effect cycle. Global action
/// handlers run inside the dispatching window's `update` (gpui's re-entrancy
/// guard: that window is taken out of `cx.windows` mid-dispatch), so an
/// immediate `handle.update` on the same window fails with "window not found"
/// — deferring waits until the slot is restored.
fn with_active_window(cx: &mut App, f: impl FnOnce(&mut Window, &mut App) + 'static) {
    if let Some(window) = cx.active_window() {
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| f(window, cx));
        });
    }
}

/// ⌘Q / "Quit Holt". `cx.quit()` runs the platform's standard quit routine,
/// which invokes gpui `App::shutdown` — that fires the `on_app_quit` observers
/// registered in `run_app` (embedded-engine drain: live runs + doc snapshot
/// flush) with gpui's shutdown timeout before the process exits. Same graceful
/// path as quitting from the Dock or closing the last window.
///
/// This handler runs mid action-dispatch, i.e. inside the dispatching window's
/// `update` (gpui's re-entrancy guard has that window taken out of
/// `cx.windows`), so it must NOT `active_window().update(...)` synchronously —
/// that fails with "window not found" and silently swallowed the quit. Instead
/// `cx.quit()` defers `[NSApp terminate:]` to the main queue; the resulting
/// `applicationShouldTerminate` re-enters gpui from the run loop, where
/// `terminal::lifecycle`'s `on_should_quit` gate runs the shared
/// `request_close` confirmation and, once approved, re-calls `cx.quit()`.
fn quit(_: &Quit, cx: &mut App) {
    cx.quit();
}

/// Fixed app-level shortcuts backing the menu key equivalents. These live
/// outside the customizable keymap; `shell::apply_keymap` calls this after its
/// `clear_key_bindings` so they survive keymap re-application. Settings follows
/// the platform convention everywhere (Cmd+, on macOS, Ctrl+, elsewhere);
/// window/application lifecycle shortcuts remain macOS-only.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(app_key_bindings(cfg!(target_os = "macos")));
}

/// The binding table behind [`bind_keys`] — `KeyBinding` construction is pure
/// (no `App`), so unit tests can inspect it directly.
fn app_key_bindings(macos: bool) -> Vec<KeyBinding> {
    let primary = if macos { "cmd" } else { "ctrl" };
    let mut bindings = vec![
        KeyBinding::new(
            if macos { "cmd-," } else { "ctrl-," },
            shell::OpenSettings,
            None,
        ),
        // Interface zoom: the "+" variant covers the shifted "=" key — macOS
        // reports Cmd+Shift+= as the keystroke "cmd-+" (shift folded into the
        // character), and Linux keysyms name it "plus" the same way.
        KeyBinding::new(&format!("{primary}-="), ZoomIn, None),
        KeyBinding::new(&format!("{primary}-+"), ZoomIn, None),
        KeyBinding::new(&format!("{primary}--"), ZoomOut, None),
    ];
    if macos {
        bindings.extend([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-h", Hide, None),
            KeyBinding::new("alt-cmd-h", HideOthers, None),
            KeyBinding::new("cmd-m", Minimize, None),
            KeyBinding::new("cmd-w", CloseWindow, None),
        ]);
    }
    bindings
}

/// The holt menu bar. macOS renders this natively; mac-only entries are gated
/// at runtime (`cfg!`) so the whole module compiles and tests on Linux.
pub fn app_menus() -> Vec<Menu> {
    let macos = cfg!(target_os = "macos");

    // macOS titles the first menu with the bundle/process name regardless of
    // what we pass, but gpui still wants a name.
    let mut app_items = vec![
        // Placeholder until a real about dialog exists (explicitly disabled).
        MenuItem::action("About Holt", About).disabled(true),
        MenuItem::separator(),
        MenuItem::action("Settings", shell::OpenSettings),
        MenuItem::separator(),
    ];
    if macos {
        app_items.extend([
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide Holt", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
        ]);
    }
    app_items.push(MenuItem::action("Quit Holt", Quit));

    let mut menus = vec![
        Menu::new("Holt").items(app_items),
        // Standard clipboard verbs tied to the composer's existing actions via
        // their native selectors (`OsAction` → cut:/copy:/paste:/selectAll:),
        // so the OS Edit menu routes through the responder chain to the focused
        // input — zed wires its editor actions identically
        // (crates/zed/src/zed/app_menus.rs, Edit/Selection menus).
        Menu::new("Edit").items([
            // Undo/Redo have no `OsAction` counterpart — they dispatch as plain
            // actions to the focused input, same as the composer keymap.
            MenuItem::action("Undo", composer::Undo),
            MenuItem::action("Redo", composer::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", composer::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", composer::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", composer::Paste, OsAction::Paste),
            MenuItem::separator(),
            MenuItem::os_action("Select All", composer::SelectAll, OsAction::SelectAll),
        ]),
    ];
    // Appearance and interface zoom live under View on every platform —
    // "Appearance" as a top-level menu would read oddly next to Edit.
    menus.push(Menu::new("View").items([
        MenuItem::action("Appearance: System", AppearanceSystem),
        MenuItem::action("Appearance: Light", AppearanceLight),
        MenuItem::action("Appearance: Dark", AppearanceDark),
        MenuItem::separator(),
        MenuItem::action("Zoom In", ZoomIn),
        MenuItem::action("Zoom Out", ZoomOut),
    ]));
    if macos {
        // Standard Window menu; macOS appends the open-window list itself.
        menus.push(Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
            MenuItem::separator(),
            MenuItem::action("Close Window", CloseWindow),
        ]));
    }
    menus
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Action as _, Keystroke};

    fn action_names(menu: &Menu) -> Vec<&'static str> {
        menu.items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action { action, .. } => Some(action.name()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn app_menu_ends_with_quit() {
        let menus = app_menus();
        assert_eq!(menus[0].name.as_ref(), "Holt");
        let Some(MenuItem::Action { name, action, .. }) = menus[0].items.last() else {
            panic!("last app-menu item must be an action");
        };
        assert_eq!(name.as_ref(), "Quit Holt");
        assert_eq!(action.name(), Quit.name());
    }

    #[test]
    fn app_menu_offers_settings() {
        let menus = app_menus();
        assert!(
            menus[0].items.iter().any(|item| matches!(
                item,
                MenuItem::Action { name, action, .. }
                    if name.as_ref() == "Settings" && action.name() == shell::OpenSettings.name()
            )),
            "the application menu should expose Settings"
        );
    }

    #[test]
    fn about_is_disabled_placeholder() {
        let menus = app_menus();
        let first = &menus[0].items[0];
        assert!(
            first.is_disabled(),
            "About stays disabled until implemented"
        );
    }

    #[test]
    fn edit_menu_uses_composer_clipboard_os_actions() {
        let menus = app_menus();
        let edit = menus
            .iter()
            .find(|m| m.name.as_ref() == "Edit")
            .expect("Edit menu present");
        // `OsAction` has no `Debug` impl at the pinned rev, so compare
        // per-field.
        let expect = [
            (composer::Cut.name(), OsAction::Cut),
            (composer::Copy.name(), OsAction::Copy),
            (composer::Paste.name(), OsAction::Paste),
            (composer::SelectAll.name(), OsAction::SelectAll),
        ];
        let got: Vec<(&str, OsAction)> = edit
            .items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action {
                    action,
                    os_action: Some(os_action),
                    ..
                } => Some((action.name(), *os_action)),
                _ => None,
            })
            .collect();
        assert_eq!(got.len(), expect.len());
        for ((got_name, got_os), (want_name, want_os)) in got.iter().zip(expect.iter()) {
            assert_eq!(got_name, want_name);
            assert!(got_os == want_os, "OsAction mismatch for {want_name}");
        }
    }

    #[test]
    fn view_menu_offers_all_three_appearance_modes() {
        let menus = app_menus();
        let view = menus
            .iter()
            .find(|m| m.name.as_ref() == "View")
            .expect("View menu present");
        assert_eq!(
            action_names(view)[..3],
            [
                AppearanceSystem.name(),
                AppearanceLight.name(),
                AppearanceDark.name()
            ]
        );
    }

    #[test]
    fn view_menu_offers_interface_zoom() {
        let menus = app_menus();
        let view = menus
            .iter()
            .find(|m| m.name.as_ref() == "View")
            .expect("View menu present");
        assert_eq!(action_names(view)[3..], [ZoomIn.name(), ZoomOut.name()]);
    }

    #[test]
    fn app_bindings_use_platform_settings_convention() {
        // `KeyBinding::new` panics on unparseable combos, so constructing the
        // table is itself the parse check.
        let find = |bindings: &[KeyBinding], name: &str| {
            bindings
                .iter()
                .find(|binding| binding.action().name() == name)
                .map(|binding| {
                    binding
                        .keystrokes()
                        .iter()
                        .map(|ks| ks.inner().clone())
                        .collect::<Vec<_>>()
                })
        };
        let combo = |source: &str| vec![Keystroke::parse(source).unwrap()];
        let macos = app_key_bindings(true);
        assert_eq!(
            find(&macos, shell::OpenSettings.name()),
            Some(combo("cmd-,"))
        );
        assert_eq!(find(&macos, Quit.name()), Some(combo("cmd-q")));
        assert_eq!(find(&macos, CloseWindow.name()), Some(combo("cmd-w")));
        assert_eq!(find(&macos, Minimize.name()), Some(combo("cmd-m")));
        assert_eq!(find(&macos, ZoomIn.name()), Some(combo("cmd-=")));
        assert_eq!(find(&macos, ZoomOut.name()), Some(combo("cmd--")));

        let other = app_key_bindings(false);
        assert_eq!(
            find(&other, shell::OpenSettings.name()),
            Some(combo("ctrl-,"))
        );
        assert_eq!(find(&other, ZoomIn.name()), Some(combo("ctrl-=")));
        assert_eq!(find(&other, ZoomOut.name()), Some(combo("ctrl--")));
        assert_eq!(find(&other, Quit.name()), None);
    }

    #[test]
    fn shifted_plus_parses_as_the_zoom_in_equivalent() {
        // macOS reports Cmd+Shift+= as the keystroke "cmd-+" (the platform
        // event parser folds shift into the shifted character), and the
        // binding table carries that literal variant.
        let plus = Keystroke::parse("cmd-+").unwrap();
        assert_eq!(plus.key, "+");
        assert!(plus.modifiers.platform);
        assert!(!plus.modifiers.shift);
        let other = Keystroke::parse("ctrl-+").unwrap();
        assert_eq!(other.key, "+");
        assert!(other.modifiers.control);
    }

    #[gpui::test]
    fn zoom_actions_step_the_active_window_and_clamp_at_the_catalog_ends(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            crate::typography::init(
                crate::typography::UiFontFamily::default(),
                crate::typography::UiFontSize::default(),
                crate::typography::FontAvailability::all(),
                cx,
            );
            init(cx);
        });
        let (_view, cx) = cx.add_window_view(|_, _| gpui::Empty);
        // `with_active_window` routes through `App::active_window`, which the
        // test platform only tracks after an explicit activation.
        cx.update(|window, _| window.activate_window());

        let shown_size =
            |cx: &gpui::TestAppContext| cx.update(|cx| crate::typography::font_size(cx).pixels());

        // 16 → 18: the deferred active-window update flushes with dispatch.
        cx.dispatch_action(ZoomIn);
        assert_eq!(shown_size(cx), 18.0);
        cx.dispatch_action(ZoomOut);
        assert_eq!(shown_size(cx), 16.0);

        // Repeated zoom-in parks at the top of the catalog (20), repeated
        // zoom-out at the bottom (12).
        for _ in 0..8 {
            cx.dispatch_action(ZoomIn);
        }
        assert_eq!(shown_size(cx), 20.0);
        for _ in 0..8 {
            cx.dispatch_action(ZoomOut);
        }
        assert_eq!(shown_size(cx), 12.0);
    }
}
