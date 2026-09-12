//! The permission-mode control (ADR-0014, prototype 1-B): the composer
//! footer's persistent shield chip + tier menu — one control, identical in
//! empty and populated chats. The menu rows carry fixed per-tier tints
//! (Auto-review blue, Full access orange — semantic signposts, deliberately
//! NOT the selectable accent); the footer chip itself stays in the app's
//! neutral chip idiom.
//!
//! A tier pick on an existing chat goes through `Mutate
//! setChatPermissionMode` (the WatchChats republish recolors the chip; the
//! switch takes effect from the next Turn). On the new-chat canvas the pick
//! stays draft-local ([`DraftConfig::permission_mode`]) and rides the first
//! send — see `composer/send.rs`.

use gpui::{AnyElement, App, Context, SharedString, div, prelude::*, px};

use holt_proto::{Chat, PermissionMode};
use holt_rpc::methods;

use crate::popover;
use crate::theme::Theme;

use super::{PickerKind, Pickers};

/// The tiers in menu order — keyboard nav walks this array.
pub const MODE_TIERS: [PermissionMode; 3] = [
    PermissionMode::ConfirmChanges,
    PermissionMode::AutoReview,
    PermissionMode::FullAccess,
];

/// The tier's display name.
pub fn mode_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::ConfirmChanges => "Confirm changes",
        PermissionMode::AutoReview => "Auto-review",
        PermissionMode::FullAccess => "Full access",
    }
}

/// The tier's one-line description in the menu (prototype 1-B).
pub fn mode_description(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::ConfirmChanges => "Asks before every write, edit, or command",
        PermissionMode::AutoReview => {
            "The model reviews each change first; rejections come with a reason"
        }
        PermissionMode::FullAccess => {
            "Everything runs without asking — only for tasks you fully trust"
        }
    }
}

/// The tier's glyph: shield (gated), eye (model review), open lock (ungated).
pub fn mode_icon(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::ConfirmChanges => crate::icons::SHIELD,
        PermissionMode::AutoReview => crate::icons::EYE,
        PermissionMode::FullAccess => crate::icons::LOCK_OPEN,
    }
}

/// The tier's fixed menu tint: Auto-review reads informational blue, Full
/// access warning orange; Confirm changes — the safe default — stays
/// neutral. Fixed hues, NOT the selectable accent: the tiers are semantic
/// signposts and must read the same under every accent choice.
pub fn mode_tint(mode: PermissionMode, theme: &Theme) -> Option<gpui::Hsla> {
    match mode {
        PermissionMode::ConfirmChanges => None,
        PermissionMode::AutoReview => {
            Some(crate::theme::AccentColor::Blue.primary(theme.appearance))
        }
        PermissionMode::FullAccess => {
            Some(crate::theme::AccentColor::Orange.primary(theme.appearance))
        }
    }
}

/// The tier's index in [`MODE_TIERS`] (menu highlight + keyboard pick).
pub fn mode_index(mode: PermissionMode) -> usize {
    MODE_TIERS.iter().position(|m| *m == mode).unwrap_or(0)
}

/// The mode-precedence rule, shared by the footer chip and the send path:
/// the selected chat's stored config wins; on the new-chat canvas the draft
/// pick applies; untouched means confirm-changes (the proto default — the
/// engine's sticky default only reveals itself once the chat exists, an
/// accepted limitation).
pub fn resolve_permission_mode(
    chat_config: Option<PermissionMode>,
    draft_pick: Option<PermissionMode>,
) -> PermissionMode {
    chat_config.or(draft_pick).unwrap_or_default()
}

