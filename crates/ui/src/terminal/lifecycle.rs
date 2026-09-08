//! One confirmation path for window chrome, Cmd+Q, and native Dock quit.
//! The file sidebar's unsaved drafts gate the SAME decision, ahead of the
//! running-terminal check (ADR-0020; decision 11): a failed save refuses the
//! exit, and cancelling either keeps the app — and the terminals — alive.

use crate::shell::Shell;
use crate::state::AppState;
use gpui::{App, Entity, Global, Window};
use holt_rpc::{methods, terminals::TerminalStatus};

struct Lifecycle {
    state: Entity<AppState>,
    pending: bool,
    approved_quit: bool,
    /// The shell's file-draft surface, attached on its first render — the
    /// lifecycle global itself is created before any window exists.
    shell: Option<Entity<Shell>>,
}
impl Global for Lifecycle {}

pub fn init(state: Entity<AppState>, cx: &mut App) {
    cx.set_global(Lifecycle {
        state,
        pending: false,
        approved_quit: false,
        shell: None,
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

/// The shell registers once it exists (first render of the ready page).
pub fn attach_shell(shell: Entity<Shell>, cx: &mut App) {
    if cx.has_global::<Lifecycle>() {
        cx.global_mut::<Lifecycle>().shell = Some(shell);
    }
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
    let shell = lifecycle.shell.clone();
    cx.global_mut::<Lifecycle>().pending = true;
    window
        .spawn(cx, async move |cx| {
            // Unsaved file drafts decide FIRST (before anything terminates):
            // Save writes every modified buffer, Discard drops them, Cancel
            // keeps the app — and the terminals — exactly as they were.
            let drafts = shell
                .as_ref()
                .and_then(|shell| cx.update(|_, cx| shell.read(cx).dirty_file_tabs_everywhere(cx)).ok())
                .map(|drafts| drafts.len())
                .unwrap_or(0);
            if drafts > 0 {
                let answer = cx.update(|window, cx| {
                    window.prompt(
                        gpui::PromptLevel::Warning,
                        if quit {
                            "Quit Holt with unsaved files?"
                        } else {
                            "Close window with unsaved files?"
                        },
                        Some(&format!(
                            "{drafts} file(s) have unsaved changes. Saving writes them to disk; discarding loses them."
                        )),
                        &["Cancel", "Discard changes", "Save and continue"],
                        cx,
                    )
                });
                let choice = match answer {
                    Ok(answer) => answer.await,
                    Err(_) => Ok(0),
                };
                match choice {
                    Ok(2) => {
                        let saved = shell.and_then(|shell| {
                            cx.update(|_, cx| {
                                shell.update(cx, |shell, cx| shell.save_all_dirty_files(cx))
                            })
                            .ok()
                        });
                        let all_saved = match saved {
                            Some(task) => task.await,
                            None => Err(drafts),
                        };
                        if let Err(failed) = all_saved {
                            let _ = cx.update(|window, cx| {
                                cx.global_mut::<Lifecycle>().pending = false;
                                drop(window.prompt(
                                    gpui::PromptLevel::Critical,
                                    "Could not save every file",
                                    Some(&format!(
                                        "{failed} file(s) could not be saved. The app stays open — nothing was lost."
                                    )),
                                    &["OK"],
                                    cx,
                                ));
                            });
                            return;
                        }
                    }
                    Ok(1) => {
                        // Discard: the quit itself releases the drafts.
                    }
                    _ => {
                        let _ = cx.update(|_, cx| cx.global_mut::<Lifecycle>().pending = false);
                        return;
                    }
                }
            }
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
