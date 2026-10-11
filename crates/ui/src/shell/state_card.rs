//! The chat-state card: the goal loop (ADR-0044) and the Plan/Provider
//! Mode states (ADR-0025/0037) as a pinned top-right overlay — one card,
//! since the three states are mutually exclusive. The goal form shows the
//! objective, the loop's status and round, the last verdict's reason, and
//! the pause/resume + clear controls (it replaced the composer footer's
//! goal chip); the mode form shows the active mode with the button forms
//! of `/plan off` and `/provider off` (the mode-flag chips' successor).
//! State answers to the conversation, not to the next message. Rendered
//! only on the Chat route, above the notice stack in the same anchored
//! column. The goal form collapses to a capsule (glyph + status word,
//! persisted in `UiSettings::state_card_collapsed`); the swap tweens
//! the box's width/height between the two forms' measured sizes instead of
//! cutting.

use super::Shell;

use gpui::{AnyElement, Context, IntoElement, SharedString, div, prelude::*, px};

use holt_proto::{Chat, ChatGoalState, GoalStatus};
use holt_rpc::methods;

use crate::motion::{self, AnimationExt as _};
use crate::theme::Theme;

const CARD_WIDTH: f32 = 300.0;

/// What the mode form shows and does for one active mode. Plan wins when
/// both flags are somehow set — the engine keeps the modes exclusive; the
/// UI reads the chat row and picks deterministically.
struct ModeCard {
    icon: &'static str,
    name: &'static str,
    /// The mode's status word, after the name.
    status: &'static str,
    blurb: SharedString,
    /// The off button's tooltip: names the slash form it mirrors.
    tooltip: &'static str,
    method: &'static str,
}

fn mode_card_for(chat: &Chat) -> Option<ModeCard> {
    if let Some(plan) = chat.plan_mode.as_ref() {
        return Some(ModeCard {
            icon: crate::icons::CHECKLIST,
            name: "Plan Mode",
            status: "planning",
            blurb: format!(
                "Read-only planning. Approving a plan exits and restores the {} permission mode.",
                crate::pickers::mode_label(plan.entry_permission_mode)
            )
            .into(),
            tooltip: "Leave Plan Mode (/plan off)",
            method: methods::EXIT_PLAN_MODE,
        });
    }
    if chat.provider_mode {
        return Some(ModeCard {
            icon: crate::icons::CLOUD,
            name: "Provider Mode",
            status: "setup",
            blurb: "Catalog setup — the conversation adds and updates providers.".into(),
            tooltip: "Leave Provider Mode (/provider off)",
            method: methods::EXIT_PROVIDER_MODE,
        });
    }
    None
}

/// The floating box both card forms share — the composer pill's chrome
/// recipe: a faint wash over the frost blur with a hairline border, never
/// a solid slab (composer.rs; a drop shadow under the translucent fill
/// paints through as an inner glow, so glass gets none).
fn state_card_chrome(theme: &Theme) -> gpui::Div {
    div()
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
}

/// The entrance fade, then the card's own frost scene layer (the
/// BadgeCard precedent): sharing the transcript's layer lets bubble text
/// paint over it.
fn fade_and_frost(
    el: gpui::Stateful<gpui::Div>,
    id: &'static str,
    base_opacity: f32,
) -> AnyElement {
    let faded = el.with_animation(id, motion::FADE_QUICK.animation(), move |el, t| {
        el.opacity(t * base_opacity)
    });
    crate::frost::frosted(crate::popover::CARD_RADIUS, crate::frost::MENU_BLUR, faded)
        .into_any_element()
}

/// The collapse/expand morph: the outgoing form's measured outer size at the
/// toggle instant. The box tweens from here toward the current form's LIVE
/// measured size (canvas-fed each frame), so a reflow mid-morph retargets
/// the lerp instead of finishing on a stale size. Manual evaluation, never
/// `with_animation` — element-id keying replays on remount (panes.rs
/// `WidthTween`'s rationale).
#[derive(Debug, Clone, Copy)]
pub(super) struct StateCardMorph {
    from: (f32, f32),
    started: std::time::Instant,
}