/// `Mutate setChatPermissionMode` with the send path's timeout + warn — the
/// ONE JSON shape both the tier menu (existing chat) and the draft-first-send
/// apply. Best-effort: a failure just leaves the chat on its previous mode.
pub(crate) async fn set_chat_permission_mode(
    engine: &crate::state::EngineHandle,
    executor: &gpui::BackgroundExecutor,
    chat_id: &str,
    mode: PermissionMode,
) {
    let result = crate::attachments::call_with_timeout(
        engine,
        executor,
        methods::MUTATE,
        serde_json::json!({
            "op": "setChatPermissionMode",
            "chatId": chat_id,
            "mode": mode,
        }),
        std::time::Duration::from_secs(30),
    )
    .await;
    if let Err(err) = result {
        tracing::warn!(error = %err, "setChatPermissionMode failed");
    }
}

/// The Plan Mode footer label (ADR-0025): `None` when the chat is not
/// planning, else the plain marker — the proposed plan's own card carries
/// the per-proposal state.
pub(crate) fn plan_label(chat: Option<&Chat>) -> Option<String> {
    chat?.plan_mode.as_ref().map(|_| "Plan".to_string())
}

impl Pickers {
    /// The mode the chip advertises: the selected chat's stored mode; on the
    /// new-chat canvas the draft pick, else confirm-changes (the first-launch
    /// default — the engine's sticky default only reveals itself once the
    /// chat exists and its watch row lands).
    pub fn effective_permission_mode(&self, cx: &App) -> PermissionMode {
        resolve_permission_mode(
            self.state
                .read(cx)
                .selected_chat_row()
                .and_then(|chat| chat.config.as_ref())
                .map(|config| config.permission_mode),
            self.config.permission_mode,
        )
    }

    /// A tier pick from the menu. An existing chat switches through the mode
    /// RPC (the chip follows the WatchChats republish); the new-chat canvas
    /// keeps the pick draft-local and the first send applies it. Either way
    /// the pick also seeds the draft memory — new chats inherit the last mode
    /// used, so the canvas chip anticipates the engine's sticky default.
    pub(super) fn pick_permission_mode(&mut self, mode: PermissionMode, cx: &mut Context<Self>) {
        self.config.permission_mode = Some(mode);
        // Close on CLICK, not on the RPC (the spec: "Selecting a tier updates
        // the chat through the mode RPC and closes the menu") — the menu
        // must not hang open for a round-trip; the chip recolors when the
        // WatchChats republish lands.
        self.animate_close(cx);
        cx.notify();
        let chat_id = self.state.read(cx).selected_chat.clone();
        if let Some(chat_id) = chat_id
            && let Some(engine) = self.engine(cx)
        {
            self.mutate_task = Some(cx.spawn(async move |_, cx| {
                set_chat_permission_mode(&engine, cx.background_executor(), &chat_id, mode).await;
            }));
        }
    }

    /// The persistent footer trigger (prototype 1-B): tier icon + current
    /// tier name + a small chevron. Colors are the shared [`Pickers::footer_chip`]
    /// idiom (muted icon/label brightening on hover) — no per-tier tint.
    pub(super) fn mode_chip(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let mode = self.effective_permission_mode(cx);
        self.footer_chip_with_icon_tint(
            PickerKind::Mode,
            "picker-mode",
            mode_icon(mode),
            SharedString::from(mode_label(mode)),
            mode_tint(mode, theme),
            theme,
            cx,
        )
    }

