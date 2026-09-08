//! holt-ui — the gpui viewport. Shell, sidebar, conversation, composer, terminal,
//! diff pane.
//!
//! Design: ARCHITECTURE.md §4; animation catalog docs/research/feature-inventory.md
//! §1.12; virtualization/markdown techniques docs/research/mugen-pretext.md.
//!
//! M3a foundation:
//! - [`theme`] — always-dark monochrome theme (oklch-derived neutrals), a gpui Global;
//! - [`motion`] — the holt animation catalog over gpui `Animation` + cubic-bezier;
//! - [`state`] — `AppState` entity + `EngineHandle` (connect-or-embed engine);
//! - [`settings`] — persisted pane widths/collapse flags;
//! - [`shell`] — sidebar + main panel + right-pane scaffold + gate;
//! - [`loaders`] — holt pulse loader, gradient spinner, boot splash.

mod app_icon;
pub mod app_menus;
pub mod appearance;
pub mod attachments;
pub mod badges;
pub mod change_requests;
pub mod changes;
pub mod comments;
pub mod composer;
pub mod edge_fade;
pub mod files;
pub mod frost;
pub mod history;
pub mod icons;
pub mod image_viewer;
pub mod images;
pub mod links;
pub mod loaders;
pub mod markdown;
pub mod motion;
pub mod path_refs;
pub mod pickers;
pub mod popover;
pub mod rail;
pub mod settings;
pub mod shell;
pub mod state;
pub mod syntax_cache;
pub mod terminal;
pub mod theme;
pub mod theme_library;
pub mod transcript;
pub mod typography;
mod watch_coordinator;

use std::path::PathBuf;