impl Shell {
    /// The selected chat's state card, or `None` when the chat carries
    /// none of the states: the goal loop first, then Plan/Provider Mode
    /// (mutually exclusive; the UI picks deterministically).
    pub(super) fn render_state_card(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let chat = self.state.read(cx).selected_chat_row()?.clone();
        let Some(goal) = chat.goal.clone() else {
            let mode = mode_card_for(&chat)?;
            return Some(self.mode_card(mode, theme, cx));
        };
        let collapsed = self.settings.state_card_collapsed;
        let paused = goal.status == GoalStatus::Paused;
        // Distinct ids per form: the swap unmounts one, so the other's
        // entrance fade replays (opacity only — the morph moves the box,
        // the content never translates).
        let (id, content) = if collapsed {
            ("goal-card-pill", self.goal_pill(&goal, theme))
        } else {
            ("goal-card", self.goal_body("goal-card", &goal, theme, cx))
        };

        // The in-flight box size: eased lerp from the toggle-time size to
        // the live measured target (previous frame — the bottom_stack
        // idiom). A completed morph clears to natural sizing.
        let natural = self.state_card_size.get();
        let mut morph_size = None;
        if let Some(morph) = self.state_card_morph {
            let total = motion::RESIZE.total().mul_f32(motion::speed_scale());
            let raw = morph.started.elapsed().as_secs_f32() / total.as_secs_f32();
            if raw >= 1.0 || self.reduced_motion {
                self.state_card_morph = None;
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
        let measured = self.state_card_size.clone();
        let mut card = state_card_chrome(theme)
            .id(id)
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
                    cx.new(|_| crate::popover::TextTooltip("Expand the card".into()))
                        .into()
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_state_card_collapsed(false, cx);
                }));
        }
        // The swap fades the content in over the morph; the paused dim
        // rides the SAME opacity write so the entrance can't clobber it.
        let base_opacity = if paused { 0.72 } else { 1.0 };
        Some(fade_and_frost(card, id, base_opacity))
    }

    /// The mode body inside the shared chrome: identity glyph, name, the
    /// status word, and the off button over the one-line blurb. No
    /// collapse — modes change rarely and the card is small.
    fn mode_card(&mut self, mode: ModeCard, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let method = mode.method;
        let tooltip: SharedString = mode.tooltip.into();
        let content = div()
            .w(px(CARD_WIDTH))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .px(px(12.0))
            .py(px(10.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        crate::icons::icon(mode.icon)
                            .size(px(13.0))
                            .text_color(theme.text_muted),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(theme.text)
                            .child(SharedString::from(mode.name)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.0))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(mode.status)),
                    )
                    .child(
                        div()
                            .id("mode-card-off")
                            .debug_selector(|| "mode-card-off".to_string())
                            .flex_none()
                            .size(px(20.0))
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|el| el.bg(crate::theme::ink(0.06)))
                            .tooltip(move |_, cx| {
                                cx.new(|_| crate::popover::TextTooltip(tooltip.clone()))
                                    .into()
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.exit_mode_from_card(method, cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::CLOSE_CIRCLE)
                                    .size(px(13.0))
                                    .text_color(theme.text_muted),
                            ),
                    ),
            )
            .child(
                div()
                    .min_w_0()
                    .text_size(px(11.0))
                    .line_height(px(15.0))
                    .text_color(theme.text_faint)
                    .child(mode.blurb),
            );
        let card = state_card_chrome(theme).id("mode-card").child(content);
        fade_and_frost(card, "mode-card", 1.0)
    }

    /// `ExitPlanMode` / `ExitProviderMode` from the mode form's off button
    /// (the slash forms' button twin). Fire-and-forget — the WatchChats
    /// republish unmounts the card, which is confirmation enough; only a
    /// failure needs the notice strip.
    fn exit_mode_from_card(&mut self, method: &'static str, cx: &mut Context<Self>) {
        let mode = if method == methods::EXIT_PLAN_MODE {
            "Plan"
        } else {
            "Provider"
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.push_holt_notice(
                super::HoltNoticeKind::Error,
                format!("Engine not connected — couldn't reach {mode} Mode.").into(),
                cx,
            );
            return;
        };
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Err(error) = engine
                .client()
                .call(method, serde_json::json!({ "chatId": chat_id }))
                .await
            {
                tracing::warn!(error = %error, method, "leaving a mode from the state card failed");
                let _ = this.update(cx, |this, cx| {
                    this.push_holt_notice(
                        super::HoltNoticeKind::Error,
                        format!("Leaving {mode} Mode failed: {error}").into(),
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    /// The capsule's content row: identity glyph, the goal's name, the
    /// loop's status word, and the expand affordance. Handlers live on the
    /// box (render_state_card adds them in collapsed mode).
    fn goal_pill(&mut self, goal: &ChatGoalState, theme: &Theme) -> AnyElement {
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
                    .max_w(px(140.0))
                    .truncate()
                    .debug_selector(|| "goal-card-name".to_string())
                    .text_size(px(12.0))
                    .line_height(px(18.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(goal_name(goal)),
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
    /// chrome lives in `render_state_card`; `id` scopes the buttons.
    fn goal_body(
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
                            .max_w(px(150.0))
                            .truncate()
                            .debug_selector(|| "goal-card-name".to_string())
                            .text_size(px(12.0))
                            .line_height(px(18.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(goal_name(goal)),
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
                    // `UiSettings::state_card_collapsed`), not a goal action —
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
                                cx.new(|_| crate::popover::TextTooltip("Collapse the card".into()))
                                    .into()
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_state_card_collapsed(true, cx);
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
            // The verifier-maintained plan: a finished step reads struck
            // and faint behind its check, a pending one plain behind an
            // empty ring.
            .when(!goal.steps.is_empty(), |el| {
                el.child(div().flex().flex_col().gap(px(4.0)).children(
                    goal.steps.iter().enumerate().map(|(ix, step)| {
                        let selector = format!("goal-card-step-{ix}");
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .debug_selector(move || selector.clone())
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(12.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(if step.done {
                                        crate::icons::icon(crate::icons::CHECK)
                                            .size(px(11.0))
                                            .text_color(
                                                crate::theme::AccentColor::Green
                                                    .primary(theme.appearance),
                                            )
                                            .into_any_element()
                                    } else {
                                        div()
                                            .size(px(8.0))
                                            .rounded_full()
                                            .border_1()
                                            .border_color(theme.text_faint)
                                            .into_any_element()
                                    }),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(11.0))
                                    .line_height(px(15.0))
                                    .text_color(if step.done {
                                        theme.text_faint
                                    } else {
                                        theme.text_muted
                                    })
                                    .when(step.done, |el| el.line_through())
                                    .child(SharedString::from(step.text.clone())),
                            )
                            .into_any_element()
                    }),
                ))
            })
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
    /// persisted in `UiSettings::state_card_collapsed`. The direct store
    /// write survives Shell saves — `ShellSettingsFields` doesn't own it.
    /// Starts the size morph from the outgoing form's measured size; a
    /// zero measurement (never painted) or reduced motion snaps instead.
    fn set_state_card_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        let from = self.state_card_size.get();
        self.state_card_morph = if self.reduced_motion || from == (0.0, 0.0) {
            None
        } else {
            Some(StateCardMorph {
                from,
                started: std::time::Instant::now(),
            })
        };
        self.settings.state_card_collapsed = collapsed;
        crate::settings::update(crate::settings::SavePolicy::Debounced, cx, move |current| {
            current.state_card_collapsed = collapsed;
        });
        cx.notify();
    }

    /// `SetGoalPaused` from the goal form's button: fire-and-forget — the
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
                tracing::warn!(error = %error, "SetGoalPaused from the state card failed");
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

    /// `ClearGoal` from the goal form's button (the retired chip's ×).
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
                    tracing::warn!(error = %error, "ClearGoal from the state card failed");
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

/// The card's name for the goal: the verifier-given short name, or the
/// bare label until the first pass names it (never the raw objective).
fn goal_name(goal: &ChatGoalState) -> SharedString {
    goal.name.clone().unwrap_or_else(|| "Goal".into()).into()
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
            name: None,
            steps: Vec::new(),
            status,
            iteration,
            no_progress: 0,
            eval_failures: 0,
            started_at: chrono::Utc::now(),
            last_reason: None,
        }
    }

    #[test]
    fn the_card_names_the_goal_from_the_verifier() {
        // Until the first verifier pass names the goal, the card carries
        // the bare label — never the raw objective.
        assert_eq!(goal_name(&goal(GoalStatus::Active, 0)), "Goal");
        let mut named = goal(GoalStatus::Active, 2);
        named.name = Some("Ship the compaction slice".into());
        assert_eq!(goal_name(&named), "Ship the compaction slice");
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
                    .update(cx, |shell, cx| shell.render_state_card(&theme, cx));
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
        // returns None — render_state_card's contract).
        shell.update(cx, |shell, cx| {
            let theme = Theme::of(cx).clone();
            assert!(
                shell.render_state_card(&theme, cx).is_some(),
                "a chat with a goal draws the card"
            );
        });
        let (_view, visual) = cx.add_window_view(|_, _| GoalCardView { shell });

        // The card paints its two controls (their debug selectors); a
        // goal-less chat would draw neither (render_state_card returns None).
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
                    .update(cx, |shell, cx| shell.render_state_card(&theme, cx));
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
        assert!(visual.read(|cx| shell.read(cx).settings.state_card_collapsed));
        assert!(visual.debug_bounds("goal-card-pill").is_some());
        assert!(visual.debug_bounds("goal-card-pause").is_none());

        // The capsule expands back on click.
        let pill = visual.debug_bounds("goal-card-pill").unwrap();
        visual.simulate_click(pill.center(), Default::default());
        visual.run_until_parked();
        assert!(!visual.read(|cx| shell.read(cx).settings.state_card_collapsed));
        assert!(visual.debug_bounds("goal-card-pause").is_some());
    }

    /// The verifier-maintained card content: the goal's name heads the card
    /// and its steps render in order — done ones styled complete.
    #[gpui::test]
    fn the_card_shows_the_name_and_steps(cx: &mut gpui::TestAppContext) {
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
                    .update(cx, |shell, cx| shell.render_state_card(&theme, cx));
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
                        "text": "ship the compaction slice and prove it holds",
                        "name": "Ship compaction",
                        "status": "active", "iteration": 2,
                        "noProgress": 0, "evalFailures": 0,
                        "startedAt": "2026-10-10T00:00:00Z",
                        "steps": [
                            { "text": "Freeze the change set", "done": true },
                            { "text": "Wire the verifier", "done": true },
                            { "text": "Add the regression test", "done": false }
                        ]
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
        let chats = state.clone();
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
        let (_view, visual) = cx.add_window_view(|_, _| GoalCardView { shell });

        // The header paints the verifier-given name: its element's laid-out
        // width tracks the content (the test text system shapes every glyph
        // at a fixed advance), so "Ship compaction" (15 chars) paints far
        // wider than the 4-char "Goal" fallback. When the name clears, the
        // card repaints at the fallback's width — the raw objective never
        // heads the card.
        let named = visual
            .debug_bounds("goal-card-name")
            .expect("the name element paints");
        visual.update(|_, cx| {
            chats.update(cx, |state, _| {
                state.chats[0].goal.as_mut().unwrap().name = None;
            });
            cx.refresh_windows();
        });
        visual.run_until_parked();
        let unnamed = visual
            .debug_bounds("goal-card-name")
            .expect("the fallback still paints");
        assert!(
            f32::from(named.size.width) > f32::from(unnamed.size.width) * 2.0,
            "the name element tracks goal.name (named {named:?}, fallback {unnamed:?})"
        );

        // Every step row renders; a goal without steps draws none (the
        // collapse test's fixture above asserts the same card's controls).
        for selector in ["goal-card-step-0", "goal-card-step-1", "goal-card-step-2"] {
            assert!(
                visual.debug_bounds(selector).is_some(),
                "{selector} renders"
            );
        }
        assert!(visual.debug_bounds("goal-card-step-3").is_none());
    }

    /// The mode form: a Plan/Provider chat (no goal) draws the off button,
    /// and it reaches the right exit RPC with a notice-strip confirmation.
    #[test]
    fn the_card_names_the_active_mode_or_none() {
        use holt_proto::{ChatPlanState, PermissionMode};

        let mut chat = plain_chat("mode-chat");
        assert!(mode_card_for(&chat).is_none());

        chat.plan_mode = Some(ChatPlanState {
            entry_permission_mode: PermissionMode::AutoReview,
        });
        let card = mode_card_for(&chat).expect("a planning chat draws the plan form");
        assert_eq!(card.name, "Plan Mode");
        assert_eq!(card.method, methods::EXIT_PLAN_MODE);
        assert!(
            card.blurb.contains("Auto review"),
            "the blurb names the restored permission mode"
        );

        // Plan wins when both flags are somehow set (the engine keeps the
        // modes exclusive; the UI picks deterministically).
        chat.provider_mode = true;
        assert_eq!(mode_card_for(&chat).unwrap().name, "Plan Mode");

        let mut providing = plain_chat("mode-chat");
        providing.provider_mode = true;
        let card = mode_card_for(&providing).expect("a provider chat draws the provider form");
        assert_eq!(card.name, "Provider Mode");
        assert_eq!(card.method, methods::EXIT_PROVIDER_MODE);
    }

    fn plain_chat(id: &str) -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": id, "deviceId": "test-device", "archived": false,
            "cwd": "/tmp", "createdAt": "2026-10-10T00:00:00Z"
        }))
        .unwrap()
    }

    /// The drawing + wiring half: the goal form leads while a chat somehow
    /// carries a goal and a mode; once the goal is gone the mode form takes
    /// over, and its off button reaches `ExitPlanMode` / `ExitProviderMode`
    /// — the same surface the slash forms drive.
    #[gpui::test]
    fn the_mode_form_off_button_exits_the_active_mode(cx: &mut gpui::TestAppContext) {
        use std::sync::{Arc, Mutex};

        use crate::state::AppState;
        use holt_proto::{ChatPlanState, PermissionMode};

        /// Records both exit RPCs; every other method is unknown, so the
        /// standing AppState watches just retry on their timer.
        struct ModeEngine {
            calls: Mutex<Vec<(String, serde_json::Value)>>,
        }

        #[async_trait::async_trait]
        impl holt_rpc::RpcService for ModeEngine {
            async fn handle(
                &self,
                method: &str,
                params: serde_json::Value,
            ) -> Result<holt_rpc::RpcReply, holt_rpc::RpcError> {
                if [methods::EXIT_PLAN_MODE, methods::EXIT_PROVIDER_MODE].contains(&method) {
                    self.calls
                        .lock()
                        .unwrap()
                        .push((method.to_string(), params));
                    return holt_rpc::RpcReply::value(&serde_json::json!({ "active": false }));
                }
                Err(holt_rpc::RpcError::UnknownMethod(method.to_string()))
            }
        }

        struct CardView {
            shell: gpui::Entity<Shell>,
        }
        impl gpui::Render for CardView {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let theme = Theme::of(cx).clone();
                let card = self
                    .shell
                    .update(cx, |shell, cx| shell.render_state_card(&theme, cx));
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
        let engine = Arc::new(ModeEngine {
            calls: Mutex::new(Vec::new()),
        });
        let state = cx.new(|_| {
            let mut state = AppState::new();
            let mut chat = plain_chat("mode-chat");
            chat.goal = Some(
                serde_json::from_value(serde_json::json!({
                    "text": "ship it", "status": "active", "iteration": 1,
                    "noProgress": 0, "evalFailures": 0,
                    "startedAt": "2026-10-10T00:00:00Z"
                }))
                .unwrap(),
            );
            chat.plan_mode = Some(ChatPlanState {
                entry_permission_mode: PermissionMode::ConfirmChanges,
            });
            state.chats.push(chat);
            state.selected_chat = Some("mode-chat".into());
            state
        });
        let client = {
            let _guard = runtime.enter();
            holt_rpc::memory_client(engine.clone())
        };
        state.update(cx, |state, cx| state.attach_test_engine(client, cx));
        let shell = cx.new(|cx| {
            let mut shell = Shell::new(
                state.clone(),
                crate::state::EngineBootConfig {
                    data_dir: std::env::temp_dir(),
                },
                cx,
            );
            shell.route = super::super::Route::Chat;
            shell
        });
        let (_view, visual) = cx.add_window_view(|_, _| CardView {
            shell: shell.clone(),
        });
        let pump = |visual: &mut gpui::VisualTestContext, runtime: &tokio::runtime::Runtime| {
            for _ in 0..8 {
                runtime.block_on(async { tokio::task::yield_now().await });
                visual.run_until_parked();
            }
        };

        // Goal wins while both states are somehow set.
        assert!(
            visual.debug_bounds("goal-card-pause").is_some(),
            "the goal form leads"
        );
        assert!(visual.debug_bounds("mode-card-off").is_none());

        // The goal is gone: the plan form takes over, and its off button
        // exits Plan Mode.
        state.update(visual, |state, cx| {
            let chat = state.chats.first_mut().unwrap();
            chat.goal = None;
            cx.notify();
        });
        pump(visual, &runtime);

        let off = visual
            .debug_bounds("mode-card-off")
            .expect("the plan form's off button renders");
        visual.simulate_click(off.center(), Default::default());
        pump(visual, &runtime);

        assert_eq!(
            engine.calls.lock().unwrap().as_slice(),
            &[(
                methods::EXIT_PLAN_MODE.to_string(),
                serde_json::json!({ "chatId": "mode-chat" })
            )]
        );
        shell.read_with(visual, |shell, _| {
            assert!(
                shell.holt_notices.is_empty(),
                "the unmounting card is confirmation enough"
            );
        });

        // The same form serves Provider Mode: flip the chat row, the plan
        // form unmounts, the provider form's button sends the other exit.
        state.update(visual, |state, cx| {
            let chat = state.chats.first_mut().unwrap();
            chat.plan_mode = None;
            chat.provider_mode = true;
            cx.notify();
        });
        pump(visual, &runtime);

        let off = visual
            .debug_bounds("mode-card-off")
            .expect("the provider form's off button renders");
        visual.simulate_click(off.center(), Default::default());
        pump(visual, &runtime);

        let calls = engine.calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "both forms reached the engine");
        assert_eq!(calls[1].0, methods::EXIT_PROVIDER_MODE);
        assert_eq!(calls[1].1["chatId"], "mode-chat");
        shell.read_with(visual, |shell, _| {
            assert!(shell.holt_notices.is_empty(), "no success toast");
        });
    }
}
