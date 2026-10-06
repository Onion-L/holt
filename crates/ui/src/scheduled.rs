//! The Scheduled page (glossary: Routine, ADR-0042): a card grid of the
//! user's Routines with Run now and delete on hover, the Routine drawer with
//! its configuration, runs, and pause/resume, and the create modal. Reads come from the
//! AppState Routines watch; writes are RPCs from here.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use chrono::{DateTime, Local, Utc};

use gpui::{
    AnyElement, Context, Entity, FocusHandle, Focusable, SharedString, Subscription, Task, Window,
    div, prelude::*, px,
};

use holt_proto::{
    PermissionMode, RoutineCheckout, RoutinePause, RoutineRun, RoutineView, RunOutcome,
};
use holt_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::pickers::{MODE_TIERS, Pickers, mode_label};
use crate::popover;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::{Theme, hairline, ink};

const PAGE_MAX_W: f32 = 960.0;
const CARD_MIN_W: f32 = 240.0;
const GRID_GAP: f32 = 12.0;
/// How often the cards' countdowns repaint.
const TICK: Duration = Duration::from_secs(30);
const DRAWER_W: f32 = 420.0;
/// Outcomes the card strip shows, oldest left.
const STRIP_RUNS: usize = 14;

pub enum ScheduledEvent {
    /// A run Chat to show (Run now landed, or a drawer run row).
    OpenChat(String),
    /// The user opened (Some) or closed (None) a Routine's drawer; the shell
    /// keeps it in navigation history so Back from a run Chat restores it.
    Drawer(Option<String>),
}

/// The open create modal.
struct CreateForm {
    name: Entity<ComposerInput>,
    prompt: Entity<ComposerInput>,
    cron: Entity<ComposerInput>,
    space_id: Option<String>,
    mode: PermissionMode,
    checkout: RoutineCheckout,
    error: Option<SharedString>,
    saving: bool,
    focus_pending: bool,
    _events: Vec<Subscription>,
}

pub struct ScheduledPage {
    state: Entity<AppState>,
    /// The composer's pickers: a new Routine takes their resolved model.
    pickers: Entity<Pickers>,
    create: Option<CreateForm>,
    /// The Routine the open delete confirmation targets.
    confirm: Option<String>,
    /// The Routine whose drawer is open.
    drawer: Option<String>,
    error: Option<SharedString>,
    /// Grid columns, measured from the grid's width last paint.
    columns: Rc<Cell<u16>>,
    focus: FocusHandle,
    focus_pending: bool,
    task: Option<Task<()>>,
    _observe: Subscription,
    _tick: Task<()>,
}

impl gpui::EventEmitter<ScheduledEvent> for ScheduledPage {}

impl ScheduledPage {
    pub fn new(state: Entity<AppState>, pickers: Entity<Pickers>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        Self {
            state,
            pickers,
            create: None,
            confirm: None,
            drawer: None,
            error: None,
            columns: Rc::new(Cell::new(3)),
            focus: cx.focus_handle(),
            focus_pending: true,
            task: None,
            _observe: observe,
            _tick: tick,
        }
    }

    /// The shell shows the page again — re-land keyboard focus on it.
    pub fn reveal(&mut self, cx: &mut Context<Self>) {
        self.focus_pending = true;
        cx.notify();
    }

    pub fn drawer(&self) -> Option<String> {
        self.drawer.clone()
    }

    /// Show `routine_id`'s drawer (or none) without recording a navigation:
    /// the shell calls this when walking history.
    pub fn set_drawer(&mut self, routine_id: Option<String>, cx: &mut Context<Self>) {
        self.drawer = routine_id;
        cx.notify();
    }

    pub(crate) fn open_drawer(&mut self, routine_id: String, cx: &mut Context<Self>) {
        if self.drawer.as_ref() == Some(&routine_id) {
            return;
        }
        self.drawer = Some(routine_id.clone());
        cx.emit(ScheduledEvent::Drawer(Some(routine_id)));
        cx.notify();
    }

    fn close_drawer(&mut self, cx: &mut Context<Self>) {
        if self.drawer.take().is_some() {
            cx.emit(ScheduledEvent::Drawer(None));
            cx.notify();
        }
    }

    pub(crate) fn open_run(&mut self, chat_id: String, cx: &mut Context<Self>) {
        cx.emit(ScheduledEvent::OpenChat(chat_id));
    }