    /// The Plan Mode chip (ADR-0025): shown beside the permission chip while
    /// the selected chat is planning. The label states the chat-level mode;
    /// the × that appears on hover leaves it through the same `ExitPlanMode`
    /// RPC `/plan off` sends — pending approval cards settle as dismissed,
    /// and the transcript's approval card keeps resolving submissions.
    pub(super) fn plan_chip(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let state = self.state.read(cx);
        let label = plan_label(state.selected_chat_row())
            .or_else(|| self.plan_mode_draft.then(|| "Plan".to_string()))?;
        // One group: the × reveals from the label's hover too, and stays put
        // while the pointer is on the button itself.
        let group: SharedString = "picker-plan-chip".into();
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .group(group.clone())
                .child(Self::footer_label(
                    crate::icons::CHECKLIST,
                    SharedString::from(label),
                    theme,
                ))
                .child(
                    div()
                        .id("picker-plan-exit")
                        .debug_selector(|| "picker-plan-exit".into())
                        .flex_none()
                        .size(px(16.0))
                        .rounded(px(4.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        // Hidden at rest: Plan Mode is chat state, not a
                        // control the resting footer advertises. The slot is
                        // reserved either way, so the reveal never shifts the
                        // chips beside it.
                        .opacity(0.0)
                        .group_hover(group, |state| state.opacity(1.0))
                        .hover(|state| state.bg(crate::theme::wash(0.10)))
                        .tooltip(|_, cx| {
                            cx.new(|_| crate::image_viewer::ViewerTooltip("Exit Plan Mode".into()))
                                .into()
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.exit_plan_mode(cx)))
                        .child(
                            crate::icons::icon(crate::icons::CLOSE)
                                .size(px(11.0))
                                .text_color(theme.text_muted),
                        ),
                ),
        )
    }

    /// The Plan chip's close button: leave Plan Mode without the `/plan off`
    /// detour. The outcome is announced through [`PickerEvent`] — the footer
    /// row has nowhere to print it.
    fn exit_plan_mode(&mut self, cx: &mut Context<Self>) {
        self.plan_mode_draft = false;
        cx.notify();
        let chat_id = self.state.read(cx).selected_chat.clone();
        let Some(chat_id) = chat_id else {
            self.plan_mode_draft = false;
            cx.notify();
            return;
        };
        let Some(engine) = self.engine(cx) else {
            cx.emit(super::PickerEvent::PlanModeExitFailed(
                "Engine not connected".into(),
            ));
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::EXIT_PLAN_MODE,
                    serde_json::json!({ "chatId": chat_id }),
                )
                .await;
            let event = match result {
                Ok(_) => super::PickerEvent::PlanModeExited,
                Err(error) => {
                    tracing::warn!(error = %error, "ExitPlanMode failed");
                    super::PickerEvent::PlanModeExitFailed(error.to_string())
                }
            };
            this.update(cx, |_, cx| cx.emit(event)).ok();
        })
        .detach();
    }

    /// The tier menu (prototype 1-B): one row per tier — icon, name, one-line
    /// description, a check on the current tier — then the inheritance /
    /// next-Turn footnote.
    pub(super) fn render_mode_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let current = self.effective_permission_mode(cx);
        let active = self.active;
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(MODE_TIERS.into_iter().enumerate().map(|(ix, mode)| {
                let selected = mode == current;
                let tint = mode_tint(mode, &theme);
                popover::menu_row_nav(&theme, false, ix == active, format!("mode-row-{ix}"))
                    .id(("mode-row", ix))
                    .items_start()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pick_permission_mode(mode, cx);
                    }))
                    .child(
                        crate::icons::icon(mode_icon(mode))
                            .size(px(14.0))
                            .mt(px(2.0))
                            .text_color(tint.unwrap_or(theme.text_muted)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(tint.unwrap_or(theme.text))
                                            .child(SharedString::from(mode_label(mode))),
                                    )
                                    .child(
                                        crate::icons::icon(crate::icons::CHECK)
                                            .size(px(12.0))
                                            .text_color(theme.text)
                                            .opacity(if selected { 1.0 } else { 0.0 }),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(crate::typography::ui_rems(12.0))
                                    .line_height(px(16.0))
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from(mode_description(mode))),
                            ),
                    )
            }))
            .child(popover::menu_separator())
            .child(
                div()
                    .px(px(8.0))
                    .pb(px(4.0))
                    .text_size(crate::typography::ui_rems(11.0))
                    .line_height(px(15.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(
                        "New chats inherit this choice · applies from the next Turn",
                    )),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_label_marks_only_planning_chats() {
        let none = chat("chat-1", PermissionMode::ConfirmChanges);
        assert_eq!(plan_label(Some(&none)), None);
        assert_eq!(plan_label(None), None);

        let mut planning = chat("chat-1", PermissionMode::ConfirmChanges);
        planning.plan_mode = Some(holt_proto::ChatPlanState {
            entry_permission_mode: PermissionMode::ConfirmChanges,
        });
        assert_eq!(plan_label(Some(&planning)).as_deref(), Some("Plan"));
    }

    fn chat(id: &str, mode: PermissionMode) -> holt_proto::Chat {
        holt_proto::Chat {
            id: id.into(),
            device_id: "dev".into(),
            title: None,
            title_source: Default::default(),
            title_task_started: false,
            archived: false,
            cwd: Some("/project".into()),
            branch: None,
            checkout_id: None,
            source_context: None,
            config: Some(holt_proto::ChatConfig {
                provider: holt_proto::ProviderId("anthropic".into()),
                model: "claude-sonnet-4".into(),
                reasoning: None,
                model_options: Default::default(),
                permission_mode: mode,
            }),
            last_message_preview: None,
            last_message_at: None,
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            space_id: Some("space".into()),
            last_seen_at: None,
            room_gen: None,
            compact_before_next_turn: false,
            plan_mode: None,
        }
    }

    #[test]
    fn every_tier_has_a_label_description_icon_and_menu_slot() {
        for (ix, mode) in MODE_TIERS.into_iter().enumerate() {
            assert!(!mode_label(mode).is_empty());
            assert!(!mode_description(mode).is_empty());
            assert!(mode_icon(mode).ends_with(".svg"));
            assert_eq!(mode_index(mode), ix);
        }
        assert_eq!(mode_index(MODE_TIERS[0]), 0);
    }

    #[test]
    fn tier_order_matches_the_menu() {
        assert_eq!(
            MODE_TIERS,
            [
                PermissionMode::ConfirmChanges,
                PermissionMode::AutoReview,
                PermissionMode::FullAccess,
            ]
        );
        assert_eq!(
            mode_label(PermissionMode::ConfirmChanges),
            "Confirm changes"
        );
        assert_eq!(mode_label(PermissionMode::AutoReview), "Auto-review");
        assert_eq!(mode_label(PermissionMode::FullAccess), "Full access");
    }

    #[test]
    fn tier_tints_are_fixed_blue_and_orange_not_the_accent() {
        for theme in [Theme::dark(), Theme::light()] {
            assert_eq!(mode_tint(PermissionMode::ConfirmChanges, &theme), None);
            let blue = mode_tint(PermissionMode::AutoReview, &theme).expect("blue tint");
            let orange = mode_tint(PermissionMode::FullAccess, &theme).expect("orange tint");
            assert_eq!(
                blue,
                crate::theme::AccentColor::Blue.primary(theme.appearance)
            );
            assert_eq!(
                orange,
                crate::theme::AccentColor::Orange.primary(theme.appearance)
            );
            // The signposts must not collapse into each other or follow the
            // (differently-hued) default accent.
            assert_ne!(blue, orange);
        }
    }

    /// The control's states through the real entity (gpui render-entity
    /// test): untouched draft advertises confirm-changes; the menu anchors on
    /// the current tier; a canvas pick stores draft-local and closes; a
    /// selected chat's stored config outranks the draft pick.
    #[gpui::test]
    fn mode_control_states(cx: &mut gpui::TestAppContext) {
        use crate::state::AppState;
        let cx = cx.add_empty_window();
        let state = cx.new(|_| AppState::new());
        let pickers = cx.new(|cx| Pickers::new(state.clone(), cx));

        pickers.update(cx, |this, cx| {
            assert_eq!(
                this.effective_permission_mode(cx),
                PermissionMode::ConfirmChanges
            );
            assert_eq!(this.config.permission_mode, None);
        });

        // Open: anchored on the current tier (ConfirmChanges = row 0).
        cx.update(|window, cx| {
            pickers.update(cx, |this, cx| this.toggle(PickerKind::Mode, window, cx));
        });
        pickers.update(cx, |this, _| {
            assert!(this.is_open());
            assert_eq!(this.active, 0);
        });

        // A canvas pick stays draft-local and closes the menu.
        pickers.update(cx, |this, cx| {
            this.pick_permission_mode(PermissionMode::FullAccess, cx);
            assert_eq!(
                this.config.permission_mode,
                Some(PermissionMode::FullAccess)
            );
            assert!(!this.is_open());
            assert_eq!(
                this.effective_permission_mode(cx),
                PermissionMode::FullAccess
            );
        });

        // Reopening anchors on the picked tier (FullAccess = row 2).
        cx.update(|window, cx| {
            pickers.update(cx, |this, cx| this.toggle(PickerKind::Mode, window, cx));
        });
        pickers.update(cx, |this, cx| {
            assert_eq!(this.active, 2);
            this.animate_close(cx);
        });

        // The selected chat's stored config wins over the draft pick, and
        // the menu anchors on the CHAT's tier (AutoReview = row 1).
        state.update(cx, |s, _| {
            s.chats.push(chat("c1", PermissionMode::AutoReview));
            s.selected_chat = Some("c1".into());
        });
        pickers.update(cx, |this, cx| {
            assert_eq!(
                this.effective_permission_mode(cx),
                PermissionMode::AutoReview
            );
        });
        cx.update(|window, cx| {
            pickers.update(cx, |this, cx| this.toggle(PickerKind::Mode, window, cx));
        });
        pickers.update(cx, |this, _| assert_eq!(this.active, 1));
    }

    /// The drawing half of the mode-control coverage (the spec asks for gpui
    /// RENDER tests, not only entity state): per tier, the footer chip draws
    /// with that tier's icon/label, and the open tier menu draws all
    /// three rows with the keyboard highlight anchored on the current tier.
    #[gpui::test]
    fn mode_control_draws_each_tier(cx: &mut gpui::TestAppContext) {
        use crate::state::AppState;

        // The chip/menu elements must render INSIDE a view: gpui's paint
        // paths read `window.current_view()`, which is empty for an element
        // drawn bare (render.rs:2802 precedent draws a TestView the same
        // way).
        struct ModeControlView {
            pickers: gpui::Entity<Pickers>,
        }
        impl gpui::Render for ModeControlView {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let theme = Theme::of(cx).clone();
                self.pickers.update(cx, |this, cx| {
                    if this.is_open() {
                        this.render_mode_popover(cx)
                    } else {
                        this.mode_chip(&theme, cx).into_any_element()
                    }
                })
            }
        }

        let cx = cx.add_empty_window();
        cx.update(|_, cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| AppState::new());
        let pickers = cx.new(|cx| Pickers::new(state.clone(), cx));
        let view = cx.new(|_| ModeControlView {
            pickers: pickers.clone(),
        });

        for (ix, mode) in MODE_TIERS.into_iter().enumerate() {
            // Draft the tier so chip and menu present its state, then draw
            // the chip (tier icon + label + chevron).
            pickers.update(cx, |this, cx| {
                this.config.permission_mode = Some(mode);
                assert_eq!(this.effective_permission_mode(cx), mode);
            });
            cx.draw(
                gpui::point(gpui::px(0.0), gpui::px(0.0)),
                gpui::size(gpui::px(300.0), gpui::px(40.0)),
                |_, _| view.clone().into_any_element(),
            );
            // Open: the highlight anchors on THIS tier, and the menu (three
            // rows + footnote) draws.
            cx.update(|window, cx| {
                pickers.update(cx, |this, cx| this.toggle(PickerKind::Mode, window, cx));
            });
            pickers.update(cx, |this, _| {
                assert!(this.is_open());
                assert_eq!(this.active, ix);
            });
            cx.draw(
                gpui::point(gpui::px(0.0), gpui::px(0.0)),
                gpui::size(gpui::px(320.0), gpui::px(320.0)),
                |_, _| view.clone().into_any_element(),
            );
            pickers.update(cx, |this, cx| this.animate_close(cx));
        }
    }

    /// The Plan chip's × (the whole point of it): the button renders in the
    /// footer row, and a click sends `ExitPlanMode` for the SELECTED chat and
    /// announces it through `PickerEvent` — the Shell's toast, since the
    /// footer has no notice line of its own.
    #[gpui::test]
    fn the_plan_chip_close_button_exits_plan_mode(cx: &mut gpui::TestAppContext) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        use crate::state::AppState;

        /// Records the ExitPlanMode calls; every other method is unknown, so
        /// the standing AppState watches just retry on their timer.
        struct ExitEngine {
            calls: Mutex<Vec<serde_json::Value>>,
        }

        #[async_trait::async_trait]
        impl holt_rpc::RpcService for ExitEngine {
            async fn handle(
                &self,
                method: &str,
                params: serde_json::Value,
            ) -> Result<holt_rpc::RpcReply, holt_rpc::RpcError> {
                if method == methods::EXIT_PLAN_MODE {
                    self.calls.lock().unwrap().push(params);
                    return holt_rpc::RpcReply::value(&serde_json::json!({ "active": false }));
                }
                Err(holt_rpc::RpcError::UnknownMethod(method.to_string()))
            }
        }

        struct PlanChipView {
            pickers: gpui::Entity<Pickers>,
        }
        impl gpui::Render for PlanChipView {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let theme = Theme::of(cx).clone();
                let chip = self
                    .pickers
                    .update(cx, |this, cx| this.plan_chip(&theme, cx));
                div().children(chip)
            }
        }

        // `memory_client` spawns its dispatch loop with `tokio::spawn`,
        // which needs a runtime context on this thread.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        cx.update(|cx| cx.set_global(Theme::default()));
        let engine = Arc::new(ExitEngine {
            calls: Mutex::new(Vec::new()),
        });
        let state = cx.new(|_| {
            let mut state = AppState::new();
            let mut planning = chat("chat-1", PermissionMode::ConfirmChanges);
            planning.plan_mode = Some(holt_proto::ChatPlanState {
                entry_permission_mode: PermissionMode::ConfirmChanges,
            });
            state.chats.push(planning);
            state.selected_chat = Some("chat-1".into());
            state
        });
        let client = {
            let _guard = runtime.enter();
            holt_rpc::memory_client(engine.clone())
        };
        state.update(cx, |state, cx| state.attach_test_engine(client, cx));
        let pickers = cx.new(|cx| Pickers::new(state.clone(), cx));
        let exited = Arc::new(AtomicUsize::new(0));
        let _events = cx.update(|cx| {
            let exited = exited.clone();
            cx.subscribe(
                &pickers,
                move |_, event: &crate::pickers::PickerEvent, _| {
                    if matches!(event, crate::pickers::PickerEvent::PlanModeExited) {
                        exited.fetch_add(1, Ordering::SeqCst);
                    }
                },
            )
        });
        let (_view, visual) = cx.add_window_view(|_, _| PlanChipView {
            pickers: pickers.clone(),
        });

        let close = visual
            .debug_bounds("picker-plan-exit")
            .expect("the close button renders with the chip");
        // The × is paint-hidden until the chip is hovered; its hitbox is laid
        // out either way, so the click needs no pointer move first.
        visual.simulate_click(close.center(), Default::default());
        // The RPC is async over the in-memory transport: drive the dispatch
        // loop on the test thread, then the gpui foreground executor.
        for _ in 0..8 {
            runtime.block_on(async { tokio::task::yield_now().await });
            visual.run_until_parked();
        }

        assert_eq!(
            engine.calls.lock().unwrap().as_slice(),
            &[serde_json::json!({ "chatId": "chat-1" })]
        );
        assert_eq!(exited.load(Ordering::SeqCst), 1);
    }
}
