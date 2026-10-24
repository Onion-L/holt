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
use crate::i18n::t;
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
    // About is handled by the shell window (in-app dialog with the logo —
    // see shell/about.rs), like OpenSettings: the menu item dispatches the
    // action to the key window, no global handler here.
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
///
/// Labels come from the catalog (`shell_chrome.app_menus.*`) read at build
/// time — a snapshot, so a language switch rebuilds the bar by re-running
/// this through `cx.set_menus` (`i18n::set_language` does). The first menu's
/// name stays the literal app name: macOS titles it from the bundle anyway,
/// and it is identity, not copy.
pub fn app_menus() -> Vec<Menu> {
    let macos = cfg!(target_os = "macos");

    // macOS titles the first menu with the bundle/process name regardless of
    // what we pass, but gpui still wants a name.
    let mut app_items = vec![
        MenuItem::action(t!("shell_chrome.app_menus.about"), About),
        MenuItem::separator(),
        MenuItem::action(t!("shell_chrome.app_menus.settings"), shell::OpenSettings),
        MenuItem::separator(),
    ];
    if macos {
        app_items.extend([
            MenuItem::os_submenu(
                t!("shell_chrome.app_menus.services"),
                SystemMenuType::Services,
            ),
            MenuItem::separator(),
            MenuItem::action(t!("shell_chrome.app_menus.hide_app"), Hide),
            MenuItem::action(t!("shell_chrome.app_menus.hide_others"), HideOthers),
            MenuItem::action(t!("shell_chrome.app_menus.show_all"), ShowAll),
            MenuItem::separator(),
        ]);
    }
    app_items.push(MenuItem::action(t!("shell_chrome.app_menus.quit"), Quit));

    let mut menus = vec![
        Menu::new("Holt").items(app_items),
        // Standard clipboard verbs tied to the composer's existing actions via
        // their native selectors (`OsAction` → cut:/copy:/paste:/selectAll:),
        // so the OS Edit menu routes through the responder chain to the focused
        // input — zed wires its editor actions identically
        // (crates/zed/src/zed/app_menus.rs, Edit/Selection menus).
        Menu::new(t!("shell_chrome.app_menus.menu_edit")).items([
            // Undo/Redo have no `OsAction` counterpart — they dispatch as plain
            // actions to the focused input, same as the composer keymap.
            MenuItem::action(t!("shell_chrome.app_menus.undo"), composer::Undo),
            MenuItem::action(t!("shell_chrome.app_menus.redo"), composer::Redo),
            MenuItem::separator(),
            MenuItem::os_action(
                t!("shell_chrome.app_menus.cut"),
                composer::Cut,
                OsAction::Cut,
            ),
            MenuItem::os_action(
                t!("shell_chrome.app_menus.copy"),
                composer::Copy,
                OsAction::Copy,
            ),
            MenuItem::os_action(
                t!("shell_chrome.app_menus.paste"),
                composer::Paste,
                OsAction::Paste,
            ),
            MenuItem::separator(),
            MenuItem::os_action(
                t!("shell_chrome.app_menus.select_all"),
                composer::SelectAll,
                OsAction::SelectAll,
            ),
        ]),
    ];
    // Appearance and interface zoom live under View on every platform —
    // "Appearance" as a top-level menu would read oddly next to Edit.
    menus.push(Menu::new(t!("shell_chrome.app_menus.menu_view")).items([
        MenuItem::action(
            t!("shell_chrome.app_menus.appearance_system"),
            AppearanceSystem,
        ),
        MenuItem::action(
            t!("shell_chrome.app_menus.appearance_light"),
            AppearanceLight,
        ),
        MenuItem::action(t!("shell_chrome.app_menus.appearance_dark"), AppearanceDark),
        MenuItem::separator(),
        MenuItem::action(t!("shell_chrome.app_menus.zoom_in"), ZoomIn),
        MenuItem::action(t!("shell_chrome.app_menus.zoom_out"), ZoomOut),
    ]));
    if macos {
        // Standard Window menu; macOS appends the open-window list itself.
        menus.push(Menu::new(t!("shell_chrome.app_menus.menu_window")).items([
            MenuItem::action(t!("shell_chrome.app_menus.minimize"), Minimize),
            MenuItem::action(t!("shell_chrome.app_menus.zoom"), Zoom),
            MenuItem::separator(),
            MenuItem::action(t!("shell_chrome.app_menus.close_window"), CloseWindow),
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
        let _locale = crate::i18n::tests::locale_lock();
        let menus = app_menus();
        assert_eq!(menus[0].name.as_ref(), "Holt");
        let Some(MenuItem::Action { name, action, .. }) = menus[0].items.last() else {
            panic!("last app-menu item must be an action");
        };
        assert_eq!(name.as_ref(), t!("shell_chrome.app_menus.quit").as_ref());
        assert_eq!(action.name(), Quit.name());
    }

    #[test]
    fn app_menu_offers_settings() {
        let _locale = crate::i18n::tests::locale_lock();
        let menus = app_menus();
        assert!(
            menus[0].items.iter().any(|item| matches!(
                item,
                MenuItem::Action { name, action, .. }
                    if name.as_ref() == t!("shell_chrome.app_menus.settings").as_ref()
                        && action.name() == shell::OpenSettings.name()
            )),
            "the application menu should expose Settings"
        );
    }

    #[test]
    fn about_is_the_first_enabled_app_menu_item() {
        let _locale = crate::i18n::tests::locale_lock();
        let menus = app_menus();
        let MenuItem::Action { name, action, .. } = &menus[0].items[0] else {
            panic!("first app-menu item must be the About action");
        };
        assert_eq!(name.as_ref(), t!("shell_chrome.app_menus.about").as_ref());
        assert_eq!(action.name(), About.name());
        assert!(!menus[0].items[0].is_disabled());
    }

    #[test]
    fn edit_menu_uses_composer_clipboard_os_actions() {
        let _locale = crate::i18n::tests::locale_lock();
        let menus = app_menus();
        let edit = menus
            .iter()
            .find(|m| m.name.as_ref() == t!("shell_chrome.app_menus.menu_edit").as_ref())
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
        let _locale = crate::i18n::tests::locale_lock();
        let menus = app_menus();
        let view = menus
            .iter()
            .find(|m| m.name.as_ref() == t!("shell_chrome.app_menus.menu_view").as_ref())
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
        let _locale = crate::i18n::tests::locale_lock();
        let menus = app_menus();
        let view = menus
            .iter()
            .find(|m| m.name.as_ref() == t!("shell_chrome.app_menus.menu_view").as_ref())
            .expect("View menu present");
        assert_eq!(action_names(view)[3..], [ZoomIn.name(), ZoomOut.name()]);
    }

    /// Labels come from the catalog, so the menu bar re-translates when the
    /// process locale moves — the property `i18n::set_language` relies on
    /// when it re-runs `app_menus` through `cx.set_menus`. Expected values
    /// assert literal zh-CN text (not `t!` again — the test must observe
    /// the switch, not assume it). Holds the locale lock: another test
    /// must not flip the language mid-assert, and the locale resets after.
    #[test]
    fn menu_labels_follow_the_locale() {
        let _guard = crate::i18n::tests::locale_lock();
        let menus = app_menus();
        let MenuItem::Action { name, .. } = &menus[0].items[0] else {
            panic!("first app-menu item must be an action");
        };
        assert_eq!(name.as_ref(), "About Holt");

        rust_i18n::set_locale("zh-CN");
        let menus = app_menus();
        let MenuItem::Action { name, .. } = &menus[0].items[0] else {
            panic!("first app-menu item must be an action");
        };
        assert_eq!(name.as_ref(), "关于 Holt");
        let MenuItem::Action { name, .. } = menus[0].items.last().unwrap() else {
            panic!("last app-menu item must be an action");
        };
        assert_eq!(name.as_ref(), "退出 Holt");
        let edit = menus
            .iter()
            .find(|m| m.name.as_ref() == "编辑")
            .expect("Edit menu follows the locale");
        // Undo, Redo, Cut, Copy, Paste, Select All, and the two separators.
        assert_eq!(edit.items.len(), 8);

        crate::i18n::tests::reset_locale();
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