    fn open_create(&mut self, cx: &mut Context<Self>) {
        let name = cx.new(|cx| ComposerInput::new("Morning triage", cx));
        let prompt = cx.new(|cx| ComposerInput::new("What should the agent do each run?", cx));
        let cron = cx.new(|cx| ComposerInput::new("0 9 * * 1-5", cx));
        prompt.update(cx, |input, _| input.set_max_display_height(160.0));
        let events = [&name, &prompt, &cron]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this: &mut Self, _, event, cx| match event {
                    ComposerInputEvent::Submitted => this.submit_create(cx),
                    ComposerInputEvent::Edited => {
                        if let Some(form) = this.create.as_mut() {
                            form.error = None;
                        }
                        cx.notify();
                    }
                    _ => {}
                })
            })
            .collect();
        let space_id = {
            let state = self.state.read(cx);
            state
                .selected_space
                .clone()
                .filter(|id| state.space_row(id).is_some())
                .or_else(|| state.spaces_sorted().first().map(|space| space.id.clone()))
        };
        self.create = Some(CreateForm {
            name,
            prompt,
            cron,
            space_id,
            mode: PermissionMode::AutoReview,
            checkout: RoutineCheckout::MainCheckout,
            error: None,
            saving: false,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    fn close_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.create = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn submit_create(&mut self, cx: &mut Context<Self>) {
        let config = self.pickers.read(cx).resolved(cx).chat_config();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(form) = self.create.as_mut() else {
            return;
        };
        if form.saving {
            return;
        }
        let name = form.name.read(cx).text().trim().to_string();
        let prompt = form.prompt.read(cx).text().trim().to_string();
        let cron = form.cron.read(cx).text().trim().to_string();
        let problem = if name.is_empty() {
            Some("Give the routine a name.")
        } else if prompt.is_empty() {
            Some("Write the prompt each run starts with.")
        } else if cron.is_empty() {
            Some("Set a cron schedule.")
        } else if form.space_id.is_none() {
            Some("Pick a project.")
        } else if config.is_none() {
            Some("Pick a model in the composer first.")
        } else {
            None
        };
        if let Some(problem) = problem {
            form.error = Some(problem.into());
            cx.notify();
            return;
        }
        let Some(mut config) = config else {
            return;
        };
        config.permission_mode = form.mode;
        let params = serde_json::json!({
            "name": name,
            "spaceId": form.space_id,
            "prompt": prompt,
            "cron": cron,
            "config": config,
            "checkout": form.checkout,
        });
        form.saving = true;
        form.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::CREATE_ROUTINE, params).await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.create = None;
                        page.focus_pending = true;
                    }
                    Err(err) => {
                        if let Some(form) = page.create.as_mut() {
                            form.saving = false;
                            form.error = Some(err.to_string().into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn run_now(&mut self, routine_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::RUN_ROUTINE_NOW,
                    serde_json::json!({ "routineId": routine_id }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(reply) => {
                        if let Some(chat_id) = reply.get("chatId").and_then(|v| v.as_str()) {
                            cx.emit(ScheduledEvent::OpenChat(chat_id.to_string()));
                        }
                    }
                    Err(err) => page.error = Some(format!("Run now failed: {err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn set_paused(&mut self, routine_id: String, paused: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SET_ROUTINE_PAUSED,
                    serde_json::json!({ "routineId": routine_id, "paused": paused }),
                )
                .await;
            if let Err(err) = result {
                let verb = if paused { "Pause" } else { "Resume" };
                this.update(cx, |page, cx| {
                    page.error = Some(format!("{verb} failed: {err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    fn ask_delete(&mut self, routine_id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm = Some(routine_id);
        // Enter / Esc answer the dialog through the root's key handler.
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        let Some(routine_id) = self.confirm.take() else {
            return;
        };
        if self.drawer.as_ref() == Some(&routine_id) {
            self.close_drawer(cx);
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::DELETE_ROUTINE,
                    serde_json::json!({ "routineId": routine_id }),
                )
                .await;
            if let Err(err) = result {
                this.update(cx, |page, cx| {
                    page.error = Some(format!("Delete failed: {err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
        cx.notify();
    }

    fn on_key(&mut self, event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if self.confirm.is_some() {
            match key {
                "escape" => self.confirm = None,
                "enter" => self.delete(cx),
                _ => return,
            }
        } else if self.create.is_some() && key == "escape" {
            self.close_create(window, cx);
        } else if self.drawer.is_some() && key == "escape" {
            self.close_drawer(cx);
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    // ---- render pieces ----

    fn model_label(&self, routine: &RoutineView, cx: &Context<Self>) -> SharedString {
        let config = &routine.routine.config;
        self.pickers
            .read(cx)
            .model_label(&config.provider, &config.model)
            .map(str::to_string)
            .unwrap_or_else(|| short_model(&config.model).to_string())
            .into()
    }

    fn render_card(&self, theme: &Theme, view: &RoutineView, cx: &mut Context<Self>) -> AnyElement {
        let routine = &view.routine;
        let group: SharedString = format!("routine-card-{}", routine.id).into();
        let space = self
            .state
            .read(cx)
            .space_row(&routine.space_id)
            .map(|space| space.display_name().to_string())
            .unwrap_or_else(|| "Missing project".into());
        let run_id = routine.id.clone();
        let delete_id = routine.id.clone();
        let actions = div()
            .flex_none()
            .flex()
            .flex_row()
            .gap(px(2.0))
            .invisible()
            .group_hover(group.clone(), |s| s.visible())
            .child(
                icon_button(
                    theme,
                    format!("routine-run-{}", routine.id).into(),
                    icons::PLAY,
                    false,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.run_now(run_id.clone(), cx);
                })),
            )
            .child(
                icon_button(
                    theme,
                    format!("routine-delete-{}", routine.id).into(),
                    icons::TRASH_BIN_MINIMALISTIC,
                    true,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.ask_delete(delete_id.clone(), window, cx);
                })),
            );
        let faint = theme.text_muted.opacity(0.75);
        let open_id = routine.id.clone();
        let selected = self.drawer.as_ref() == Some(&routine.id);
        div()
            .id(group.clone())
            .group(group)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.open_drawer(open_id.clone(), cx)))
            .min_w_0()
            .min_h(px(176.0))
            .p(px(14.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(hairline(0.08))
            .bg(ink(0.025))
            .hover(|s| s.bg(ink(0.045)))
            .when(selected, |card| {
                card.border_color(hairline(0.2)).bg(ink(0.045))
            })
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .h(px(26.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(13.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(routine.name.clone())),
                    )
                    .child(actions),
            )
            .child(
                div()
                    .truncate()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(faint)
                    .child(SharedString::from(format!(
                        "{} \u{b7} {space}",
                        routine.cron
                    ))),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .child(match routine.paused {
                        Some(pause) => paused_pill(theme, pause).into_any_element(),
                        None => div()
                            .text_size(crate::typography::ui_rems(15.0))
                            .text_color(theme.text)
                            .children(view.next_fire_at.map(|next| countdown(next, Utc::now())))
                            .into_any_element(),
                    })
                    .child(run_strip(theme, &routine.runs)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(faint)
                    .child(div().min_w_0().truncate().child(self.model_label(view, cx)))
                    .child(
                        div()
                            .flex_none()
                            .child(mode_label(routine.config.permission_mode)),
                    ),
            )
            .into_any_element()
    }

    fn render_drawer(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let routine_id = self.drawer.as_ref()?;
        let (view, space, chats) = {
            let state = self.state.read(cx);
            let view = state
                .routines
                .iter()
                .find(|view| &view.routine.id == routine_id)?
                .clone();
            let space = state
                .space_row(&view.routine.space_id)
                .map(|space| space.display_name().to_string())
                .unwrap_or_else(|| "Missing project".into());
            let chats: std::collections::HashSet<String> =
                state.chats.iter().map(|chat| chat.id.clone()).collect();
            (view, space, chats)
        };
        let routine = &view.routine;
        let model = self.model_label(&view, cx);
        let next = view
            .next_fire_at
            .map(|next| format!("{} ({})", local_time(next), countdown(next, Utc::now())))
            .unwrap_or_else(|| "\u{2014}".into());
        let checkout = match routine.checkout {
            RoutineCheckout::MainCheckout => "Main checkout",
            RoutineCheckout::NewWorktree => "New worktree",
        };
        let config: [(&'static str, SharedString); 7] = [
            ("Schedule", routine.cron.clone().into()),
            ("Time zone", routine.time_zone.clone().into()),
            ("Next run", next.into()),
            ("Project", space.into()),
            ("Checkout", checkout.into()),
            ("Model", model),
            (
                "Permission",
                mode_label(routine.config.permission_mode).into(),
            ),
        ];
        let label = |text: &'static str| {
            div()
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted)
                .child(text)
        };
        let grid = div()
            .grid()
            .grid_cols(2)
            .gap_x(px(12.0))
            .gap_y(px(8.0))
            .children(config.into_iter().map(|(key, value)| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .min_w_0()
                    .child(label(key))
                    .child(
                        div()
                            .truncate()
                            .text_size(crate::typography::ui_rems(13.0))
                            .text_color(theme.text)
                            .child(value),
                    )
            }));
        let run_id = routine.id.clone();
        let pause_id = routine.id.clone();
        let paused = routine.paused.is_some();
        let delete_id = routine.id.clone();
        let banner = routine.paused.map(|pause| {
            div()
                .px(px(12.0))
                .py(px(8.0))
                .rounded(px(8.0))
                .bg(ink(0.05))
                .border_1()
                .border_color(hairline(0.08))
                .text_size(crate::typography::ui_rems(12.5))
                .text_color(theme.text_muted)
                .child(pause_banner(pause))
        });
        let rows: Vec<AnyElement> = routine
            .runs
            .iter()
            .enumerate()
            .map(|(ix, run)| self.render_run_row(theme, ix, run, &chats, cx))
            .collect();
        let empty = rows.is_empty();
        let panel = div()
            .id("routine-drawer")
            .debug_selector(|| "routine-drawer".into())
            .occlude()
            .absolute()
            .top_0()
            .right_0()
            .h_full()
            .w(px(DRAWER_W))
            .bg(theme.surface_dialog)
            .border_l_1()
            .border_color(hairline(0.10))
            .shadow_lg()
            .flex()
            .flex_col()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                // Clicks inside a dialog this drawer opened stay put.
                if this.confirm.is_none() && this.create.is_none() {
                    this.close_drawer(cx);
                }
            }))
            .child(
                div()
                    .flex_none()
                    .px(px(20.0))
                    .pt(px(18.0))
                    .pb(px(14.0))
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(popover::dialog_title(theme, &routine.name)),
                            )
                            .child(
                                icon_button(
                                    theme,
                                    "routine-drawer-close".into(),
                                    icons::CLOSE,
                                    false,
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.close_drawer(cx))),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .child(
                                popover::btn_primary(theme, "Run now")
                                    .id("routine-drawer-run")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.run_now(run_id.clone(), cx)
                                    })),
                            )
                            .child(
                                popover::btn_ghost(
                                    theme,
                                    if paused { "Resume" } else { "Pause" },
                                    "routine-drawer-pause",
                                )
                                .id("routine-drawer-pause")
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.set_paused(pause_id.clone(), !paused, cx)
                                    },
                                )),
                            )
                            .child(
                                popover::btn_ghost(theme, "Delete", "routine-drawer-delete")
                                    .id("routine-drawer-delete")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.ask_delete(delete_id.clone(), window, cx)
                                    })),
                            ),
                    )
                    .children(banner)
                    .child(grid)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .child(label("Prompt"))
                            .child(
                                div()
                                    .id("routine-drawer-prompt")
                                    .max_h(px(140.0))
                                    .overflow_y_scroll()
                                    .occlude()
                                    .px(px(12.0))
                                    .py(px(10.0))
                                    .rounded(px(8.0))
                                    .bg(ink(0.03))
                                    .border_1()
                                    .border_color(hairline(0.06))
                                    .text_size(crate::typography::ui_rems(13.0))
                                    .text_color(theme.text)
                                    .child(SharedString::from(routine.prompt.clone())),
                            ),
                    )
                    .child(label("Runs")),
            )
            .child(
                // A nested scroller inside the page: occlude so one wheel
                // gesture can't move both (ADR-0013).
                div()
                    .id("routine-drawer-runs")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .occlude()
                    .px(px(12.0))
                    .pb(px(16.0))
                    .when(empty, |list| {
                        list.child(
                            div()
                                .px(px(8.0))
                                .text_size(crate::typography::ui_rems(12.5))
                                .text_color(theme.text_muted)
                                .child("No runs yet."),
                        )
                    })
                    .children(rows),
            );
        Some(panel.into_any_element())
    }

    fn render_run_row(
        &self,
        theme: &Theme,
        ix: usize,
        run: &RoutineRun,
        chats: &std::collections::HashSet<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chat_id = run.chat_id.clone().filter(|id| chats.contains(id));
        let mut marks: Vec<String> = Vec::new();
        if run.manual {
            marks.push("Manual".into());
        }
        if run.missed_fires > 0 {
            marks.push(format!("补跑 \u{b7} {} missed", run.missed_fires));
        }
        let detail = run.note.clone().or_else(|| {
            (run.chat_id.is_some() && chat_id.is_none()).then(|| "chat deleted".to_string())
        });
        div()
            .id(("routine-run-row", ix))
            .px(px(8.0))
            .py(px(7.0))
            .rounded(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .text_size(crate::typography::ui_rems(12.5))
            .child(outcome_mark(
                theme,
                run.outcome,
                div().flex_none().size(px(7.0)).rounded(px(999.0)),
            ))
            .child(
                div()
                    .flex_none()
                    .w(px(84.0))
                    .text_color(theme.text)
                    .child(outcome_label(run.outcome)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.text_muted)
                    .child(SharedString::from(
                        std::iter::once(local_time(run.fired_at))
                            .chain(marks)
                            .chain(detail)
                            .collect::<Vec<_>>()
                            .join(" \u{b7} "),
                    )),
            )
            .map(|row| match chat_id {
                Some(chat_id) => row.cursor_pointer().hover(|s| s.bg(ink(0.05))).on_click(
                    cx.listener(move |this, _, _, cx| this.open_run(chat_id.clone(), cx)),
                ),
                None => row.opacity(0.7),
            })
            .into_any_element()
    }

    fn render_add_card(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("routine-add-card")
            .min_h(px(176.0))
            .rounded(px(12.0))
            .border_1()
            .border_dashed()
            .border_color(hairline(0.12))
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap(px(6.0))
            .text_size(crate::typography::ui_rems(13.0))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|s| s.bg(ink(0.03)).text_color(theme.text))
            .on_click(cx.listener(|this, _, _, cx| this.open_create(cx)))
            .child(
                icon(icons::PLUS)
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            )
            .child("New routine")
            .into_any_element()
    }

    fn render_header(&self, theme: &Theme, count: usize, cx: &mut Context<Self>) -> gpui::Div {
        let summary = match count {
            0 => "Run a prompt on a schedule, in a fresh chat each time.".to_string(),
            1 => "1 routine".to_string(),
            n => format!("{n} routines"),
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(16.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(widgets::page_header(theme, "Scheduled", None))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(12.5))
                            .text_color(theme.text_muted)
                            .child(summary),
                    ),
            )
            .child(
                popover::btn_primary(theme, "New routine")
                    .id("routine-new")
                    .on_click(cx.listener(|this, _, _, cx| this.open_create(cx))),
            )
    }

    fn render_create(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let form = self.create.as_mut()?;
        if std::mem::take(&mut form.focus_pending) {
            window.focus(&form.name.focus_handle(cx), cx);
        }
        let (name, prompt, cron) = (form.name.clone(), form.prompt.clone(), form.cron.clone());
        let (space_id, mode, checkout) = (form.space_id.clone(), form.mode, form.checkout);
        let (error, saving) = (form.error.clone(), form.saving);
        let spaces: Vec<(String, SharedString)> = self
            .state
            .read(cx)
            .spaces_sorted()
            .into_iter()
            .map(|space| (space.id.clone(), space.display_name().to_string().into()))
            .collect();
        let model: SharedString = {
            let pickers = self.pickers.read(cx);
            let resolved = pickers.resolved(cx);
            match (&resolved.provider, &resolved.model) {
                (Some(provider), Some(model)) => pickers
                    .model_label(provider, model)
                    .map(str::to_string)
                    .unwrap_or_else(|| short_model(model).to_string())
                    .into(),
                _ => "No model picked".into(),
            }
        };

        let space_chips = div().flex().flex_row().flex_wrap().gap(px(6.0)).children(
            spaces.into_iter().enumerate().map(|(ix, (id, label))| {
                let active = space_id.as_deref() == Some(id.as_str());
                chip(theme, active)
                    .id(("routine-space", ix))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(form) = this.create.as_mut() {
                            form.space_id = Some(id.clone());
                            form.error = None;
                        }
                        cx.notify();
                    }))
                    .child(label)
            }),
        );
        let checkout_seg = segmented(
            theme,
            [
                (
                    "Main checkout",
                    checkout == RoutineCheckout::MainCheckout,
                    RoutineCheckout::MainCheckout,
                ),
                (
                    "New worktree",
                    checkout == RoutineCheckout::NewWorktree,
                    RoutineCheckout::NewWorktree,
                ),
            ]
            .into_iter()
            .enumerate()
            .map(|(ix, (label, active, value))| {
                seg_item(theme, active)
                    .id(("routine-checkout", ix))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(form) = this.create.as_mut() {
                            form.checkout = value;
                        }
                        cx.notify();
                    }))
                    .child(label)
            }),
        );
        let mode_seg = segmented(
            theme,
            MODE_TIERS.into_iter().enumerate().map(|(ix, tier)| {
                seg_item(theme, tier == mode)
                    .id(("routine-mode", ix))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(form) = this.create.as_mut() {
                            form.mode = tier;
                        }
                        cx.notify();
                    }))
                    .child(mode_label(tier))
            }),
        );

        let card = popover::dialog_card(theme)
            .w(px(620.0))
            .px(px(22.0))
            .py(px(18.0))
            .rounded(px(14.0))
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                if ev.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.close_create(window, cx);
                }
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(popover::dialog_title(theme, "New routine"))
                    .child(
                        icon_button(theme, "routine-create-close".into(), icons::CLOSE, false)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_create(window, cx)),
                            ),
                    ),
            )
            .child(field(
                theme,
                "Name",
                popover::dialog_field(name.into_any_element()),
            ))
            .child(field(
                theme,
                "Prompt",
                popover::dialog_field(prompt.into_any_element()).min_h(px(72.0)),
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(14.0))
                    .child(div().flex_1().min_w_0().child(field(
                        theme,
                        "Schedule (cron)",
                        popover::dialog_field(cron.into_any_element()),
                    )))
                    .child(
                        div().flex_1().min_w_0().child(field(
                            theme,
                            "Model",
                            div()
                                .px(px(12.0))
                                .py(px(8.0))
                                .rounded(px(8.0))
                                .border_1()
                                .border_color(hairline(0.08))
                                .truncate()
                                .text_size(crate::typography::ui_rems(14.0))
                                .text_color(theme.text_muted)
                                .child(model),
                        )),
                    ),
            )
            .child(field(theme, "Project", space_chips))
            .child(field(theme, "Checkout", checkout_seg))
            .child(field(theme, "Permission mode", mode_seg))
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .mt(px(14.0))
                        .child(widgets::error_strip(theme, message)),
                )
            })
            .child(
                div()
                    .mt(px(18.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(theme, "Cancel", "routine-create-cancel")
                            .id("routine-create-cancel")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_create(window, cx)),
                            ),
                    )
                    .child(
                        popover::btn_primary(theme, if saving { "Creating…" } else { "Create" })
                            .id("routine-create-save")
                            .when(saving, |el| el.opacity(0.6))
                            .on_click(cx.listener(|this, _, _, cx| this.submit_create(cx))),
                    ),
            )
            .into_any_element();
        Some(popover::modal(
            "routine-create-dialog",
            window.viewport_size(),
            card,
        ))
    }

    fn render_confirm(
        &self,
        theme: &Theme,
        routine_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (name, runs) = {
            let state = self.state.read(cx);
            let routine = state
                .routines
                .iter()
                .find(|view| view.routine.id == routine_id)?;
            let runs = state
                .chats
                .iter()
                .filter(|chat| {
                    chat.routine_run
                        .as_ref()
                        .is_some_and(|marker| marker.routine_id == routine_id)
                })
                .count();
            (routine.routine.name.clone(), runs)
        };
        let card = popover::dialog_card(theme)
            .w(px(420.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.confirm = None;
                cx.notify();
            }))
            .child(div().truncate().child(popover::dialog_title(
                theme,
                &format!("Delete \u{201c}{name}\u{201d}?"),
            )))
            .child(
                div()
                    .mt(px(8.0))
                    .child(popover::dialog_body(theme, delete_body(runs))),
            )
            .child(
                div()
                    .mt(px(18.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(theme, "Cancel", "routine-delete-cancel")
                            .id("routine-delete-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.confirm = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        popover::btn_danger(theme, "Delete")
                            .id("routine-delete-confirm")
                            .on_click(cx.listener(|this, _, _, cx| this.delete(cx))),
                    ),
            )
            .into_any_element();
        Some(popover::modal(
            "routine-delete-dialog",
            window.viewport_size(),
            card,
        ))
    }
}

