//! The chat-state card: the goal loop (ADR-goal-mode-verified-loop-at-turn-settle) and the Plan/Provider
//! Mode states (ADR-plan-mode-is-an-independent-approval-checkpoint/0037) as a pinned top-right overlay — one card,
//! since the three states are mutually exclusive. The goal form shows the
//! objective, the loop's status and round, the last verdict's reason, and
//! the pause/resume + clear controls (it replaced the composer footer's
//! goal chip); the mode form shows the active mode with the button forms
//! of `/plan off` and `/provider off` (the mode-flag chips' successor).
//! State answers to the conversation, not to the next message. Rendered
//! only on the Chat route, above the notice stack in the same anchored
//! column. The goal form collapses to a capsule (glyph + status word,
//! persisted in `UiSettings::state_card_expanded`); the swap tweens
//! the box's width/height between the two forms' measured sizes instead of
//! cutting.

use super::Shell;

use gpui::{AnyElement, Context, IntoElement, SharedString, div, prelude::*, px};

use holt_proto::{Chat, ChatGoalState, GoalStatus};
use holt_rpc::methods;

use crate::goal::LoopPhase;
use crate::motion::{self, AnimationExt as _};
use crate::theme::Theme;

const CARD_WIDTH: f32 = 300.0;
/// The open card's body hangs under the name: the 13px target glyph plus
/// the header's 6px gap.
const BODY_INDENT: f32 = 19.0;

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
        let collapsed = !self.settings.state_card_expanded;
        let phase = crate::goal::loop_phase(self.state.read(cx)).unwrap_or(LoopPhase::Working);
        // Distinct ids per form: the swap unmounts one, so the other's
        // entrance fade replays (opacity only — the morph moves the box,
        // the content never translates).
        let (id, content) = if collapsed {
            ("goal-card-pill", self.goal_pill(&goal, phase, theme, cx))
        } else {
            (
                "goal-card",
                self.goal_body("goal-card", &goal, phase, theme, cx),
            )
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
                .tooltip({
                    let title = goal_title(&goal);
                    move |_, cx| {
                        let title = title.clone();
                        cx.new(|_| crate::popover::TextTooltip(title)).into()
                    }
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_state_card_collapsed(false, cx);
                }));
        }
        // The swap fades the content in over the morph. A paused loop stays
        // fully legible — the status word says it stopped, the card itself
        // never reads as disabled.
        Some(fade_and_frost(card, id, 1.0))
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

    /// The capsule's content row: identity glyph, the loop's status word,
    /// and the expand affordance (the name rides the tooltip). Handlers live on the
    /// box (render_state_card adds them in collapsed mode).
    fn goal_pill(
        &mut self,
        goal: &ChatGoalState,
        phase: LoopPhase,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let status_tint = goal_status_tint(phase, theme);
        div()
            .h(px(28.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .pl(px(9.0))
            .pr(px(7.0))
            .child(
                crate::icons::icon(crate::icons::TARGET)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(status_tint),
            )
            // Glyph + phase only: the name lives in the tooltip and the open
            // card, so the folded capsule stays small enough to sit in the
            // corner without covering the transcript.
            .child(goal_status(goal, phase, theme, cx))
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(10.0))
                    .flex_none()
                    .text_color(theme.text_faint),
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
        phase: LoopPhase,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let status_tint = goal_status_tint(phase, theme);
        // Paused and blocked both resume (`SetGoalPaused(false)` re-arms
        // either); only a live loop offers the pause.
        let stopped = goal.status != GoalStatus::Active;
        let blocked = goal.status == GoalStatus::Blocked;
        let pause_id: SharedString = format!("{id}-pause").into();
        let clear_id: SharedString = format!("{id}-clear").into();
        let collapse_id: SharedString = format!("{id}-collapse").into();
        let named = goal.name.is_some();
        let button = |id: SharedString,
                      selector: &'static str,
                      icon: &'static str,
                      tooltip: &'static str| {
            div()
                .id(id)
                .debug_selector(move || selector.to_string())
                .flex_none()
                .size(px(22.0))
                .rounded(px(6.0))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|el| el.bg(crate::theme::ink(0.06)))
                .tooltip(move |_, cx| {
                    cx.new(|_| crate::popover::TextTooltip(tooltip.into()))
                        .into()
                })
                // The svg paints its own color — it never inherits the
                // container's text color.
                .child(
                    crate::icons::icon(icon)
                        .size(px(12.0))
                        .text_color(theme.text_muted),
                )
        };
        div()
            .w(px(CARD_WIDTH))
            .pl(px(12.0))
            .pr(px(6.0))
            .pt(px(6.0))
            .pb(px(10.0))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                // Header: identity + name, then the controls. The name is
                // the verifier's short one; until the first pass names it,
                // the objective itself heads the card (one line) rather than
                // a contentless "Goal".
                div()
                    .h(px(26.0))
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
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .debug_selector(|| "goal-card-name".to_string())
                            .text_size(px(12.5))
                            .line_height(px(18.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(goal_title(goal)),
                    )
                    // Pause/resume: `SetGoalPaused`'s button form. The chat
                    // row's WatchChats republish repaints the card; a
                    // failure rides the notice strip.
                    .child(
                        button(
                            pause_id,
                            "goal-card-pause",
                            if stopped {
                                crate::icons::PLAY
                            } else {
                                crate::icons::PAUSE
                            },
                            if stopped {
                                "Resume the goal (/goal resume)"
                            } else {
                                "Pause the goal (/goal pause)"
                            },
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_goal_paused(!stopped, cx);
                        })),
                    )
                    // Clear: `/goal off`'s button form.
                    .child(
                        button(
                            clear_id,
                            "goal-card-clear",
                            crate::icons::CLOSE_CIRCLE,
                            "Clear the goal (/goal off)",
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.clear_goal(cx);
                        })),
                    )
                    // Collapse to the capsule: view chrome (persisted in
                    // `UiSettings::state_card_expanded`), not a goal action —
                    // the loop keeps running.
                    .child(
                        button(
                            collapse_id,
                            "goal-card-collapse",
                            crate::icons::ALT_ARROW_UP,
                            "Collapse the card",
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_state_card_collapsed(true, cx);
                        })),
                    ),
            )
            // The live status, aligned with the name's first glyph.
            .child(
                div()
                    .pl(px(19.0))
                    .pr(px(6.0))
                    .child(goal_status(goal, phase, theme, cx)),
            )
            // The objective under a verifier-given name: secondary, two
            // lines. Unnamed, the header already carries it.
            .when(named, |el| {
                el.child(
                    div()
                        .id("goal-card-text")
                        .debug_selector(|| "goal-card-text".to_string())
                        .min_w_0()
                        .pl(px(BODY_INDENT))
                        .pr(px(6.0))
                        .mt(px(2.0))
                        .text_size(px(11.5))
                        .line_height(px(16.0))
                        .text_color(theme.text_muted)
                        .line_clamp(2)
                        .child(SharedString::from(goal.text.clone())),
                )
            })
            // The verifier-maintained plan: a finished step reads struck
            // and faint behind its check, a pending one plain behind an
            // empty ring.
            .when(!goal.steps.is_empty(), |el| {
                el.child(
                    div()
                        .pl(px(BODY_INDENT))
                        .pr(px(6.0))
                        .mt(px(4.0))
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .children(goal.steps.iter().enumerate().map(|(ix, step)| {
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
                        })),
                )
            })
            .when_some(goal_reason_line(goal), |el, reason| {
                el.child(
                    div()
                        .id("goal-card-reason")
                        .pl(px(BODY_INDENT))
                        .pr(px(6.0))
                        .mt(px(2.0))
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
    /// persisted in `UiSettings::state_card_expanded`. The direct store
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
        self.settings.state_card_expanded = !collapsed;
        crate::settings::update(crate::settings::SavePolicy::Debounced, cx, move |current| {
            current.state_card_expanded = !collapsed;
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

/// The card's name for the goal: the verifier-given short name, or — until
/// the first pass names it — the objective's first line, so the header
/// always says something about this goal (a bare "Goal" said nothing).
fn goal_title(goal: &ChatGoalState) -> SharedString {
    match &goal.name {
        Some(name) => name.clone().into(),
        None => goal
            .text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("Goal")
            .trim()
            .to_string()
            .into(),
    }
}

/// One word group naming what the loop is doing right now: the round it is
/// on while a Turn runs, the verifier's pass between Turns, and the parked
/// states. Round 0 is the arming Turn — "working", never jargon.
fn goal_status_label(goal: &ChatGoalState, phase: LoopPhase) -> SharedString {
    let round = |verb: &str| -> SharedString {
        if goal.iteration == 0 {
            verb.to_string().into()
        } else {
            format!("round {} · {verb}", goal.iteration).into()
        }
    };
    match phase {
        LoopPhase::Working => round("working"),
        LoopPhase::Checking => "checking".into(),
        LoopPhase::Queued => "after your queue".into(),
        LoopPhase::AwaitingAnswer => "waiting for you".into(),
        LoopPhase::Paused => "paused".into(),
        LoopPhase::Blocked => "blocked".into(),
    }
}

/// The status word group with its live affordance: a running phase leads
/// with the transcript's mini spinner, so the card visibly moves while the
/// loop does.
fn goal_status(
    goal: &ChatGoalState,
    phase: LoopPhase,
    theme: &Theme,
    cx: &mut Context<Shell>,
) -> AnyElement {
    // A live loop's word breathes (the compaction label's pulse); a parked
    // one holds still. No dot — the word is the whole signal.
    let live = matches!(phase, LoopPhase::Working | LoopPhase::Checking);
    let opacity = if live {
        let beat = motion::pulse_delta(&motion::HOLT_PULSE, cx.entity_id(), cx);
        motion::lerp(0.5, 1.0, motion::pulse_wave(beat))
    } else {
        1.0
    };
    div()
        .flex_none()
        .text_size(px(11.0))
        .line_height(px(16.0))
        .text_color(goal_status_tint(phase, theme))
        .opacity(opacity)
        .child(goal_status_label(goal, phase))
        .into_any_element()
}

/// Blocked reads at a glance (the verifier parked the loop) and an open
/// question asks for the user; the other phases stay neutral — the card's
/// job is presence, not alarm.
fn goal_status_tint(phase: LoopPhase, theme: &Theme) -> gpui::Hsla {
    match phase {
        LoopPhase::Blocked => crate::theme::AccentColor::Orange.primary(theme.appearance),
        LoopPhase::AwaitingAnswer => theme.accent,
        LoopPhase::Paused => theme.text_muted,
        _ => theme.text_faint,
    }
}

/// The card's last line: the verifier's last finding — or, while its
/// passes keep failing, a plain "retrying" count instead of the raw
/// provider error or garbled reply (those stay in the log).
fn goal_reason_line(goal: &ChatGoalState) -> Option<String> {
    if goal.eval_failures > 0 {
        return Some(format!(
            "The verifier couldn't reach a verdict ({}/3) — the loop keeps going.",
            goal.eval_failures
        ));
    }
    goal.last_reason.clone()
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
            objective_stated_at: None,
        }
    }

    #[test]
    fn the_card_names_the_goal_from_the_verifier() {
        // Until the first verifier pass names the goal, the objective's
        // first line heads the card.
        let mut unnamed = goal(GoalStatus::Active, 0);
        unnamed.text = "\nship the compaction slice\nand prove it".into();
        assert_eq!(goal_title(&unnamed), "ship the compaction slice");
        let mut named = goal(GoalStatus::Active, 2);
        named.name = Some("Ship the compaction slice".into());
        assert_eq!(goal_title(&named), "Ship the compaction slice");
    }

    #[test]
    fn the_status_label_names_what_the_loop_is_doing() {
        let first = goal(GoalStatus::Active, 0);
        let later = goal(GoalStatus::Active, 7);
        assert_eq!(goal_status_label(&first, LoopPhase::Working), "working");
        assert_eq!(
            goal_status_label(&later, LoopPhase::Working),
            "round 7 · working"
        );
        assert_eq!(goal_status_label(&later, LoopPhase::Checking), "checking");
        assert_eq!(
            goal_status_label(&later, LoopPhase::AwaitingAnswer),
            "waiting for you"
        );
        assert_eq!(goal_status_label(&later, LoopPhase::Paused), "paused");
        assert_eq!(goal_status_label(&later, LoopPhase::Blocked), "blocked");
    }

    #[test]
    fn verifier_failures_read_as_a_retry_count_not_the_raw_error() {
        let mut failing = goal(GoalStatus::Active, 2);
        failing.last_reason = Some("garbled verdict: I think…".into());
        failing.eval_failures = 1;
        let line = goal_reason_line(&failing).unwrap();
        assert!(line.contains("1/3") && !line.contains("garbled"));
        failing.eval_failures = 0;
        assert_eq!(
            goal_reason_line(&failing).as_deref(),
            Some("garbled verdict: I think…")
        );
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
            shell.settings.state_card_expanded = true;
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

    /// The card ↔ capsule switch is view chrome: the card starts folded,
    /// the pill expands on click, the collapse button folds it back, and
    /// the choice is mirrored into the shell's persisted settings. No goal
    /// RPC fires — the loop keeps running either way.
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

        // Folded by default: the capsule, never the card's controls — the
        // loop's state stays out of the user's view until asked for.
        assert!(visual.debug_bounds("goal-card-pill").is_some());
        assert!(visual.debug_bounds("goal-card-pause").is_none());

        // The capsule expands on click, and the choice lands on the shell's
        // settings (the persisted copy).
        let pill = visual.debug_bounds("goal-card-pill").unwrap();
        visual.simulate_click(pill.center(), Default::default());
        visual.run_until_parked();
        assert!(visual.read(|cx| shell.read(cx).settings.state_card_expanded));
        assert!(visual.debug_bounds("goal-card-pause").is_some());
        assert!(visual.debug_bounds("goal-card-pill").is_none());

        // Folding goes back to the capsule.
        let collapse = visual.debug_bounds("goal-card-collapse").unwrap();
        visual.simulate_click(collapse.center(), Default::default());
        visual.run_until_parked();
        assert!(!visual.read(|cx| shell.read(cx).settings.state_card_expanded));
        assert!(visual.debug_bounds("goal-card-pill").is_some());
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
            shell.settings.state_card_expanded = true;
            shell
        });
        let (_view, visual) = cx.add_window_view(|_, _| GoalCardView { shell });

        // Named, the header carries the verifier's name and the objective
        // rides beneath it. Unnamed, the objective's first line heads the
        // card instead — a contentless "Goal" never does — so the line
        // beneath would only repeat it and is dropped.
        assert!(visual.debug_bounds("goal-card-name").is_some());
        assert!(visual.debug_bounds("goal-card-text").is_some());
        visual.update(|_, cx| {
            chats.update(cx, |state, _| {
                state.chats[0].goal.as_mut().unwrap().name = None;
            });
            cx.refresh_windows();
        });
        visual.run_until_parked();
        assert!(visual.debug_bounds("goal-card-name").is_some());
        assert!(visual.debug_bounds("goal-card-text").is_none());

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
            shell.settings.state_card_expanded = true;
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
