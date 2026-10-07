//! The Scheduled page (glossary: Routine, ADR-0042): a card grid of the
//! user's Routines with Run now and delete on hover, the Routine drawer with
//! its configuration, runs, and pause/resume, and the create/edit modal. Reads come from the
//! AppState Routines watch; writes are RPCs from here.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, Utc};

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, SharedString, Subscription, Task,
    Window, div, prelude::*, px,
};

use holt_proto::{
    ChatConfig, Model, PermissionMode, ProviderId, Routine, RoutineCheckout, RoutinePause,
    RoutineRun, RoutineView, RunOutcome,
};
use holt_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::pickers::{MODE_TIERS, Pickers, default_reasoning, mode_label};
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

/// The time picker's column height and row height.
const TIME_MENU_H: f32 = 232.0;
const TIME_ROW_H: f32 = 28.0;

/// How long schedule edits settle before the preview is re-read.
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(250);
/// Rows the form's model list shows at most; search narrows the rest.
const MODEL_ROWS: usize = 40;

/// The schedule shapes the form spells for the user. `Custom` is a single
/// run at a picked date and time. `Cron` is only a Routine whose stored cron
/// reads as no preset; the form keeps it as is but never offers it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Preset {
    Hourly,
    Daily,
    Weekdays,
    Weekly,
    Custom,
    Cron,
}

const PRESETS: [(Preset, &str); 5] = [
    (Preset::Hourly, "Hourly"),
    (Preset::Daily, "Daily"),
    (Preset::Weekdays, "Weekdays"),
    (Preset::Weekly, "Weekly"),
    (Preset::Custom, "Custom"),
];

/// Cron weekday numbers (0 = Sunday) in display order.
const WEEKDAYS: [(u8, &str); 7] = [
    (1, "Mon"),
    (2, "Tue"),
    (3, "Wed"),
    (4, "Thu"),
    (5, "Fri"),
    (6, "Sat"),
    (0, "Sun"),
];

/// Which dropdown the prompt box's toolbar has open; one at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormMenu {
    Space,
    Mode,
    Checkout,
    Model,
    Preset,
    Weekday,
    Date,
    Time,
}

/// What the form opens with: blank, or a Routine to edit.
struct Prefill {
    editing: Option<String>,
    name: String,
    prompt: String,
    cron: String,
    at: Option<NaiveDateTime>,
    space_id: Option<String>,
    config: Option<ChatConfig>,
    mode: PermissionMode,
    checkout: RoutineCheckout,
}

impl Default for Prefill {
    fn default() -> Self {
        Self {
            editing: None,
            name: String::new(),
            prompt: String::new(),
            cron: "0 9 * * 1-5".into(),
            at: None,
            space_id: None,
            config: None,
            mode: PermissionMode::AutoReview,
            checkout: RoutineCheckout::MainCheckout,
        }
    }
}

/// `PreviewRoutineSchedule`'s reply.
#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SchedulePreview {
    time_zone: String,
    fires: Vec<DateTime<Utc>>,
}

enum Preview {
    Pending,
    Fires(SchedulePreview),
    Invalid(SharedString),
}

/// The open create/edit modal.
struct RoutineForm {
    /// The Routine being edited; `None` creates one.
    editing: Option<String>,
    name: Entity<ComposerInput>,
    prompt: Entity<ComposerInput>,
    preset: Preset,
    /// "HH:MM" for the daily, weekdays, and weekly presets.
    time: Entity<ComposerInput>,
    weekday: u8,
    /// The custom preset's day.
    date: NaiveDate,
    /// The first day of the month the date picker shows.
    date_month: NaiveDate,
    /// The time picker's hour and minute columns.
    hour_scroll: gpui::ScrollHandle,
    minute_scroll: gpui::ScrollHandle,
    /// A stored cron no preset spells, kept as is.
    cron: Entity<ComposerInput>,
    preview: Preview,
    preview_task: Option<Task<()>>,
    space_id: Option<String>,
    /// The picked model; `None` takes the composer's.
    config: Option<ChatConfig>,
    mode: PermissionMode,
    checkout: RoutineCheckout,
    model_query: Entity<ComposerInput>,
    error: Option<SharedString>,
    saving: bool,
    focus_pending: bool,
    _events: Vec<Subscription>,
}

/// The schedule the form spells.
#[derive(Clone, Debug, PartialEq)]
enum Schedule {
    Cron(String),
    Once(NaiveDateTime),
}

impl Schedule {
    /// The `{cron}` or `{at}` params the Routine RPCs take.
    fn params(&self) -> serde_json::Value {
        match self {
            Self::Cron(cron) => serde_json::json!({ "cron": cron }),
            Self::Once(at) => serde_json::json!({ "cron": "", "at": at }),
        }
    }
}

impl RoutineForm {
    /// The schedule the form spells, or what is wrong with it.
    fn schedule(&self, cx: &App) -> Result<Schedule, SharedString> {
        let time = self.time.read(cx).text();
        match self.preset {
            Preset::Cron => {
                let cron = self.cron.read(cx).text().trim();
                if cron.is_empty() {
                    return Err("Set a cron schedule.".into());
                }
                Ok(Schedule::Cron(cron.to_string()))
            }
            Preset::Custom => parse_time(time)
                .and_then(|(hour, minute)| self.date.and_hms_opt(hour.into(), minute.into(), 0))
                .map(Schedule::Once)
                .ok_or_else(|| "Use a 24-hour time like 09:00.".into()),
            preset => preset_cron(preset, time, self.weekday)
                .map(Schedule::Cron)
                .ok_or_else(|| "Use a 24-hour time like 09:00.".into()),
        }
    }
}