impl Render for ScheduledPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        if std::mem::take(&mut self.focus_pending) {
            window.focus(&self.focus, cx);
        }
        let routines = self.state.read(cx).routines.clone();
        // A Routine deleted elsewhere takes its confirmation with it.
        if self
            .confirm
            .as_ref()
            .is_some_and(|id| !routines.iter().any(|view| &view.routine.id == id))
        {
            self.confirm = None;
        }

        let mut cards: Vec<AnyElement> = routines
            .iter()
            .map(|view| self.render_card(&theme, view, cx))
            .collect();
        cards.push(self.render_add_card(&theme, cx));
        let columns = self.columns.clone();
        let grid = div()
            .relative()
            .grid()
            .grid_cols(self.columns.get())
            .gap(px(GRID_GAP))
            .child(
                gpui::canvas(
                    move |bounds, window, _| {
                        let fit = grid_columns(f32::from(bounds.size.width));
                        if fit != columns.get() {
                            columns.set(fit);
                            window.refresh();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .children(cards);

        let confirm = self
            .confirm
            .clone()
            .and_then(|id| self.render_confirm(&theme, &id, window, cx));
        let create = self.render_create(&theme, window, cx);
        // A Routine deleted elsewhere takes its drawer with it.
        if self
            .drawer
            .as_ref()
            .is_some_and(|id| !routines.iter().any(|view| &view.routine.id == id))
        {
            self.drawer = None;
        }
        let drawer = self.render_drawer(&theme, cx);

        div()
            .id("scheduled-page")
            .debug_selector(|| "scheduled-page".into())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                this.on_key(event, window, cx);
            }))
            .relative()
            .size_full()
            .child(
                div()
                    .id("scheduled-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .w_full()
                            .max_w(px(PAGE_MAX_W))
                            .mx_auto()
                            .px(px(24.0))
                            .pt(px(28.0))
                            .pb(px(32.0))
                            .flex()
                            .flex_col()
                            .gap(px(20.0))
                            .child(self.render_header(&theme, routines.len(), cx))
                            .when_some(self.error.clone(), |el, message| {
                                el.child(
                                    widgets::error_strip(&theme, message)
                                        .id("routine-error")
                                        .cursor_pointer()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.error = None;
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(grid),
                    ),
            )
            .children(drawer)
            .children(confirm)
            .children(create)
    }
}

/// How many card columns fit a grid this wide (never fewer than one).
fn grid_columns(width: f32) -> u16 {
    (((width + GRID_GAP) / (CARD_MIN_W + GRID_GAP)).floor() as u16).max(1)
}

/// Time until the next fire at minute grain: "in 2h 14m", "in 3d 4h".
fn countdown(next: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let minutes = (next - now).num_minutes();
    if next <= now {
        return "due now".into();
    }
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => "in <1m".into(),
        (0, 0, m) => format!("in {m}m"),
        (0, h, m) => format!("in {h}h {m}m"),
        (d, h, _) => format!("in {d}d {h}h"),
    }
}

/// The delete confirmation's consequence line; run Chats outlive the Routine.
fn delete_body(runs: usize) -> String {
    match runs {
        0 => "It stops firing.".into(),
        1 => "It stops firing. Its run chat stays in Recent as an ordinary chat.".into(),
        n => format!("It stops firing. Its {n} run chats stay in Recent as ordinary chats."),
    }
}

/// A run time in the viewer's zone: "Oct 7, 09:00".
fn local_time(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%b %-d, %H:%M").to_string()
}

fn outcome_label(outcome: RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Running => "Running",
        RunOutcome::Waiting => "Waiting",
        RunOutcome::Succeeded => "Succeeded",
        RunOutcome::Failed => "Failed",
        RunOutcome::Interrupted => "Interrupted",
        RunOutcome::Skipped => "Skipped",
    }
}