use futures::StreamExt as _;
use gpui::{App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

pub use holt_proto::ProviderId;
pub use state::EngineBootConfig;

/// Everything the headed binary passes in (config/env resolution lives in
/// `apps/holt`, not here).
#[derive(Debug, Clone)]
pub struct UiConfig {
    /// Data directory — engine stores + `ui-settings.json`.
    pub data_dir: PathBuf,
    /// Conversation URL passed by the OS on a cold launch.
    pub initial_url: Option<String>,
}

impl UiConfig {
    fn boot(&self) -> EngineBootConfig {
        EngineBootConfig {
            data_dir: self.data_dir.clone(),
        }
    }
}

/// What a dock-icon reopen needs to rebuild the main window after ⌘W closed it
/// (macOS keeps the process alive with just the menu bar, like zed).
struct ReopenState {
    state: gpui::Entity<state::AppState>,
    boot: EngineBootConfig,
}

impl gpui::Global for ReopenState {}

/// Run the headed app: tokio bridge up, engine bootstrap kicked off (probe →
/// connect-or-embed), 1320×880 window (min 900×600) with [`shell::Shell`] as the
/// root view, boot splash overlaid until the engine reports ready.
pub fn run_app(config: UiConfig) {
    let app = gpui_platform::application().with_assets(icons::Assets);
    let (url_tx, mut url_rx) = futures::channel::mpsc::unbounded::<String>();
    let callback_tx = url_tx.clone();
    app.on_open_urls(move |urls| {
        for url in urls {
            let _ = callback_tx.unbounded_send(url);
        }
    });
    if let Some(url) = config.initial_url.clone() {
        let _ = url_tx.unbounded_send(url);
    }
    // Dock-icon click with no window (⌘W closed it): rebuild the main window
    // around the still-running engine — zed does the same via `on_reopen`
    // (crates/zed/src/main.rs `app.on_reopen`).
    app.on_reopen(|cx| {
        if cx.windows().is_empty()
            && let Some(reopen) = cx.try_global::<ReopenState>()
        {
            let (state, boot) = (reopen.state.clone(), reopen.boot.clone());
            open_main_window(state, boot, cx);
        }
    });
    app.run(move |cx: &mut App| {
        // Dock/⌘-Tab icon before the first frame paints.
        app_icon::install();
        // NB: pinned-rev API — `gpui_tokio::init(cx)` free function (not `Tokio::init`).
        gpui_tokio::init(cx);
        let data_dir = config.boot().data_dir.clone();
        let ui_settings = settings::UiSettings::load(&data_dir);
        settings::init(ui_settings.clone(), data_dir.clone(), cx);
        let font_availability = typography::register_fonts(cx);
        // Typography first: theme installation reads the effective family, so
        // the first frame has the final font and palette without a flash.
        typography::init(
            ui_settings.ui_font_family.clone(),
            ui_settings.ui_font_size,
            font_availability,
            cx,
        );
        theme_library::init(data_dir.clone(), cx);
        appearance::init(
            ui_settings.appearance,
            ui_settings.theme_selection,
            ui_settings.accent,
            ui_settings.surface,
            cx,
        );
        composer::init(cx);
        files::editor::init(cx);
        terminal::panel::init(cx);
        app_menus::init(cx);
        cx.register_url_scheme("holt").detach();

        let state = cx.new(|_| state::AppState::new());
        terminal::lifecycle::init(state.clone(), cx);
        let url_state = state.clone();
        cx.spawn(async move |cx| {
            while let Some(url) = url_rx.next().await {
                url_state.update(cx, |state, cx| state.open_deep_link(&url, cx));
            }
        })
        .detach();
        state::AppState::bootstrap(state.clone(), config.boot(), cx);

        // Graceful teardown: the in-process engine drains live runs and flushes
        // doc snapshots before the process exits.
        let quit_state = state.clone();
        cx.on_app_quit(move |cx| {
            settings::flush(cx);
            let shutdown =
                quit_state.read(cx).engine().cloned().map(|handle| {
                    gpui_tokio::Tokio::spawn(cx, async move { handle.shutdown().await })
                });
            async move {
                if let Some(task) = shutdown {
                    let _ = task.await;
                }
            }
        })
        .detach();

        cx.set_global(ReopenState {
            state: state.clone(),
            boot: config.boot(),
        });
        open_main_window(state, config.boot(), cx);
        // Native menu bar — macOS gets the standard app menu (About/Services/
        // Hide/Quit ⌘Q), Edit clipboard verbs routed to the focused input, and
        // a Window menu (⌘M/⌘W). Without this, `NSApp.mainMenu` stays nil: no
        // Cmd+Q, and nothing for the system menu bar to show. Set after
        // `open_main_window` because `Shell::new` ran `apply_keymap`
        // synchronously, so `set_menus` reads the final bindings for the ⌘-key
        // equivalents (gpui snapshots the keymap at set time).
        cx.set_menus(app_menus::app_menus());
        cx.activate(true);
    });
}

/// Open the 1320×880 main window (min 900×600) with [`shell::Shell`] as the
/// root view. Called at boot and again from `on_reopen` if the dock icon is
/// clicked after ⌘W closed the window.
fn open_main_window(state: gpui::Entity<state::AppState>, boot: EngineBootConfig, cx: &mut App) {
    // holt window geometry: 1320×880, min 900×600 (feature-inventory §1.1).
    let bounds = Bounds::centered(None, size(px(1320.), px(880.)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(900.), px(600.))),
            // `kind` is deliberately left at its default `WindowKind::Normal`
            // (gpui platform.rs WindowOptions::default), which on macOS maps
            // to `NSNormalWindowLevel` (gpui_macos window.rs) — same as zed's
            // main window. Nothing here raises the window level or touches
            // presentation options; the "menu bar never appears" symptom came
            // from the missing `set_menus` call (nil `NSApp.mainMenu`), not
            // from window kind/level, and `appears_transparent` only affects
            // the titlebar, not the menu bar.
            // macOS: frameless-inset chrome like the original Electron app
            // (`titleBarStyle: "hiddenInset"`, traffic lights at 14,15 —
            // feature-inventory §1.1). No title text — the strip is
            // custom-drawn (zed sets `title: None` the same way). On
            // Linux/Windows `appears_transparent` hides the system titlebar
            // for our custom-drawn chrome; harmless where unsupported.
            titlebar: Some(TitlebarOptions {
                title: None,
                appears_transparent: true,
                // Centered on the titlebar's content line (40px bar, content
                // shifted 4px down, lights ~12px tall → center 22).
                traffic_light_position: Some(gpui::point(px(14.), px(14.))),
            }),
            // Our own titlebar strip drags the window (WindowControlArea::
            // Drag + start_window_move) — mark the content view app-owned
            // so AppKit neither dead-zones the strip nor delays clicks.
            app_owns_titlebar_drag: true,
            // Linux: request client-side decorations — holt draws its own
            // unified titlebar and (under CSD) its own caption buttons
            // (shell.rs `render_linux_caption_controls`). Leaving this unset
            // requests SERVER decorations, which stacked a compositor
            // titlebar on top of the app's chrome under sway/KDE, while
            // compositors without SSD support (GNOME) went client-side
            // anyway — frameless, and before the shell drew caption buttons,
            // with no window controls at all. The compositor can still
            // override via xdg-decoration negotiation; the shell re-resolves
            // what to draw every frame.
            window_decorations: cfg!(target_os = "linux")
                .then_some(gpui::WindowDecorations::Client),
            // Frosted shell (macOS): blur the desktop behind the window; the
            // shell paints its frost surface translucent so the sidebar reads
            // as glass (shell.rs root). Elsewhere blur support is compositor
            // roulette — stay opaque.
            // One source of truth with the re-apply loop in `appearance::apply`
            // — if these two ever disagree, vibrancy dies on the first theme
            // change and never comes back.
            window_background: theme::Theme::of(cx).window_background_appearance(),
            app_id: Some("holt".into()),
            ..Default::default()
        },
        move |window, cx| {
            window.on_window_should_close(cx, |window, cx| {
                terminal::lifecycle::request_close(window, cx, false);
                false
            });
            window.set_rem_size(px(typography::font_size(cx).pixels()));
            // React to the user flipping macOS between light and dark. Detached:
            // the subscription lives as long as the window does, and the window
            // owns nothing that would drop it early.
            appearance::observe_window(window, cx).detach();
            cx.new(|cx| shell::Shell::new(state, boot, cx))
        },
    )
    .expect("failed to open window");
    // Belt and braces: assert the blur once the window actually exists. The
    // `WindowOptions` value is applied during creation, before the view is
    // attached; re-pushing it here means a window is never left opaque.
    appearance::reapply_window_background(cx);
}