pub struct ScheduledPage {
    state: Entity<AppState>,
    /// The composer's pickers: a new Routine takes their resolved model.
    pickers: Entity<Pickers>,
    form: Option<RoutineForm>,
    /// The prompt-box toolbar's open dropdown; one at a time.
    form_menu: popover::Popup<FormMenu>,
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
            form: None,
            form_menu: popover::Popup::default(),
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
        self.open_form(Prefill::default(), cx);
    }

    /// Open the form on `routine_id`'s configuration; saving updates it.
    pub(crate) fn edit_routine(&mut self, routine_id: &str, cx: &mut Context<Self>) {
        let Some(routine) = self
            .state
            .read(cx)
            .routines
            .iter()
            .find(|view| view.routine.id == routine_id)
            .map(|view| view.routine.clone())
        else {
            return;
        };
        self.open_form(
            Prefill {
                editing: Some(routine.id),
                name: routine.name,
                prompt: routine.prompt,
                cron: routine.cron,
                at: routine.at,
                // A removed Space is not offered; the form picks another.
                space_id: self
                    .state
                    .read(cx)
                    .space_row(&routine.space_id)
                    .map(|_| routine.space_id.clone()),
                mode: routine.config.permission_mode,
                config: Some(routine.config),
                checkout: routine.checkout,
            },
            cx,
        );
    }

    fn open_form(&mut self, prefill: Prefill, cx: &mut Context<Self>) {
        // The form owns the page area, so an open drawer gives way and Back
        // lands on the grid.
        if self.drawer.take().is_some() {
            cx.emit(ScheduledEvent::Drawer(None));
        }
        let input = |placeholder: &'static str, text: &str, cx: &mut Context<Self>| {
            let text = text.to_string();
            cx.new(|cx| {
                let mut input = ComposerInput::new(placeholder, cx);
                input.set_text(text, cx);
                input
            })
        };
        let (preset, time, weekday) = match prefill.at {
            Some(at) => (Preset::Custom, at.format("%H:%M").to_string(), 1),
            None => cron_preset(&prefill.cron).unwrap_or((Preset::Cron, "09:00".into(), 1)),
        };
        let date = prefill
            .at
            .map_or_else(|| Local::now().date_naive(), |at| at.date());
        let name = input("Morning triage", &prefill.name, cx);
        let prompt = input("What should the agent do each run?", &prefill.prompt, cx);
        let time = input("09:00", &time, cx);
        let cron = input("0 9 * * 1-5", &prefill.cron, cx);
        let model_query = input("Search models", "", cx);
        prompt.update(cx, |input, _| input.set_max_display_height(160.0));
        let mut events: Vec<Subscription> = [&name, &prompt]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this: &mut Self, _, event, cx| {
                    this.on_form_input(event, false, cx)
                })
            })
            .collect();
        events.extend([&time, &cron].into_iter().map(|input| {
            cx.subscribe(input, |this: &mut Self, _, event, cx| {
                this.on_form_input(event, true, cx)
            })
        }));
        events.push(
            cx.subscribe(&model_query, |this: &mut Self, _, event, cx| match event {
                ComposerInputEvent::Submitted => {
                    let first = this.model_matches(cx).into_iter().next().cloned();
                    if let Some(model) = first {
                        this.pick_model(&model, cx);
                    }
                }
                ComposerInputEvent::Edited => cx.notify(),
                _ => {}
            }),
        );
        let space_id = prefill.space_id.or_else(|| {
            let state = self.state.read(cx);
            state
                .selected_space
                .clone()
                .filter(|id| state.space_row(id).is_some())
                .or_else(|| state.spaces_sorted().first().map(|space| space.id.clone()))
        });
        self.form = Some(RoutineForm {
            editing: prefill.editing,
            name,
            prompt,
            preset,
            time,
            weekday,
            date,
            date_month: month_start(date),
            hour_scroll: gpui::ScrollHandle::new(),
            minute_scroll: gpui::ScrollHandle::new(),
            cron,
            preview: Preview::Pending,
            preview_task: None,
            space_id,
            config: prefill.config,
            mode: prefill.mode,
            checkout: prefill.checkout,
            model_query,
            error: None,
            saving: false,
            focus_pending: true,
            _events: events,
        });
        self.refresh_preview(cx);
        cx.notify();
    }

    fn on_form_input(
        &mut self,
        event: &ComposerInputEvent,
        schedule: bool,
        cx: &mut Context<Self>,
    ) {
        match event {
            ComposerInputEvent::Submitted => self.submit_form(cx),
            ComposerInputEvent::Edited => {
                if let Some(form) = self.form.as_mut() {
                    form.error = None;
                }
                if schedule {
                    self.refresh_preview(cx);
                }
                cx.notify();
            }
            _ => {}
        }
    }

    fn close_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.form = None;
        self.form_menu = popover::Popup::default();
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn set_preset(&mut self, preset: Preset, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.preset = preset;
        if preset == Preset::Custom
            && let Ok(Schedule::Once(at)) = form.schedule(cx)
            && at <= Local::now().naive_local()
        {
            // A single run starts out in the future: tomorrow, same time.
            form.date = Local::now().date_naive() + chrono::Days::new(1);
        }
        form.error = None;
        self.refresh_preview(cx);
        cx.notify();
    }

    /// Replace the hour and/or minute of the form's time.
    fn set_time(&mut self, hour: Option<u8>, minute: Option<u8>, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let (h, m) = parse_time(form.time.read(cx).text()).unwrap_or((9, 0));
        let text = format!("{:02}:{:02}", hour.unwrap_or(h), minute.unwrap_or(m));
        form.time.update(cx, |input, cx| input.set_text(&text, cx));
        form.error = None;
        self.refresh_preview(cx);
        cx.notify();
    }

    fn set_date(&mut self, date: NaiveDate, cx: &mut Context<Self>) {
        if let Some(form) = self.form.as_mut() {
            form.date = date;
            form.error = None;
        }
        self.refresh_preview(cx);
        cx.notify();
    }

    /// Page the date picker a month back or forward, never before this month.
    fn shift_date_month(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let month = chrono::Months::new(1);
        let next = if forward {
            form.date_month.checked_add_months(month)
        } else {
            form.date_month.checked_sub_months(month)
        };
        if let Some(next) = next.filter(|next| *next >= month_start(Local::now().date_naive())) {
            form.date_month = next;
            cx.notify();
        }
    }

    fn set_weekday(&mut self, weekday: u8, cx: &mut Context<Self>) {
        if let Some(form) = self.form.as_mut() {
            form.weekday = weekday;
        }
        self.refresh_preview(cx);
        cx.notify();
    }

    /// Re-read the schedule's next fires, debounced so typing a cron does
    /// not call per keystroke. A dropped task cancels a stale preview.
    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(form) = self.form.as_mut() else {
            return;
        };
        // No timeZone param: the engine reads the device's zone.
        let params = match form.schedule(cx) {
            Ok(schedule) => schedule.params(),
            Err(problem) => {
                form.preview = Preview::Invalid(problem);
                form.preview_task = None;
                return;
            }
        };
        form.preview_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PREVIEW_DEBOUNCE).await;
            let result = engine
                .client()
                .call(methods::PREVIEW_ROUTINE_SCHEDULE, params)
                .await;
            this.update(cx, |page, cx| {
                let Some(form) = page.form.as_mut() else {
                    return;
                };
                form.preview = match result {
                    Ok(value) => serde_json::from_value::<SchedulePreview>(value)
                        .map(Preview::Fires)
                        .unwrap_or_else(|err| Preview::Invalid(err.to_string().into())),
                    Err(err) => Preview::Invalid(rpc_problem(&err).into()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Catalog models matching the open model list's search, capped.
    fn model_matches<'a>(&self, cx: &'a App) -> Vec<&'a Model> {
        let Some(form) = self.form.as_ref() else {
            return Vec::new();
        };
        let query = form.model_query.read(cx).text().trim().to_lowercase();
        self.pickers
            .read(cx)
            .offered_models()
            .into_iter()
            .filter(|model| {
                query.is_empty()
                    || model.label.to_lowercase().contains(&query)
                    || model.id.to_lowercase().contains(&query)
            })
            .take(MODEL_ROWS)
            .collect()
    }

    fn pick_model(&mut self, model: &Model, cx: &mut Context<Self>) {
        let config = ChatConfig {
            provider: model.provider.clone(),
            model: model.id.clone(),
            reasoning: default_reasoning(&model.reasoning_levels),
            model_options: Default::default(),
            permission_mode: PermissionMode::default(),
            scope: Default::default(),
        };
        if let Some(form) = self.form.as_mut() {
            form.config = Some(config);
            form.error = None;
        }
        self.close_form_menu(cx);
        cx.notify();
    }

    /// Open one prompt-box toolbar menu; the others give way (one card at a
    /// time, the sidebar menus' rule). The Model menu starts with a fresh,
    /// focused search.
    fn open_form_menu(&mut self, menu: FormMenu, window: &mut Window, cx: &mut Context<Self>) {
        self.form_menu.open(menu);
        if menu == FormMenu::Time
            && let Some(form) = self.form.as_ref()
        {
            let (hour, minute) = parse_time(form.time.read(cx).text()).unwrap_or((9, 0));
            // Land the pick in the column's middle row.
            let above = (TIME_MENU_H / (TIME_ROW_H + 2.0) / 2.0) as usize;
            form.hour_scroll
                .scroll_to_top_of_item((hour as usize).saturating_sub(above));
            form.minute_scroll
                .scroll_to_top_of_item((minute as usize).saturating_sub(above));
        }
        if menu == FormMenu::Date
            && let Some(form) = self.form.as_mut()
        {
            form.date_month = month_start(form.date);
        }
        if menu == FormMenu::Model
            && let Some(form) = self.form.as_mut()
        {
            form.model_query
                .update(cx, |input, cx| input.set_text("", cx));
            window.focus(&form.model_query.focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn close_form_menu(&mut self, cx: &mut Context<Self>) {
        if self.form_menu.begin_close() {
            popover::reap_popup(cx, |this: &mut Self| &mut this.form_menu);
            cx.notify();
        }
    }

    fn submit_form(&mut self, cx: &mut Context<Self>) {
        let resolved = self.pickers.read(cx).resolved(cx).chat_config();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.saving {
            return;
        }
        let name = form.name.read(cx).text().trim().to_string();
        let prompt = form.prompt.read(cx).text().trim().to_string();
        let schedule = form.schedule(cx);
        let config = form.config.clone().or(resolved);
        let problem: Option<SharedString> = if name.is_empty() {
            Some("Give the routine a name.".into())
        } else if prompt.is_empty() {
            Some("Write the prompt each run starts with.".into())
        } else if let Err(problem) = &schedule {
            Some(problem.clone())
        } else if let Preview::Invalid(problem) = &form.preview {
            Some(problem.clone())
        } else if form.space_id.is_none() {
            Some("Pick a project.".into())
        } else if config.is_none() {
            Some("Pick a model.".into())
        } else {
            None
        };
        if let Some(problem) = problem {
            form.error = Some(problem);
            cx.notify();
            return;
        }
        let (Ok(schedule), Some(mut config)) = (schedule, config) else {
            return;
        };
        config.permission_mode = form.mode;
        // No timeZone param: runs fire in the device's zone.
        let mut params = serde_json::json!({
            "name": name,
            "spaceId": form.space_id,
            "prompt": prompt,
            "config": config,
            "checkout": form.checkout,
        });
        if let (Some(params), serde_json::Value::Object(schedule)) =
            (params.as_object_mut(), schedule.params())
        {
            params.extend(schedule);
        }
        let method = match &form.editing {
            Some(id) => {
                params["routineId"] = id.clone().into();
                methods::UPDATE_ROUTINE
            }
            None => methods::CREATE_ROUTINE,
        };
        form.saving = true;
        form.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.form = None;
                        page.focus_pending = true;
                    }
                    Err(err) => {
                        if let Some(form) = page.form.as_mut() {
                            form.saving = false;
                            form.error = Some(rpc_problem(&err).into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The open form's editing target, name, and schedule, for tests.
    #[cfg(test)]
    pub(crate) fn form_snapshot(&self, cx: &App) -> Option<(Option<String>, String, String)> {
        let form = self.form.as_ref()?;
        Some((
            form.editing.clone(),
            form.name.read(cx).text().to_string(),
            match form.schedule(cx).ok()? {
                Schedule::Cron(cron) => cron,
                Schedule::Once(at) => at.format("%Y-%m-%dT%H:%M").to_string(),
            },
        ))
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
        } else if self.form.is_some() && key == "escape" {
            self.close_form(window, cx);
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
        let space_removed = routine.paused == Some(RoutinePause::SpaceRemoved);
        let actions = div()
            .flex_none()
            .flex()
            .flex_row()
            .gap(px(2.0))
            .invisible()
            .group_hover(group.clone(), |s| s.visible())
            .when(!space_removed, |actions| {
                actions.child(
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
            })
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
                        schedule_label(&routine.cron, routine.at)
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
                    .child(match (live_run(routine), routine.paused) {
                        (Some(run), _) => live_pill(theme, run.outcome).into_any_element(),
                        (None, Some(pause)) => paused_pill(theme, pause).into_any_element(),
                        (None, None) => div()
                            .text_size(crate::typography::ui_rems(15.0))
                            .text_color(theme.text)
                            .child(match view.next_fire_at {
                                Some(next) => countdown(next, Utc::now()),
                                // A one-time Routine that has fired.
                                None if routine.at.is_some() => "Done".into(),
                                None => String::new(),
                            })
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
            ("Schedule", schedule_label(&routine.cron, routine.at).into()),
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
        // Only an edit to another Space brings it back.
        let space_removed = routine.paused == Some(RoutinePause::SpaceRemoved);
        let edit_id = routine.id.clone();
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
        let live = live_run(routine).map(|run| {
            let outcome = run.outcome;
            let chat_id = run.chat_id.clone();
            div()
                .id("routine-drawer-live")
                .debug_selector(|| "routine-drawer-live".into())
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .px(px(12.0))
                .py(px(8.0))
                .rounded(px(8.0))
                .bg(ink(0.05))
                .border_1()
                .border_color(hairline(0.08))
                .text_size(crate::typography::ui_rems(12.5))
                .text_color(theme.text)
                .child(live_dot(theme, outcome))
                .child(div().flex_1().child(live_banner(outcome)))
                .when_some(chat_id, |banner, chat_id| {
                    banner
                        .cursor_pointer()
                        .hover(|banner| banner.bg(ink(0.08)))
                        .child(div().text_color(theme.text_muted).child("Open"))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.open_run(chat_id.clone(), cx)),
                        )
                })
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
                if this.confirm.is_none() && this.form.is_none() {
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
                                    .when(space_removed, |button| {
                                        button
                                            .debug_selector(|| "routine-drawer-run-disabled".into())
                                            .opacity(0.4)
                                            .cursor_default()
                                    })
                                    .when(!space_removed, |button| {
                                        button.on_click(cx.listener(move |this, _, _, cx| {
                                            this.run_now(run_id.clone(), cx)
                                        }))
                                    }),
                            )
                            .when(!space_removed, |row| {
                                row.child(
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
                            })
                            .child(
                                popover::btn_ghost(theme, "Edit", "routine-drawer-edit")
                                    .id("routine-drawer-edit")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.edit_routine(&edit_id, cx)
                                    })),
                            )
                            .child(
                                popover::btn_ghost(theme, "Delete", "routine-drawer-delete")
                                    .id("routine-drawer-delete")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.ask_delete(delete_id.clone(), window, cx)
                                    })),
                            ),
                    )
                    .children(live)
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

    /// The no-routines body (holt settings.archived.tsx's centered icon +
    /// headline + hint); creating happens in the header's New routine button.
    fn render_empty(theme: &Theme) -> AnyElement {
        div()
            .mt(px(96.0))
            .flex()
            .flex_col()
            .items_center()
            .text_center()
            .text_color(theme.text_muted.opacity(0.5))
            .child(
                icon(icons::CLOCK_CIRCLE)
                    .size(px(28.0))
                    .text_color(theme.text_muted.opacity(0.2)),
            )
            .child(
                div()
                    .mt(px(12.0))
                    .text_size(crate::typography::ui_rems(14.0))
                    .child(SharedString::from("No routines yet")),
            )
            .child(
                div()
                    .mt(px(4.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted.opacity(0.4))
                    .child(SharedString::from(
                        "Run a prompt on a schedule, in a fresh chat each time.",
                    )),
            )
            .into_any_element()
    }

    fn render_header(&self, theme: &Theme, count: usize, cx: &mut Context<Self>) -> gpui::Div {
        let summary = match count {
            0 => "No routines".to_string(),
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

    fn render_form(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let form = self.form.as_mut()?;
        if std::mem::take(&mut form.focus_pending) {
            window.focus(&form.name.focus_handle(cx), cx);
        }
        let (name, prompt) = (form.name.clone(), form.prompt.clone());
        let time = form.time.clone();
        let (preset, weekday, form_date) = (form.preset, form.weekday, form.date);
        let (space_id, mode, checkout) = (form.space_id.clone(), form.mode, form.checkout);
        let (error, saving, editing) = (form.error.clone(), form.saving, form.editing.is_some());
        let model_query = form.model_query.clone();
        let picked = form.config.clone();
        let preview = match &form.preview {
            Preview::Pending => None,
            Preview::Fires(preview) => Some(Ok(preview.clone())),
            Preview::Invalid(problem) => Some(Err(problem.clone())),
        };
        let spaces: Vec<(String, SharedString)> = self
            .state
            .read(cx)
            .spaces_sorted()
            .into_iter()
            .map(|space| (space.id.clone(), space.display_name().to_string().into()))
            .collect();
        let model: SharedString = {
            let pickers = self.pickers.read(cx);
            let (provider, model) = match &picked {
                Some(config) => (Some(config.provider.clone()), Some(config.model.clone())),
                None => {
                    let resolved = pickers.resolved(cx);
                    (resolved.provider, resolved.model)
                }
            };
            match (provider, model) {
                (Some(provider), Some(model)) => pickers
                    .model_label(&provider, &model)
                    .map(str::to_string)
                    .unwrap_or_else(|| short_model(&model).to_string())
                    .into(),
                _ => "Pick a model".into(),
            }
        };
        // The prompt box's toolbar: each chip opens a dropdown menu, one at
        // a time; the model rows exist only while the Model menu is up.
        let form_menu = self.form_menu.get().copied();
        let model_rows: Vec<(String, ProviderId, SharedString, bool)> =
            if form_menu == Some(FormMenu::Model) {
                let current = picked.as_ref().map(|config| config.model.as_str());
                self.model_matches(cx)
                    .into_iter()
                    .map(|row| {
                        (
                            row.id.clone(),
                            row.provider.clone(),
                            row.label.clone().into(),
                            current == Some(row.id.as_str()),
                        )
                    })
                    .collect()
            } else {
                Vec::new()
            };

        let space_label: SharedString = space_id
            .as_deref()
            .and_then(|id| {
                spaces
                    .iter()
                    .find(|(space, _)| space == id)
                    .map(|(_, label)| label.clone())
            })
            .unwrap_or_else(|| "Pick a project".into());
        let space_card =
            (form_menu == Some(FormMenu::Space)).then(|| self.form_space_menu(theme, cx));
        let mode_card = (form_menu == Some(FormMenu::Mode)).then(|| self.form_mode_menu(theme, cx));
        let time_card = (form_menu == Some(FormMenu::Time)).then(|| self.form_time_menu(theme, cx));
        let checkout_card =
            (form_menu == Some(FormMenu::Checkout)).then(|| self.form_checkout_menu(theme, cx));
        let model_card = (form_menu == Some(FormMenu::Model))
            .then(|| self.form_model_menu(theme, model_query, model_rows, cx));
        let preset_card =
            (form_menu == Some(FormMenu::Preset)).then(|| self.form_preset_menu(theme, cx));
        let weekday_card =
            (form_menu == Some(FormMenu::Weekday)).then(|| self.form_weekday_menu(theme, cx));
        let preset_label = PRESETS
            .iter()
            .find(|(value, _)| *value == preset)
            .map_or("Cron", |(_, label)| label);
        let preset_chip = self.form_menu_trigger(
            select_chip(
                theme,
                "routine-preset",
                preset_label.into(),
                self.form_menu.as_open().copied() == Some(FormMenu::Preset),
            )
            .min_w(px(112.0))
            .justify_between(),
            FormMenu::Preset,
            "routine-preset",
            false,
            preset_card,
            cx,
        );
        let connective = |word: &'static str| {
            div()
                .flex_none()
                .text_size(crate::typography::ui_rems(13.0))
                .text_color(theme.text_muted)
                .child(word)
        };
        let weekday_chip = (preset == Preset::Weekly).then(|| {
            let label = WEEKDAYS
                .iter()
                .find(|(day, _)| *day == weekday)
                .map_or("Mon", |(_, label)| label);
            self.form_menu_trigger(
                select_chip(
                    theme,
                    "routine-weekday",
                    label.into(),
                    self.form_menu.as_open().copied() == Some(FormMenu::Weekday),
                ),
                FormMenu::Weekday,
                "routine-weekday",
                false,
                weekday_card,
                cx,
            )
        });
        let date_card = (form_menu == Some(FormMenu::Date)).then(|| self.form_date_menu(theme, cx));
        let date_chip = (preset == Preset::Custom).then(|| {
            self.form_menu_trigger(
                select_chip(
                    theme,
                    "routine-date",
                    date_label(form_date, Local::now().date_naive()).into(),
                    self.form_menu.as_open().copied() == Some(FormMenu::Date),
                ),
                FormMenu::Date,
                "routine-date",
                false,
                date_card,
                cx,
            )
        });
        let time_chip = matches!(
            preset,
            Preset::Daily | Preset::Weekdays | Preset::Weekly | Preset::Custom
        )
        .then(|| {
            let text = time.read(cx).text().trim();
            let label = if text.is_empty() { "09:00" } else { text }.to_string();
            self.form_menu_trigger(
                select_chip(
                    theme,
                    "routine-time",
                    label.into(),
                    self.form_menu.as_open().copied() == Some(FormMenu::Time),
                ),
                FormMenu::Time,
                "routine-time",
                false,
                time_card,
                cx,
            )
        });
        // The schedule reads as one sentence: preset, (day,) "at" time, then
        // the engine's preview of what it spells.
        let preview_line = div()
            .id("routine-form-preview")
            .debug_selector(|| "routine-form-preview".into())
            .flex_1()
            .min_w_0()
            .pl(px(4.0))
            .truncate()
            .text_size(crate::typography::ui_rems(12.5))
            .map(|line| match preview {
                None => line.text_color(theme.text_muted).child("\u{2026}"),
                Some(Ok(preview)) => line.text_color(theme.text_muted).child(format!(
                    "{} \u{b7} Next {}",
                    preview.time_zone,
                    preview_fires(&preview.fires)
                )),
                Some(Err(problem)) => line.text_color(theme.danger).child(problem),
            });
        let schedule_bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .p(px(6.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(hairline(0.08))
            .bg(ink(0.04))
            .child(preset_chip)
            .children(weekday_chip)
            .children(date_chip)
            .when(time_chip.is_some(), |bar| bar.child(connective("at")))
            .children(time_chip)
            .child(preview_line);
        let space_chip = self.form_toolbar_chip(
            theme,
            FormMenu::Space,
            "routine-space",
            Some(icons::FOLDER),
            space_label,
            false,
            space_card,
            cx,
        );
        let mode_chip = self.form_toolbar_chip(
            theme,
            FormMenu::Mode,
            "routine-mode",
            Some(icons::SHIELD),
            mode_label(mode).into(),
            false,
            mode_card,
            cx,
        );
        let checkout_chip = self.form_toolbar_chip(
            theme,
            FormMenu::Checkout,
            "routine-checkout",
            Some(match checkout {
                RoutineCheckout::MainCheckout => icons::FOLDER,
                RoutineCheckout::NewWorktree => icons::FOLDER_WITH_FILES,
            }),
            match checkout {
                RoutineCheckout::MainCheckout => "Main checkout",
                RoutineCheckout::NewWorktree => "New worktree",
            }
            .into(),
            true,
            checkout_card,
            cx,
        );
        let model_chip = self.form_toolbar_chip(
            theme,
            FormMenu::Model,
            "routine-model",
            None,
            model,
            true,
            model_card,
            cx,
        );

        // The form owns the page area (a navigation, not an overlay); the
        // page's scroll container handles overflow.
        let card = div()
            .id("routine-form-page")
            .w_full()
            .flex()
            .flex_col()
            .text_color(theme.text)
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                let keystroke = &ev.keystroke;
                if keystroke.key == "escape" {
                    cx.stop_propagation();
                    if this.form_menu.is_open() {
                        this.close_form_menu(cx);
                    } else {
                        this.close_form(window, cx);
                    }
                } else if keystroke.key == "enter" && keystroke.modifiers.platform {
                    cx.stop_propagation();
                    this.submit_form(cx);
                }
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .child(
                                icon_button(
                                    theme,
                                    "routine-form-back".into(),
                                    icons::ARROW_LEFT,
                                    false,
                                )
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.close_form(window, cx)),
                                ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.0))
                                    .child(widgets::page_header(
                                        theme,
                                        if editing {
                                            "Edit routine"
                                        } else {
                                            "New routine"
                                        },
                                        None,
                                    ))
                                    .child(
                                        div()
                                            .text_size(crate::typography::ui_rems(12.5))
                                            .text_color(theme.text_muted)
                                            .child("When it fires, what it runs, and how."),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child("\u{2318}\u{21a9} to save"),
                    ),
            )
            .child(field(
                theme,
                "Name",
                popover::dialog_field(name.into_any_element()),
            ))
            .child(field(theme, "Schedule", schedule_bar))
            // The prompt box wears the composer's shape: the input on top and
            // a toolbar of dropdown chips at the bottom (project + permission
            // mode left, checkout + model right).
            .child(field(
                theme,
                "Prompt",
                div()
                    .flex()
                    .flex_col()
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(hairline(0.08))
                    .bg(ink(0.04))
                    .child(
                        div()
                            .w_full()
                            .px(px(12.0))
                            .pt(px(10.0))
                            .min_h(px(96.0))
                            .text_size(crate::typography::ui_rems(14.0))
                            .child(prompt.into_any_element()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(4.0))
                            .border_t_1()
                            .border_color(hairline(0.06))
                            .px(px(8.0))
                            .py(px(6.0))
                            .child(space_chip)
                            .child(mode_chip)
                            .child(div().flex_1())
                            .child(checkout_chip)
                            .child(model_chip),
                    ),
            ))
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
                    .items_center()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(theme, "Cancel", "routine-form-cancel")
                            .id("routine-form-cancel")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_form(window, cx)),
                            ),
                    )
                    .child(
                        popover::btn_primary(
                            theme,
                            match (saving, editing) {
                                (true, _) => "Saving…",
                                (false, true) => "Save",
                                (false, false) => "Create",
                            },
                        )
                        .id("routine-form-save")
                        .when(saving, |el| el.opacity(0.6))
                        .on_click(cx.listener(|this, _, _, cx| this.submit_form(cx))),
                    ),
            )
            .into_any_element();
        Some(card)
    }

    /// A prompt-box toolbar chip with its dropdown mounted while `menu` is
    /// the open one. `align_end` right-aligns the card to the chip (trailing
    /// chips open leftward, staying inside the page).
    #[allow(clippy::too_many_arguments)]
    fn form_toolbar_chip(
        &self,
        theme: &Theme,
        menu: FormMenu,
        id: &'static str,
        icon_path: Option<&'static str>,
        label: SharedString,
        align_end: bool,
        card: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.form_menu.as_open().copied() == Some(menu);
        let chip = form_chip(theme, id, icon_path, label, open);
        self.form_menu_trigger(chip, menu, id, align_end, card, cx)
    }

    /// Wire `chip` to toggle `menu`, with its dropdown mounted below it while
    /// `menu` is up (or closing).
    fn form_menu_trigger(
        &self,
        chip: gpui::Stateful<gpui::Div>,
        menu: FormMenu,
        id: &'static str,
        align_end: bool,
        card: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mounted = self.form_menu.get().copied() == Some(menu);
        let chip = chip
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.form_menu
                        .note_trigger_press_matching(|kind| *kind == menu);
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                if this.form_menu.take_press_was_open() {
                    this.close_form_menu(cx);
                } else {
                    this.open_form_menu(menu, window, cx);
                }
            }));
        match (mounted, card) {
            (true, Some(card)) => {
                let closing = self.form_menu.closing_since();
                let menu_id: SharedString = format!("{id}-menu").into();
                let anchored = if align_end {
                    popover::anchored_menu_below_end(menu_id, card, closing)
                } else {
                    popover::anchored_menu_below(menu_id, card, closing)
                };
                chip.relative().child(anchored).into_any_element()
            }
            _ => chip.into_any_element(),
        }
    }

    /// The schedule preset menu.
    fn form_preset_menu(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let current = self.form.as_ref().map(|form| form.preset);
        popover::popover_card(theme)
            .w(px(160.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_form_menu(cx)))
            .children(PRESETS.into_iter().enumerate().map(|(ix, (value, label))| {
                popover::menu_row(
                    theme,
                    current == Some(value),
                    format!("routine-preset-fade-{ix}"),
                )
                .id(("routine-preset-row", ix))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_preset(value, cx);
                    this.close_form_menu(cx);
                }))
                .child(label)
            }))
            .into_any_element()
    }

    /// The weekly preset's day menu, Monday first.
    fn form_weekday_menu(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let current = self.form.as_ref().map(|form| form.weekday);
        popover::popover_card(theme)
            .w(px(140.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_form_menu(cx)))
            .children(WEEKDAYS.into_iter().map(|(day, label)| {
                popover::menu_row(
                    theme,
                    current == Some(day),
                    format!("routine-weekday-fade-{day}"),
                )
                .id(("routine-weekday-row", day as usize))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_weekday(day, cx);
                    this.close_form_menu(cx);
                }))
                .child(weekday_name(label))
            }))
            .into_any_element()
    }

    /// The Custom preset's day: a month calendar, Monday first. Days before
    /// today are not offered.
    fn form_date_menu(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(form) = self.form.as_ref() else {
            return div().into_any_element();
        };
        let (picked, month, today) = (form.date, form.date_month, Local::now().date_naive());
        let lead = month.weekday().num_days_from_monday() as usize;
        let days = month
            .checked_add_months(chrono::Months::new(1))
            .map_or(31, |next| (next - month).num_days()) as usize;
        let at_first_month = month <= month_start(today);
        let (hover, selected) = (theme.element_hover, crate::theme::card_selected_bg());
        let arrow = |id: &'static str, path: &'static str, enabled: bool, forward: bool| {
            div()
                .id(id)
                .size(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .child(icon(path).size(px(14.0)).text_color(theme.text_muted))
                .when(!enabled, |el| el.opacity(0.35))
                .when(enabled, |el| {
                    el.cursor_pointer().hover(move |s| s.bg(hover)).on_click(
                        cx.listener(move |this, _, _, cx| this.shift_date_month(forward, cx)),
                    )
                })
        };
        let cell = || {
            div()
                .w(px(32.0))
                .h(px(28.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .text_size(crate::typography::ui_rems(12.5))
        };
        let mut grid = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .w(px(7.0 * 32.0))
            .children(WEEKDAYS.iter().map(|(_, label)| {
                cell()
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_faint)
                    .child(&label[..2])
            }))
            .children((0..lead).map(|_| cell()));
        for day in 0..days {
            let date = month + chrono::Days::new(day as u64);
            let label = (day + 1).to_string();
            grid = grid.child(
                if date < today {
                    cell()
                        .text_color(theme.text_faint.opacity(0.5))
                        .child(label)
                        .into_any_element()
                } else {
                    cell()
                        .id(("routine-date-day", day))
                        .cursor_pointer()
                        .when(date == today, |el| {
                            el.font_weight(gpui::FontWeight::SEMIBOLD)
                        })
                        .map(|el| {
                            if date == picked {
                                el.bg(selected).text_color(theme.text)
                            } else {
                                el.text_color(theme.text_muted).hover(move |s| s.bg(hover))
                            }
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_date(date, cx);
                            this.close_form_menu(cx);
                        }))
                        .child(label)
                        .into_any_element()
                }
                .into_any_element(),
            );
        }
        popover::popover_card(theme)
            .p(px(8.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_form_menu(cx)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .pl(px(6.0))
                    .child(
                        div()
                            .flex_1()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(month.format("%B %Y").to_string()),
                    )
                    .child(arrow(
                        "routine-date-prev",
                        icons::ALT_ARROW_LEFT,
                        !at_first_month,
                        false,
                    ))
                    .child(arrow(
                        "routine-date-next",
                        icons::ALT_ARROW_RIGHT,
                        true,
                        true,
                    )),
            )
            .child(grid)
            .into_any_element()
    }

    /// The Space menu: one row per project, the form's pick marked.
    fn form_space_menu(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.form.as_ref().and_then(|form| form.space_id.clone());
        let rows: Vec<(String, SharedString, bool)> = self
            .state
            .read(cx)
            .spaces_sorted()
            .into_iter()
            .map(|space| {
                let active = selected.as_deref() == Some(space.id.as_str());
                (
                    space.id.clone(),
                    space.display_name().to_string().into(),
                    active,
                )
            })
            .collect();
        popover::popover_card(theme)
            .w(px(200.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_form_menu(cx)))
            .children(
                rows.into_iter()
                    .enumerate()
                    .map(|(ix, (id, label, active))| {
                        popover::menu_row(theme, active, format!("routine-space-fade-{ix}"))
                            .id(("routine-space-row", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(form) = this.form.as_mut() {
                                    form.space_id = Some(id.clone());
                                    form.error = None;
                                }
                                this.close_form_menu(cx);
                            }))
                            .child(label)
                    }),
            )
            .into_any_element()
    }

    /// The permission-mode menu: the three tiers, the form's pick marked.
    fn form_mode_menu(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let current = self.form.as_ref().map(|form| form.mode);
        popover::popover_card(theme)
            .w(px(200.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_form_menu(cx)))
            .children(MODE_TIERS.into_iter().enumerate().map(|(ix, tier)| {
                popover::menu_row(
                    theme,
                    current == Some(tier),
                    format!("routine-mode-fade-{ix}"),
                )
                .id(("routine-mode-row", ix))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(form) = this.form.as_mut() {
                        form.mode = tier;
                    }
                    this.close_form_menu(cx);
                }))
                .child(mode_label(tier))
            }))
            .into_any_element()
    }

    /// The time picker: an hour column and a minute column, each scrolled to
    /// the current pick when the menu opens. An hour keeps the menu open; a
    /// minute completes the pick.
    fn form_time_menu(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(form) = self.form.as_ref() else {
            return div().into_any_element();
        };
        let (hour, minute) = parse_time(form.time.read(cx).text()).unwrap_or((9, 0));
        let (hour_scroll, minute_scroll) = (form.hour_scroll.clone(), form.minute_scroll.clone());
        let column = |id: &'static str, scroll: &gpui::ScrollHandle| {
            div()
                .id(id)
                .w(px(56.0))
                .h(px(TIME_MENU_H))
                .p(px(4.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .overflow_y_scroll()
                .track_scroll(scroll)
                .occlude()
        };
        let (hover, text) = (theme.element_hover, theme.text);
        let cell = |active: bool| {
            div()
                .flex_none()
                .h(px(TIME_ROW_H))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .cursor_pointer()
                .text_size(crate::typography::ui_rems(13.0))
                .when(active, |el| {
                    el.bg(crate::theme::card_selected_bg())
                        .text_color(theme.text)
                })
                .when(!active, |el| {
                    el.text_color(theme.text_muted)
                        .hover(move |s| s.bg(hover).text_color(text))
                })
        };
        popover::popover_card(theme)
            .p_0()
            .flex()
            .flex_row()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_form_menu(cx)))
            .child(
                column("routine-time-hours", &hour_scroll).children((0..24u8).map(|h| {
                    cell(h == hour)
                        .id(("routine-time-hour", h as usize))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.set_time(Some(h), None, cx)),
                        )
                        .child(format!("{h:02}"))
                })),
            )
            .child(div().w(px(1.0)).bg(hairline(0.08)))
            .child(
                column("routine-time-minutes", &minute_scroll).children((0..60u8).map(|m| {
                    cell(m == minute)
                        .id(("routine-time-minute", m as usize))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_time(None, Some(m), cx);
                            this.close_form_menu(cx);
                        }))
                        .child(format!("{m:02}"))
                })),
            )
            .into_any_element()
    }

    /// The checkout-kind menu: the Space's main checkout or a new worktree.
    fn form_checkout_menu(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let current = self.form.as_ref().map(|form| form.checkout);
        popover::popover_card(theme)
            .w(px(200.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_form_menu(cx)))
            .children(
                [
                    ("Main checkout", RoutineCheckout::MainCheckout),
                    ("New worktree", RoutineCheckout::NewWorktree),
                ]
                .into_iter()
                .enumerate()
                .map(|(ix, (label, value))| {
                    popover::menu_row(
                        theme,
                        current == Some(value),
                        format!("routine-checkout-fade-{ix}"),
                    )
                    .id(("routine-checkout-row", ix))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(form) = this.form.as_mut() {
                            form.checkout = value;
                        }
                        this.close_form_menu(cx);
                    }))
                    .child(label)
                }),
            )
            .into_any_element()
    }

    /// The model menu: the old inline panel (search + matches) re-homed in a
    /// dropdown card.
    fn form_model_menu(
        &self,
        theme: &Theme,
        model_query: Entity<ComposerInput>,
        rows: Vec<(String, ProviderId, SharedString, bool)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let empty = rows.is_empty();
        popover::popover_card(theme)
            .w(px(260.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_form_menu(cx)))
            .child(popover::search_input_frame(
                theme,
                model_query.into_any_element(),
            ))
            .child(
                div()
                    .id("routine-model-list")
                    .max_h(px(220.0))
                    .overflow_y_scroll()
                    .occlude()
                    .flex()
                    .flex_col()
                    .when(empty, |list| {
                        list.child(
                            div()
                                .px(px(8.0))
                                .py(px(6.0))
                                .text_size(crate::typography::ui_rems(12.5))
                                .text_color(theme.text_muted)
                                .child("No matching models"),
                        )
                    })
                    .children(rows.into_iter().enumerate().map(
                        |(ix, (id, provider, label, active))| {
                            popover::menu_row(theme, active, format!("routine-model-fade-{ix}"))
                                .id(("routine-model-row", ix))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let model = this
                                        .pickers
                                        .read(cx)
                                        .offered_models()
                                        .into_iter()
                                        .find(|model| model.id == id && model.provider == provider)
                                        .cloned();
                                    if let Some(model) = model {
                                        this.pick_model(&model, cx);
                                    }
                                }))
                                .child(div().flex_1().min_w_0().truncate().child(label))
                        },
                    )),
            )
            .into_any_element()
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

        let form = self.render_form(&theme, window, cx);
        // The form owns the page area (a navigation, not an overlay):
        // header, grid, drawer, and dialogs stand down while it is open.
        let form_open = form.is_some();
        let body: AnyElement = match form {
            Some(form) => form,
            None => {
                let area: AnyElement = if routines.is_empty() {
                    Self::render_empty(&theme)
                } else {
                    let cards: Vec<AnyElement> = routines
                        .iter()
                        .map(|view| self.render_card(&theme, view, cx))
                        .collect();
                    let columns = self.columns.clone();
                    div()
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
                        .children(cards)
                        .into_any_element()
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(20.0))
                    .child(self.render_header(&theme, routines.len(), cx))
                    .child(area)
                    .into_any_element()
            }
        };

        let confirm = if form_open {
            None
        } else {
            self.confirm
                .clone()
                .and_then(|id| self.render_confirm(&theme, &id, window, cx))
        };
        // A Routine deleted elsewhere takes its drawer with it.
        if self
            .drawer
            .as_ref()
            .is_some_and(|id| !routines.iter().any(|view| &view.routine.id == id))
        {
            self.drawer = None;
        }
        let drawer = if form_open {
            None
        } else {
            self.render_drawer(&theme, cx)
        };

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
                            .child(body),
                    ),
            )
            .children(drawer)
            .children(confirm)
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

