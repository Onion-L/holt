//! One confirmation path for window chrome, Cmd+Q, and native Dock quit.
//! The file sidebar's unsaved drafts gate the SAME decision, ahead of the
//! running-terminal check (ADR-file-editing-state-belongs-to-spaces; decision 11): a failed save refuses the
//! exit, and cancelling either keeps the app — and the terminals — alive.

use crate::i18n::t;
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
                .and_then(|shell| {
                    cx.update(|_, cx| shell.read(cx).dirty_file_tabs_everywhere(cx))
                        .ok()
                })
                .map(|drafts| drafts.len())
                .unwrap_or(0);
            if drafts > 0 {
                let answer = cx.update(|window, cx| {
                    let title = if quit {
                        t!("terminal_panel.lifecycle.quit_unsaved_title")
                    } else {
                        t!("terminal_panel.lifecycle.close_unsaved_title")
                    };
                    window.prompt(
                        gpui::PromptLevel::Warning,
                        &title,
                        Some(&t!(
                            "terminal_panel.lifecycle.unsaved_detail",
                            count = drafts
                        )),
                        &[
                            t!("terminal_panel.confirm.cancel").as_ref(),
                            t!("terminal_panel.lifecycle.discard_changes").as_ref(),
                            t!("terminal_panel.lifecycle.save_and_continue").as_ref(),
                        ],
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
                                    &t!("terminal_panel.lifecycle.save_failed_title"),
                                    Some(&t!(
                                        "terminal_panel.lifecycle.save_failed_detail",
                                        count = failed
                                    )),
                                    &[t!("terminal_panel.confirm.ok").as_ref()],
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
                            &t!("terminal_panel.lifecycle.check_failed_title"),
                            Some(&error.to_string()),
                            &[t!("terminal_panel.confirm.ok").as_ref()],
                            cx,
                        ));
                    });
                    return;
                }
            };
            if running > 0 {
                let answer = cx.update(|window, cx| {
                    let title = if quit {
                        t!("terminal_panel.lifecycle.quit_end_title")
                    } else {
                        t!("terminal_panel.lifecycle.close_end_title")
                    };
                    let confirm = if quit {
                        t!("terminal_panel.lifecycle.quit_holt")
                    } else {
                        t!("terminal_panel.lifecycle.close_window")
                    };
                    window.prompt(
                        gpui::PromptLevel::Warning,
                        &title,
                        Some(&t!("terminal_panel.confirm.end_detail", count = running)),
                        &[
                            t!("terminal_panel.confirm.cancel").as_ref(),
                            confirm.as_ref(),
                        ],
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
                        &t!("terminal_panel.lifecycle.close_failed_title"),
                        Some(&error.to_string()),
                        &[t!("terminal_panel.confirm.ok").as_ref()],
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
