//! The Goal card (ADR-0044, relocated): the chat's goal loop as a pinned
//! top-right overlay — the objective, the loop's status and round, the last
//! verdict's reason, and the pause/resume + clear controls. Replaces the
//! composer footer's goal chip: state answers to the conversation, not to
//! the next message, and the card can show the objective text the chip never
//! could. Rendered only on the Chat route, above the notice stack in the
//! same anchored column. The card collapses to a capsule (glyph + status
//! word, persisted in `UiSettings::goal_card_collapsed`); the swap tweens
//! the box's width/height between the two forms' measured sizes instead of
//! cutting.

use super::Shell;

use gpui::{AnyElement, Context, IntoElement, SharedString, div, prelude::*, px};

use holt_proto::{ChatGoalState, GoalStatus};
use holt_rpc::methods;

use crate::motion::{self, AnimationExt as _};
use crate::theme::Theme;

const CARD_WIDTH: f32 = 300.0;

/// The collapse/expand morph: the outgoing form's measured outer size at the
/// toggle instant. The box tweens from here toward the current form's LIVE
/// measured size (canvas-fed each frame), so a reflow mid-morph retargets
/// the lerp instead of finishing on a stale size. Manual evaluation, never
/// `with_animation` — element-id keying replays on remount (panes.rs
/// `WidthTween`'s rationale).
#[derive(Debug, Clone, Copy)]
pub(super) struct GoalCardMorph {
    from: (f32, f32),
    started: std::time::Instant,
}