/// The form preview's fire list, deduplicated: one time-of-day across all
/// fires collapses to the date span ("Oct 8 – Oct 10 at 09:00"); same-day
/// fires share the date ("Oct 7, 17:00 · 18:00 · 19:00"); anything else
/// lists each fire.
fn preview_fires(fires: &[DateTime<Utc>]) -> String {
    let local: Vec<DateTime<Local>> = fires
        .iter()
        .map(|fire| fire.with_timezone(&Local))
        .collect();
    match local.as_slice() {
        [] => String::new(),
        [only] => only.format("%b %-d, %H:%M").to_string(),
        many => {
            let times: Vec<String> = many
                .iter()
                .map(|at| at.format("%H:%M").to_string())
                .collect();
            let one_time = times.iter().all(|time| *time == times[0]);
            if one_time {
                let days: Vec<NaiveDate> = many.iter().map(|at| at.date_naive()).collect();
                let consecutive = days
                    .windows(2)
                    .all(|pair| (pair[1] - pair[0]).num_days() == 1);
                let dates = if consecutive {
                    format!(
                        "{} \u{2013} {}",
                        days[0].format("%b %-d"),
                        days[days.len() - 1].format("%b %-d")
                    )
                } else {
                    let mut seen: Vec<NaiveDate> = Vec::new();
                    for day in days {
                        if seen.last() != Some(&day) {
                            seen.push(day);
                        }
                    }
                    seen.iter()
                        .map(|day| day.format("%b %-d").to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                format!("{dates} at {}", times[0])
            } else {
                let first_date = many[0].date_naive();
                let same_day = many.iter().all(|at| at.date_naive() == first_date);
                if same_day {
                    format!(
                        "{}, {}",
                        first_date.format("%b %-d"),
                        times.join(" \u{b7} ")
                    )
                } else {
                    many.iter()
                        .map(|at| at.format("%b %-d, %H:%M").to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            }
        }
    }
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

/// The Routine's run in flight, if any (at most one: a fire is skipped
/// while a run is live).
pub(crate) fn live_run(routine: &Routine) -> Option<&RoutineRun> {
    routine.runs.iter().find(|run| run.outcome.is_live())
}

fn live_dot(theme: &Theme, outcome: RunOutcome) -> gpui::Div {
    div()
        .flex_none()
        .size(px(7.0))
        .rounded_full()
        .bg(outcome_color(theme, outcome))
}

fn live_pill(theme: &Theme, outcome: RunOutcome) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .text_size(crate::typography::ui_rems(15.0))
        .text_color(theme.text)
        .child(live_dot(theme, outcome))
        .child(outcome_label(outcome))
}

fn live_banner(outcome: RunOutcome) -> &'static str {
    if outcome == RunOutcome::Waiting {
        "Waiting for your input."
    } else {
        "Running now."
    }
}

fn pause_banner(pause: RoutinePause) -> &'static str {
    match pause {
        RoutinePause::User => "Paused. It won't fire until you resume it.",
        RoutinePause::SpaceRemoved => {
            "Paused because its project was removed. Edit it to pick another project."
        }
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

/// A schedule-bar dropdown trigger: the prompt toolbar's ghost chip (no fill
/// until hovered or open) at the bar's size; the bar draws the frame.
fn select_chip(
    theme: &Theme,
    id: &'static str,
    label: SharedString,
    open: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .h(px(28.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .pl(px(10.0))
        .pr(px(8.0))
        .rounded(px(8.0))
        .text_size(crate::typography::ui_rems(13.0))
        .text_color(theme.text)
        .bg(if open {
            theme.element_hover
        } else {
            crate::motion::hover_blend(id, gpui::transparent_black(), theme.element_hover)
        })
        .on_hover(crate::motion::hover_listener(id))
        .cursor_pointer()
        .child(label)
        .child(
            icon(icons::ALT_ARROW_DOWN)
                .size(px(12.0))
                .text_color(theme.text_muted.opacity(0.7)),
        )
}

/// "Mon" → "Monday".
/// A picked day: "Today", "Tomorrow", else "Fri, Oct 9".
fn month_start(date: NaiveDate) -> NaiveDate {
    date.with_day(1).unwrap_or(date)
}

fn date_label(date: NaiveDate, today: NaiveDate) -> String {
    match (date - today).num_days() {
        0 => "Today".into(),
        1 => "Tomorrow".into(),
        _ => date.format("%a, %b %-d").to_string(),
    }
}

fn weekday_name(short: &str) -> &'static str {
    match short {
        "Mon" => "Monday",
        "Tue" => "Tuesday",
        "Wed" => "Wednesday",
        "Thu" => "Thursday",
        "Fri" => "Friday",
        "Sat" => "Saturday",
        _ => "Sunday",
    }
}

/// `openai/gpt-5.4` → `gpt-5.4` when the catalog hasn't named the model.
fn short_model(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// "HH:MM" on a 24-hour clock.
fn parse_time(text: &str) -> Option<(u8, u8)> {
    let (hour, minute) = text.trim().split_once(':')?;
    let digits = |part: &str, max: u8| {
        (!part.is_empty() && part.len() <= 2 && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse::<u8>().ok())
            .flatten()
            .filter(|value| *value <= max)
    };
    Some((digits(hour, 23)?, digits(minute, 59)?))
}

/// The cron a preset spells at `time` on `weekday` (0 = Sunday). `None`
/// for an unreadable time, and for the custom preset, which has no
/// spelling of its own.
fn preset_cron(preset: Preset, time: &str, weekday: u8) -> Option<String> {
    if preset == Preset::Hourly {
        return Some("0 * * * *".into());
    }
    let (hour, minute) = parse_time(time)?;
    let days = match preset {
        Preset::Daily => "*".to_string(),
        Preset::Weekdays => "1-5".to_string(),
        Preset::Weekly => weekday.to_string(),
        Preset::Hourly | Preset::Custom | Preset::Cron => return None,
    };
    Some(format!("{minute} {hour} * * {days}"))
}

/// The preset a stored cron reads as, with its "HH:MM" and weekday; `None`
/// when only a custom cron says it.
fn cron_preset(cron: &str) -> Option<(Preset, String, u8)> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    let [minute, hour, "*", "*", days] = fields[..] else {
        return None;
    };
    if [minute, hour, days] == ["0", "*", "*"] {
        return Some((Preset::Hourly, "09:00".into(), 1));
    }
    let (hour, minute) = parse_time(&format!("{hour}:{minute}"))?;
    let time = format!("{hour:02}:{minute:02}");
    match days {
        "*" => Some((Preset::Daily, time, 1)),
        "1-5" => Some((Preset::Weekdays, time, 1)),
        day => {
            let day = day
                .parse::<u8>()
                .ok()
                .filter(|d| *d <= 6 && d.to_string() == day)?;
            Some((Preset::Weekly, time, day))
        }
    }
}

/// A stored cron in words ("Weekdays at 09:00"); a cron no preset spells
/// shows as itself.
fn schedule_label(cron: &str, at: Option<NaiveDateTime>) -> String {
    if let Some(at) = at {
        return at.format("Once on %b %-d at %H:%M").to_string();
    }
    let Some((preset, time, weekday)) = cron_preset(cron) else {
        return cron.to_string();
    };
    match preset {
        Preset::Hourly => "Hourly".into(),
        Preset::Daily => format!("Daily at {time}"),
        Preset::Weekdays => format!("Weekdays at {time}"),
        Preset::Weekly => {
            let day = WEEKDAYS
                .iter()
                .find(|(day, _)| *day == weekday)
                .map_or("Sun", |(_, label)| label);
            format!("{}s at {time}", weekday_name(day))
        }
        Preset::Custom | Preset::Cron => cron.to_string(),
    }
}

/// An RPC failure as the form shows it: the engine's message without the
/// transport's "bad params:" framing.
fn rpc_problem(error: &holt_rpc::RpcError) -> String {
    match error {
        holt_rpc::RpcError::BadParams(message) => {
            let mut message = message.clone();
            if let Some(first) = message.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            message
        }
        other => other.to_string(),
    }
}

/// A labeled form row in the create/edit form.
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

/// One prompt-box toolbar chip: the composer footer's ghost-button recipe
/// (pickers/common.rs `footer_chip`), the leading icon optional.
fn form_chip(
    theme: &Theme,
    id: &'static str,
    icon_path: Option<&'static str>,
    label: SharedString,
    open: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(20.0))
        .max_w(px(200.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .px(px(8.0))
        .rounded(px(6.0))
        .text_size(crate::typography::ui_rems(12.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(crate::motion::hover_blend(
            id,
            theme.text_muted.opacity(0.7),
            theme.text.opacity(0.8),
        ))
        .bg(if open {
            theme.element_hover
        } else {
            crate::motion::hover_blend(id, gpui::transparent_black(), theme.element_hover)
        })
        .on_hover(crate::motion::hover_listener(id))
        .cursor_pointer()
        .when_some(icon_path, |el, path| {
            el.child(
                icon(path)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            )
        })
        .child(div().min_w_0().truncate().child(label))
        .child(
            icon(icons::ALT_ARROW_DOWN)
                .size(px(12.0))
                .text_color(theme.text_muted.opacity(0.5)),
        )
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
    fn preview_fires_deduplicates_repeated_dates_and_times() {
        use chrono::TimeZone;
        let at = |day: u32, hour: u32| {
            Local
                .with_ymd_and_hms(2026, 10, day, hour, 0, 0)
                .unwrap()
                .with_timezone(&Utc)
        };
        // Daily: consecutive days, one time — the span carries the dates.
        assert_eq!(
            preview_fires(&[at(8, 9), at(9, 9), at(10, 9)]),
            "Oct 8 \u{2013} Oct 10 at 09:00"
        );
        // Weekly: one time, spread out — dates listed.
        assert_eq!(
            preview_fires(&[at(9, 18), at(16, 18), at(23, 18)]),
            "Oct 9, Oct 16, Oct 23 at 18:00"
        );
        // Hourly: one day, many times — the date carries the times.
        assert_eq!(
            preview_fires(&[at(7, 17), at(7, 18), at(7, 19)]),
            "Oct 7, 17:00 \u{b7} 18:00 \u{b7} 19:00"
        );
        assert_eq!(preview_fires(&[at(8, 9)]), "Oct 8, 09:00");
        assert_eq!(preview_fires(&[]), "");
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
    fn presets_round_trip_through_cron() {
        for (cron, preset, time, weekday) in [
            ("0 * * * *", Preset::Hourly, "09:00", 1),
            ("30 7 * * *", Preset::Daily, "07:30", 1),
            ("0 9 * * 1-5", Preset::Weekdays, "09:00", 1),
            ("15 18 * * 0", Preset::Weekly, "18:15", 0),
        ] {
            assert_eq!(
                cron_preset(cron),
                Some((preset, time.to_string(), weekday)),
                "{cron}"
            );
            assert_eq!(preset_cron(preset, time, weekday).as_deref(), Some(cron));
        }
        for custom in [
            "*/15 * * * *",
            "0 9 1 * *",
            "0 9 * * 1,3",
            "0 24 * * *",
            "0 9 * * 7",
        ] {
            assert_eq!(cron_preset(custom), None, "{custom}");
        }
    }

    #[test]
    fn preset_time_must_be_a_24_hour_clock() {
        assert_eq!(
            preset_cron(Preset::Daily, " 9:05 ", 1).as_deref(),
            Some("5 9 * * *")
        );
        for bad in ["", "9", "24:00", "09:60", "9:5:0", "a:00", "-1:00"] {
            assert_eq!(preset_cron(Preset::Daily, bad, 1), None, "{bad:?}");
        }
        assert_eq!(preset_cron(Preset::Custom, "09:00", 1), None);
        assert_eq!(preset_cron(Preset::Cron, "09:00", 1), None);
        assert_eq!(
            preset_cron(Preset::Hourly, "", 1).as_deref(),
            Some("0 * * * *")
        );
    }

    #[test]
    fn form_problems_drop_the_transport_prefix() {
        let bad = holt_rpc::RpcError::BadParams("invalid cron: expected 5 fields".into());
        assert_eq!(rpc_problem(&bad), "Invalid cron: expected 5 fields");
    }

    #[test]
    fn short_model_drops_the_vendor_prefix() {
        assert_eq!(short_model("openai/gpt-5.4"), "gpt-5.4");
        assert_eq!(short_model("kimi-k3"), "kimi-k3");
    }
}