fn outcome_color(theme: &Theme, outcome: RunOutcome) -> gpui::Hsla {
    match outcome {
        RunOutcome::Running => theme.busy,
        RunOutcome::Waiting => theme.warning,
        RunOutcome::Succeeded => theme.success,
        RunOutcome::Failed => theme.danger,
        RunOutcome::Interrupted => theme.text_faint,
        RunOutcome::Skipped => ink(0.3),
    }
}

/// Fill `mark` with the outcome's colour; a skipped fire is only outlined.
fn outcome_mark(theme: &Theme, outcome: RunOutcome, mark: gpui::Div) -> gpui::Div {
    let color = outcome_color(theme, outcome);
    if outcome == RunOutcome::Skipped {
        mark.border_1().border_color(color)
    } else {
        mark.bg(color)
    }
}

fn paused_pill(theme: &Theme, pause: RoutinePause) -> gpui::Div {
    div()
        .px(px(8.0))
        .py(px(2.0))
        .rounded_full()
        .bg(ink(0.06))
        .text_size(crate::typography::ui_rems(11.5))
        .text_color(theme.text_muted)
        .child(match pause {
            RoutinePause::User => "Paused",
            RoutinePause::SpaceRemoved => "Paused \u{b7} Project removed",
        })
}

fn pause_banner(pause: RoutinePause) -> &'static str {
    match pause {
        RoutinePause::User => "Paused. It won't fire until you resume it.",
        RoutinePause::SpaceRemoved => "Paused because its project was removed.",
    }
}