impl Shell {
    /// The selected chat's goal card, or `None` when the chat carries no
    /// goal (never armed, cleared, or judged complete).
    pub(super) fn render_goal_card(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let goal = self
            .state
            .read(cx)
            .selected_chat_row()
            .map(|chat| chat.goal.clone())??;
        let collapsed = self.settings.goal_card_collapsed;
        let paused = goal.status == GoalStatus::Paused;
        // Distinct ids per form: the swap unmounts one, so the other's
        // entrance fade replays (opacity only — the morph moves the box,
        // the content never translates).
        let (id, content) = if collapsed {
            ("goal-card-pill", self.goal_card_pill(&goal, theme))
        } else {
            (
                "goal-card",
                self.goal_card_body("goal-card", &goal, theme, cx),
            )
        };

        // The in-flight box size: eased lerp from the toggle-time size to
        // the live measured target (previous frame — the bottom_stack
        // idiom). A completed morph clears to natural sizing.
        let natural = self.goal_card_size.get();
        let mut morph_size = None;
        if let Some(morph) = self.goal_card_morph {
            let total = motion::RESIZE.total().mul_f32(motion::speed_scale());
            let raw = morph.started.elapsed().as_secs_f32() / total.as_secs_f32();
            if raw >= 1.0 || self.reduced_motion {
                self.goal_card_morph = None;
            } else {
                let eased = motion::RESIZE.progress(raw);
                self.motion_active.set(true);
                morph_size = Some((
                    motion::lerp(morph.from.0, natural.0, eased),
                    motion::lerp(morph.from.1, natural.1, eased),
                ));
            }
        }

        // The floating box: the composer pill's chrome recipe — a faint
        // wash over the frost blur with a hairline border, never a solid
        // slab (composer.rs; a drop shadow under the translucent fill
        // paints through as an inner glow, so glass gets none). Clips to
        // the morph size while the content keeps its natural size, glued
        // to the top-right (the stack's top and the column's right edge
        // are both fixed, so only the left/bottom edges sweep).
        let measured = self.goal_card_size.clone();
        let mut card = div()
            .id(id)
            // The card floats over the transcript: keep its clicks from
            // landing on the rows beneath, but let wheel scroll through.
            .block_mouse_except_scroll()
            .overflow_hidden()
            .rounded(px(crate::popover::CARD_RADIUS))
            .border_1()
            .border_color(theme.border)
            .bg(if theme.is_frost() {
                theme.input_glass_bg()
            } else {
                theme.surface_overlay
            })
            .when(!theme.is_frost(), |el| el.shadow_lg())
            .when_some(morph_size, |el, (w, h)| el.w(px(w)).h(px(h)))
            .flex()
            .flex_col()
            .items_end()
            .child(
                div()
                    .relative()
                    .flex_none()
                    .child(
                        gpui::canvas(
                            move |bounds, _, _| {
                                measured.set((
                                    f32::from(bounds.size.width),
                                    f32::from(bounds.size.height),
                                ));
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .child(content),
            );
        if collapsed {
            // The capsule itself is the expand button.
            card = card
                .debug_selector(|| "goal-card-pill".to_string())
                .cursor_pointer()
                .tooltip(move |_, cx| {
                    cx.new(|_| crate::popover::TextTooltip("Expand the goal card".into()))
                        .into()
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_goal_card_collapsed(false, cx);
                }));
        }
        // The swap fades the content in over the morph; the paused dim
        // rides the SAME opacity write so the entrance can't clobber it.
        let base_opacity = if paused { 0.72 } else { 1.0 };
        let faded = card.with_animation(id, motion::FADE_QUICK.animation(), move |el, t| {
            el.opacity(t * base_opacity)
        });
        // `frosted` gives the card its own scene layer (the BadgeCard
        // precedent): sharing the transcript's layer lets bubble text paint
        // over it.
        Some(
            crate::frost::frosted(crate::popover::CARD_RADIUS, crate::frost::MENU_BLUR, faded)
                .into_any_element(),
        )
    }

    /// The capsule's content row: identity glyph, name, the loop's status
    /// word, and the expand affordance. Handlers live on the box
    /// (render_goal_card adds them in collapsed mode).
    fn goal_card_pill(&mut self, goal: &ChatGoalState, theme: &Theme) -> AnyElement {
        let status_tint = goal_status_tint(goal.status, theme);
        let blocked = goal.status == GoalStatus::Blocked;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .py(px(4.0))
            .pl(px(8.0))
            .pr(px(6.0))
            .child(
                crate::icons::icon(crate::icons::TARGET)
                    .size(px(13.0))
                    .flex_none()
                    .text_color(status_tint),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .line_height(px(18.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child("Goal"),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .line_height(px(18.0))
                    .text_color(if blocked {
                        status_tint
                    } else {
                        theme.text_faint
                    })
                    .child(goal_status_label(goal)),
            )
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(11.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .into_any_element()
    }

    /// The card's content column: header (identity, status, controls),
    /// the objective, and the last verdict's reason. The floating box
    /// chrome lives in `render_goal_card`; `id` scopes the buttons.
    fn goal_card_body(
        &mut self,
        id: &'static str,
        goal: &ChatGoalState,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let status_tint = goal_status_tint(goal.status, theme);
        let paused = goal.status == GoalStatus::Paused;
        let blocked = goal.status == GoalStatus::Blocked;
        let pause_id: SharedString = format!("{id}-pause").into();
        let clear_id: SharedString = format!("{id}-clear").into();
        let collapse_id: SharedString = format!("{id}-collapse").into();
        div()
            .w(px(CARD_WIDTH))
            .p(px(10.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                // Header: identity + loop state left, controls right.
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        crate::icons::icon(crate::icons::TARGET)
                            .size(px(13.0))
                            .flex_none()
                            .text_color(status_tint),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .line_height(px(18.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child("Goal"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.0))
                            .line_height(px(18.0))
                            .text_color(if blocked {
                                status_tint
                            } else {
                                theme.text_faint
                            })
                            .child(goal_status_label(goal)),
                    )
                    // Pause/resume: `SetGoalPaused`'s button form. The chat
                    // row's WatchChats republish repaints the card; a
                    // failure rides the notice strip.
                    .child(
                        div()
                            .id(pause_id)
                            .debug_selector(|| "goal-card-pause".to_string())
                            .flex_none()
                            .size(px(20.0))
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|el| el.bg(crate::theme::ink(0.06)))
                            .tooltip(move |_, cx| {
                                cx.new(|_| {
                                    crate::popover::TextTooltip(
                                        if paused {
                                            "Resume the goal (/goal resume)"
                                        } else {
                                            "Pause the goal (/goal pause)"
                                        }
                                        .into(),
                                    )
                                })
                                .into()
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_goal_paused(!paused, cx);
                            }))
                            .child(
                                crate::icons::icon(if paused {
                                    crate::icons::PLAY
                                } else {
                                    crate::icons::PAUSE
                                })
                                .size(px(13.0))
                                .text_color(theme.text_muted),
                            ),
                    )
                    // Clear: `/goal off`'s button form.
                    .child(
                        div()
                            .id(clear_id)
                            .debug_selector(|| "goal-card-clear".to_string())
                            .flex_none()
                            .size(px(20.0))
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|el| el.bg(crate::theme::ink(0.06)))
                            .tooltip(move |_, cx| {
                                cx.new(|_| {
                                    crate::popover::TextTooltip("Clear the goal (/goal off)".into())
                                })
                                .into()
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.clear_goal(cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::CLOSE_CIRCLE)
                                    .size(px(13.0))
                                    .text_color(theme.text_muted),
                            ),
                    )
                    // Collapse to the capsule: view chrome (persisted in
                    // `UiSettings::goal_card_collapsed`), not a goal action —
                    // the loop keeps running.
                    .child(
                        div()
                            .id(collapse_id)
                            .debug_selector(|| "goal-card-collapse".to_string())
                            .flex_none()
                            .size(px(20.0))
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|el| el.bg(crate::theme::ink(0.06)))
                            .tooltip(move |_, cx| {
                                cx.new(|_| {
                                    crate::popover::TextTooltip("Collapse the goal card".into())
                                })
                                .into()
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_goal_card_collapsed(true, cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::ALT_ARROW_UP)
                                    .size(px(13.0))
                                    .text_color(theme.text_muted),
                            ),
                    ),
            )
            .child(
                // The objective: two lines, the rest on hover.
                div()
                    .id("goal-card-text")
                    .min_w_0()
                    .text_size(px(12.0))
                    .line_height(px(17.0))
                    .text_color(theme.text)
                    .line_clamp(2)
                    .child(SharedString::from(goal.text.clone())),
            )
            .when_some(goal.last_reason.clone(), |el, reason| {
                el.child(
                    div()
                        .id("goal-card-reason")
                        .flex()
                        .flex_row()
                        .gap(px(5.0))
                        .items_start()
                        .text_size(px(11.0))
                        .line_height(px(15.0))
                        .text_color(if blocked {
                            status_tint
                        } else {
                            theme.text_faint
                        })
                        .when(blocked, |el| {
                            el.child(
                                crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                                    .size(px(11.0))
                                    .flex_none()
                                    .mt(px(2.0)),
                            )
                        })
                        .child(
                            div()
                                .min_w_0()
                                .line_clamp(2)
                                .child(SharedString::from(reason)),
                        ),
                )
            })
            .into_any_element()
    }

    /// Card ↔ capsule toggle: pure view chrome (the loop keeps running),
    /// persisted in `UiSettings::goal_card_collapsed`. The direct store
    /// write survives Shell saves — `ShellSettingsFields` doesn't own it.
    /// Starts the size morph from the outgoing form's measured size; a
    /// zero measurement (never painted) or reduced motion snaps instead.
    fn set_goal_card_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        let from = self.goal_card_size.get();
        self.goal_card_morph = if self.reduced_motion || from == (0.0, 0.0) {
            None
        } else {
            Some(GoalCardMorph {
                from,
                started: std::time::Instant::now(),
            })
        };
        self.settings.goal_card_collapsed = collapsed;
        crate::settings::update(crate::settings::SavePolicy::Debounced, cx, move |current| {
            current.goal_card_collapsed = collapsed;
        });
        cx.notify();
    }

    /// `SetGoalPaused` from the goal card's button: fire-and-forget — the
    /// WatchChats republish repaints the card, and only a failure needs the
    /// user told.
    fn set_goal_paused(&mut self, paused: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.push_holt_notice(
                crate::shell::HoltNoticeKind::Error,
                "Engine not connected — couldn't reach the goal.".into(),
                cx,
            );
            return;
        };
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let verb = if paused { "Pausing" } else { "Resuming" };
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SET_GOAL_PAUSED,
                    serde_json::json!({ "chatId": chat_id, "paused": paused }),
                )
                .await;
            if let Err(error) = result {
                tracing::warn!(error = %error, "SetGoalPaused from the goal card failed");
                let _ = this.update(cx, |this, cx| {
                    this.push_holt_notice(
                        crate::shell::HoltNoticeKind::Error,
                        format!("{verb} the goal failed: {error}").into(),
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    /// `ClearGoal` from the goal card's button (the retired chip's ×).
    fn clear_goal(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.push_holt_notice(
                crate::shell::HoltNoticeKind::Error,
                "Engine not connected — couldn't reach the goal.".into(),
                cx,
            );
            return;
        };
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::CLEAR_GOAL,
                    serde_json::json!({ "chatId": chat_id }),
                )
                .await;
            match result {
                Ok(_) => {
                    let _ = this.update(cx, |this, cx| {
                        this.push_holt_notice(
                            crate::shell::HoltNoticeKind::Plain,
                            "Goal cleared".into(),
                            cx,
                        );
                    });
                }
                Err(error) => {
                    tracing::warn!(error = %error, "ClearGoal from the goal card failed");
                    let _ = this.update(cx, |this, cx| {
                        this.push_holt_notice(
                            crate::shell::HoltNoticeKind::Error,
                            format!("Clearing the goal failed: {error}").into(),
                            cx,
                        );
                    });
                }
            }
        })
        .detach();
    }
}

/// One word group naming where the loop stands (the chip label's successor):
/// the running round while active, the parked state otherwise.
fn goal_status_label(goal: &ChatGoalState) -> SharedString {
    match goal.status {
        GoalStatus::Active if goal.iteration == 0 => "armed".into(),
        GoalStatus::Active => format!("round {}", goal.iteration).into(),
        GoalStatus::Paused => "paused".into(),
        GoalStatus::Blocked => "blocked".into(),
    }
}

/// Blocked reads at a glance (the verifier parked the loop); the other
/// states stay neutral — the card's job is presence, not alarm.
fn goal_status_tint(status: GoalStatus, theme: &Theme) -> gpui::Hsla {
    match status {
        GoalStatus::Blocked => crate::theme::AccentColor::Orange.primary(theme.appearance),
        _ => theme.text_muted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal(status: GoalStatus, iteration: u32) -> ChatGoalState {
        ChatGoalState {
            text: "ship the compaction slice".into(),
            status,
            iteration,
            no_progress: 0,
            eval_failures: 0,
            started_at: chrono::Utc::now(),
            last_reason: None,
        }
    }

    #[test]
    fn the_status_label_names_the_loop_state() {
        assert_eq!(goal_status_label(&goal(GoalStatus::Active, 0)), "armed");
        assert_eq!(goal_status_label(&goal(GoalStatus::Active, 7)), "round 7");
        assert_eq!(goal_status_label(&goal(GoalStatus::Paused, 7)), "paused");
        assert_eq!(goal_status_label(&goal(GoalStatus::Blocked, 7)), "blocked");
    }

    /// The drawing + wiring half: a chat with a goal draws the card with
    /// its controls, and the pause / clear buttons reach the loop's RPCs.
    /// (The mode-control spec's precedent — entity state tests assert the
    /// labels; this asserts the painted buttons do the work.)
    #[gpui::test]
    fn the_card_controls_drive_the_goal_rpcs(cx: &mut gpui::TestAppContext) {
        use std::sync::{Arc, Mutex};

        use crate::state::AppState;

        /// Records every goal RPC; every other method is unknown, so the
        /// standing AppState watches just retry on their timer.
        struct GoalEngine {
            calls: Mutex<Vec<(String, serde_json::Value)>>,
        }

        #[async_trait::async_trait]
        impl holt_rpc::RpcService for GoalEngine {
            async fn handle(
                &self,
                method: &str,
                params: serde_json::Value,
            ) -> Result<holt_rpc::RpcReply, holt_rpc::RpcError> {
                if [methods::SET_GOAL_PAUSED, methods::CLEAR_GOAL].contains(&method) {
                    self.calls
                        .lock()
                        .unwrap()
                        .push((method.to_string(), params));
                    return holt_rpc::RpcReply::value(&serde_json::json!({}));
                }
                Err(holt_rpc::RpcError::UnknownMethod(method.to_string()))
            }
        }

        struct GoalCardView {
            shell: gpui::Entity<Shell>,
        }
        impl gpui::Render for GoalCardView {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let theme = Theme::of(cx).clone();
                let card = self
                    .shell
                    .update(cx, |shell, cx| shell.render_goal_card(&theme, cx));
                div().children(card)
            }
        }

        // `memory_client` spawns its dispatch loop with `tokio::spawn`,
        // which needs a runtime context on this thread.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        cx.update(|cx| cx.set_global(Theme::default()));
        let engine = Arc::new(GoalEngine {
            calls: Mutex::new(Vec::new()),
        });
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.chats.push(
                serde_json::from_value(serde_json::json!({
                    "id": "goal-chat", "deviceId": "test-device", "archived": false,
                    "cwd": "/tmp", "createdAt": "2026-10-10T00:00:00Z",
                    "goal": {
                        "text": "ship the compaction slice",
                        "status": "active", "iteration": 3,
                        "noProgress": 0, "evalFailures": 0,
                        "startedAt": "2026-10-10T00:00:00Z"
                    }
                }))
                .unwrap(),
            );
            state.selected_chat = Some("goal-chat".into());
            state
        });
        let client = {
            let _guard = runtime.enter();
            holt_rpc::memory_client(engine.clone())
        };
        state.update(cx, |state, cx| state.attach_test_engine(client, cx));
        let shell = cx.new(|cx| {
            let mut shell = Shell::new(
                state,
                crate::state::EngineBootConfig {
                    data_dir: std::env::temp_dir(),
                },
                cx,
            );
            shell.route = super::super::Route::Chat;
            shell
        });
        // Sanity: the seeded chat draws a card at all (a goal-less chat
        // returns None — render_goal_card's contract).
        shell.update(cx, |shell, cx| {
            let theme = Theme::of(cx).clone();
            assert!(
                shell.render_goal_card(&theme, cx).is_some(),
                "a chat with a goal draws the card"
            );
        });
        let (_view, visual) = cx.add_window_view(|_, _| GoalCardView { shell });

        // The card paints its two controls (their debug selectors); a
        // goal-less chat would draw neither (render_goal_card returns None).
        let pause = visual
            .debug_bounds("goal-card-pause")
            .expect("the pause button renders with the card");
        let clear = visual
            .debug_bounds("goal-card-clear")
            .expect("the clear button renders with the card");

        visual.simulate_click(pause.center(), Default::default());
        visual.simulate_click(clear.center(), Default::default());
        // The RPCs are async over the in-memory transport: drive the
        // dispatch loop on the test thread, then the gpui foreground
        // executor.
        for _ in 0..8 {
            runtime.block_on(async { tokio::task::yield_now().await });
            cx.run_until_parked();
        }

        let calls = engine.calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "both buttons reached the engine");
        assert_eq!(calls[0].0, methods::SET_GOAL_PAUSED);
        assert_eq!(calls[0].1["chatId"], "goal-chat");
        assert_eq!(calls[0].1["paused"], true);
        assert_eq!(calls[1].0, methods::CLEAR_GOAL);
        assert_eq!(calls[1].1["chatId"], "goal-chat");
    }

    /// The card ↔ capsule switch is view chrome: the collapse button folds
    /// the card to the pill, the pill expands on click, and the choice is
    /// mirrored into the shell's persisted settings. No goal RPC fires —
    /// the loop keeps running either way.
    #[gpui::test]
    fn the_card_collapses_to_a_capsule_and_back(cx: &mut gpui::TestAppContext) {
        use std::sync::Arc;

        use crate::state::AppState;

        struct NoopEngine;

        #[async_trait::async_trait]
        impl holt_rpc::RpcService for NoopEngine {
            async fn handle(
                &self,
                method: &str,
                _params: serde_json::Value,
            ) -> Result<holt_rpc::RpcReply, holt_rpc::RpcError> {
                Err(holt_rpc::RpcError::UnknownMethod(method.to_string()))
            }
        }

        struct GoalCardView {
            shell: gpui::Entity<Shell>,
        }
        impl gpui::Render for GoalCardView {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let theme = Theme::of(cx).clone();
                let card = self
                    .shell
                    .update(cx, |shell, cx| shell.render_goal_card(&theme, cx));
                div().children(card)
            }
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.chats.push(
                serde_json::from_value(serde_json::json!({
                    "id": "goal-chat", "deviceId": "test-device", "archived": false,
                    "cwd": "/tmp", "createdAt": "2026-10-10T00:00:00Z",
                    "goal": {
                        "text": "ship the compaction slice",
                        "status": "active", "iteration": 3,
                        "noProgress": 0, "evalFailures": 0,
                        "startedAt": "2026-10-10T00:00:00Z"
                    }
                }))
                .unwrap(),
            );
            state.selected_chat = Some("goal-chat".into());
            state
        });
        let client = {
            let _guard = runtime.enter();
            holt_rpc::memory_client(Arc::new(NoopEngine))
        };
        state.update(cx, |state, cx| state.attach_test_engine(client, cx));
        let shell = cx.new(|cx| {
            let mut shell = Shell::new(
                state,
                crate::state::EngineBootConfig {
                    data_dir: std::env::temp_dir(),
                },
                cx,
            );
            shell.route = super::super::Route::Chat;
            shell
        });
        let (_view, visual) = cx.add_window_view(|_, _| GoalCardView {
            shell: shell.clone(),
        });

        // Expanded by default: the card's controls, not the pill.
        assert!(visual.debug_bounds("goal-card-collapse").is_some());
        assert!(visual.debug_bounds("goal-card-pill").is_none());

        // Fold to the capsule: the pill replaces the card and the choice
        // lands on the shell's settings (the persisted copy).
        let collapse = visual.debug_bounds("goal-card-collapse").unwrap();
        visual.simulate_click(collapse.center(), Default::default());
        visual.run_until_parked();
        assert!(visual.read(|cx| shell.read(cx).settings.goal_card_collapsed));
        assert!(visual.debug_bounds("goal-card-pill").is_some());
        assert!(visual.debug_bounds("goal-card-pause").is_none());

        // The capsule expands back on click.
        let pill = visual.debug_bounds("goal-card-pill").unwrap();
        visual.simulate_click(pill.center(), Default::default());
        visual.run_until_parked();
        assert!(!visual.read(|cx| shell.read(cx).settings.goal_card_collapsed));
        assert!(visual.debug_bounds("goal-card-pause").is_some());
    }
}
