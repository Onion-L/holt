//! One confirmation path for window chrome, Cmd+Q, and native Dock quit.

use crate::state::AppState;
use gpui::{App, Entity, Global, Window};
use holt_rpc::{methods, terminals::TerminalStatus};

struct Lifecycle {
    state: Entity<AppState>,
    pending: bool,
    approved_quit: bool,
}
impl Global for Lifecycle {}

pub fn init(state: Entity<AppState>, cx: &mut App) {
    cx.set_global(Lifecycle {
        state,
        pending: false,
        approved_quit: false,
    });
    cx.on_should_quit(|cx| {
        if cx.global::<Lifecycle>().approved_quit {
            return true;
        }
        if let Some(handle) = cx.active_window().or_else(|| cx.windows().first().copied()) {
            let _ = handle.update(cx, |_, window, cx| request_close(window, cx, true));
            false
        } else {
            true
        }
    });
}

pub fn request_close(window: &mut Window, cx: &mut App, quit: bool) {
    if !quit && cx.windows().len() > 1 {
        window.remove_window();
        return;
    }
    if !cx.has_global::<Lifecycle>() {
        if quit {
            cx.quit();
        } else {
            window.remove_window();
        }
        return;
    }
    let lifecycle = cx.global::<Lifecycle>();
    if lifecycle.pending {
        return;
    }
    let engine = lifecycle.state.read(cx).engine().cloned();
    cx.global_mut::<Lifecycle>().pending = true;
    window
        .spawn(cx, async move |cx| {
            let sessions = match &engine {
                Some(engine) => {
                    engine
                        .client()
                        .call_as::<Vec<TerminalStatus>>(
                            methods::LIST_TERMINALS,
                            serde_json::json!({}),
                        )
                        .await
                }
                None => Ok(Vec::new()),
            };
            let running = match sessions {
                Ok(sessions) => sessions.iter().filter(|s| s.has_running_jobs).count(),
                Err(error) => {
                    let _ = cx.update(|window, cx| {
                        cx.global_mut::<Lifecycle>().pending = false;
                        drop(window.prompt(
                            gpui::PromptLevel::Critical,
                            "Could not check running terminals",
                            Some(&error.to_string()),
                            &["OK"],
                            cx,
                        ));
                    });
                    return;
                }
            };
            if running > 0 {
                let answer = cx.update(|window, cx| {
                    window.prompt(
                        gpui::PromptLevel::Warning,
                        if quit {
                            "Quit Holt and end running terminals?"
                        } else {
                            "Close window and end running terminals?"
                        },
                        Some(&format!(
                            "This will end {running} terminal(s) and their running programs."
                        )),
                        &["Cancel", if quit { "Quit Holt" } else { "Close window" }],
                        cx,
                    )
                });
                let confirmed = match answer {
                    Ok(answer) => answer.await == Ok(1),
                    Err(_) => false,
                };
                if !confirmed {
                    let _ = cx.update(|_, cx| cx.global_mut::<Lifecycle>().pending = false);
                    return;
                }
            }
            if let Some(engine) = engine
                && let Err(error) = engine
                    .client()
                    .call(methods::CLOSE_ALL_TERMINALS, serde_json::json!({}))
                    .await
            {
                let _ = cx.update(|window, cx| {
                    cx.global_mut::<Lifecycle>().pending = false;
                    drop(window.prompt(
                        gpui::PromptLevel::Critical,
                        "Could not close terminals",
                        Some(&error.to_string()),
                        &["OK"],
                        cx,
                    ));
                });
                return;
            }
            let _ = cx.update(|window, cx| {
                let lifecycle = cx.global_mut::<Lifecycle>();
                lifecycle.pending = false;
                lifecycle.approved_quit = quit;
                if quit {
                    cx.quit();
                } else {
                    window.remove_window();
                }
            });
        })
        .detach();
}