/// The newest runs as the strip shows them, oldest left (`runs` is newest
/// first): each outcome, and whether it was a Catch-up run.
fn strip_outcomes(runs: &[RoutineRun]) -> Vec<(RunOutcome, bool)> {
    let mut outcomes: Vec<(RunOutcome, bool)> = runs
        .iter()
        .take(STRIP_RUNS)
        .map(|run| (run.outcome, run.missed_fires > 0))
        .collect();
    outcomes.reverse();
    outcomes
}

fn run_strip(theme: &Theme, runs: &[RoutineRun]) -> gpui::Div {
    div()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
        .children(strip_outcomes(runs).into_iter().map(|(outcome, catch_up)| {
            let mark = div().w(px(4.0)).h(px(12.0)).rounded(px(1.5));
            // A Catch-up run is ringed; a skipped one stays hollow.
            let mark = if catch_up && outcome != RunOutcome::Skipped {
                mark.border_1().border_color(theme.text)
            } else {
                mark
            };
            outcome_mark(theme, outcome, mark)
        }))
}

/// `openai/gpt-5.4` → `gpt-5.4` when the catalog hasn't named the model.
fn short_model(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// A labeled form row in the create modal.
fn field(theme: &Theme, label: &'static str, control: impl IntoElement) -> gpui::Div {
    div()
        .mt(px(14.0))
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted)
                .child(label),
        )
        .child(control)
}

fn segmented(theme: &Theme, items: impl IntoIterator<Item = impl IntoElement>) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .self_start()
        .p(px(2.0))
        .gap(px(2.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(hairline(0.08))
        .bg(ink(0.03))
        .text_color(theme.text_muted)
        .children(items)
}

fn seg_item(theme: &Theme, active: bool) -> gpui::Div {
    div()
        .px(px(10.0))
        .py(px(4.0))
        .rounded(px(6.0))
        .text_size(crate::typography::ui_rems(12.5))
        .cursor_pointer()
        .when(active, |el| el.bg(ink(0.09)).text_color(theme.text))
        .when(!active, |el| el.hover(|s| s.text_color(theme.text)))
}

fn chip(theme: &Theme, active: bool) -> gpui::Div {
    div()
        .px(px(10.0))
        .py(px(4.0))
        .rounded(px(999.0))
        .border_1()
        .text_size(crate::typography::ui_rems(12.5))
        .cursor_pointer()
        .when(active, |el| {
            el.border_color(hairline(0.18))
                .bg(ink(0.08))
                .text_color(theme.text)
        })
        .when(!active, |el| {
            el.border_color(hairline(0.08))
                .text_color(theme.text_muted)
                .hover(|s| s.text_color(theme.text))
        })
}

/// A 26px square icon button; the name doubles as id and hover group so
/// the glyph brightens through `group_hover`.
fn icon_button(
    theme: &Theme,
    name: SharedString,
    path: &'static str,
    danger: bool,
) -> gpui::Stateful<gpui::Div> {
    let (wash, hover_fg) = if danger {
        (theme.danger.opacity(0.12), theme.danger)
    } else {
        (ink(0.08), theme.text)
    };
    div()
        .id(name.clone())
        .group(name.clone())
        .flex_none()
        .size(px(26.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(move |s| s.bg(wash))
        .child(
            icon(path)
                .size(px(14.0))
                .text_color(theme.text_muted)
                .group_hover(name, move |s| s.text_color(hover_fg)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_fits_columns_to_width() {
        assert_eq!(grid_columns(0.0), 1);
        assert_eq!(grid_columns(239.0), 1);
        assert_eq!(grid_columns(240.0), 1);
        assert_eq!(grid_columns(492.0), 2);
        assert_eq!(grid_columns(912.0), 3);
    }

    #[test]
    fn countdown_reads_at_minute_grain() {
        let now = DateTime::parse_from_rfc3339("2026-10-07T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let at = |minutes: i64, seconds: i64| {
            now + chrono::Duration::minutes(minutes) + chrono::Duration::seconds(seconds)
        };
        assert_eq!(countdown(at(0, -5), now), "due now");
        assert_eq!(countdown(at(0, 40), now), "in <1m");
        assert_eq!(countdown(at(14, 30), now), "in 14m");
        assert_eq!(countdown(at(134, 0), now), "in 2h 14m");
        assert_eq!(countdown(at(3 * 1440 + 250, 0), now), "in 3d 4h");
    }

    #[test]
    fn delete_body_counts_run_chats() {
        assert_eq!(delete_body(0), "It stops firing.");
        assert!(delete_body(1).contains("Its run chat stays"));
        assert!(delete_body(4).contains("Its 4 run chats stay"));
    }

    #[test]
    fn strip_shows_the_newest_fourteen_oldest_first_and_marks_catch_up() {
        let at = DateTime::parse_from_rfc3339("2026-10-07T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // Newest first: one failure on top of twenty successes.
        let mut runs: Vec<RoutineRun> = std::iter::once(RunOutcome::Failed)
            .chain(std::iter::repeat_n(RunOutcome::Succeeded, 20))
            .map(|outcome| RoutineRun {
                fired_at: at,
                outcome,
                note: None,
                chat_id: None,
                missed_fires: 0,
                manual: false,
            })
            .collect();
        let strip = strip_outcomes(&runs);
        assert_eq!(strip.len(), STRIP_RUNS);
        assert_eq!(strip.last(), Some(&(RunOutcome::Failed, false)));
        assert_eq!(strip_outcomes(&runs[..3]).len(), 3);
        runs[0].missed_fires = 3;
        assert_eq!(
            strip_outcomes(&runs).last(),
            Some(&(RunOutcome::Failed, true)),
            "a Catch-up run is marked"
        );
    }

    #[test]
    fn short_model_drops_the_vendor_prefix() {
        assert_eq!(short_model("openai/gpt-5.4"), "gpt-5.4");
        assert_eq!(short_model("kimi-k3"), "kimi-k3");
    }
}
