use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use gpui::{
    AnyElement, Context, Entity, EventEmitter, IntoElement, Render, SharedString, Subscription,
    Task, Window, div, prelude::*, px,
};
use holt_doc::{MessagePart, SessionMessageEntry};
use holt_proto::{Model, Provider, ProviderId, ToolCall};
use holt_rpc::methods;
use serde::Deserialize;

use crate::{
    composer::ComposerInput,
    motion::{self, AnimationExt as _},
    popover::{self, Loadable, Popup},
    settings::widgets,
    state::AppState,
    theme::Theme,
};

/// Surfaced to the shell, which renders it as a window-top modal — action
/// failures (save/remove key, RPC errors) never paint inside the page.
#[derive(Debug, Clone)]
pub enum ProvidersPageEvent {
    Error(SharedString),
}

impl EventEmitter<ProvidersPageEvent> for ProvidersPage {}

/// One hidden-model row as `ListHiddenModels` reports it.
#[derive(Debug, Clone, Deserialize)]
struct HiddenModel {
    id: String,
    label: Option<String>,
}

/// The custom-provider creation form's fields: (key, placeholder).
const NEW_PROVIDER_FIELDS: [(&str, &str); 4] = [
    ("id", "Provider id (e.g. acme)"),
    ("name", "Display name"),
    ("baseUrl", "https://acme.example/v1"),
    ("defaultApi", "openai-completions"),
];

/// The model-record form's single-line fields, in render order:
/// (key, label, placeholder). Laid out two per row. No `baseUrl` field —
/// a record added under a provider rides that provider's endpoint; the
/// engine fills the default, and Advanced JSON is the override hatch.
/// `api` is not a text field: the dialect set is closed (the engine's
/// registered list), so it rides a dropdown.
const RECORD_FIELDS: [(&str, &str, &str); 8] = [
    ("id", "Model ID", "acme-1"),
    ("name", "Name", "Acme 1"),
    ("contextWindow", "Context window", "200000"),
    ("maxTokens", "Max tokens", "8192"),
    ("inputCost", "Input cost /M", "0.0"),
    ("outputCost", "Output cost /M", "0.0"),
    ("cacheReadCost", "Cache read /M", "0.0"),
    ("cacheWriteCost", "Cache write /M", "0.0"),
];

/// The Add Provider dialog's tabs (design-v2): the manual form, or the AI
/// setup chat that arrives with V2c.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddProviderTab {
    Manual,
    Ai,
}

/// The mounted model-record form. One panel is expanded at a time, so one
/// instance serves the whole page; it targets that panel's active variant.
struct RecordForm {
    provider: String,
    inputs: HashMap<String, Entity<ComposerInput>>,
    /// The picked API dialect (the dropdown's selection; the set comes
    /// from the engine's `ListApiDialects`).
    api: String,
    reasoning: bool,
    image: bool,
    error: Option<String>,
}

pub struct ProvidersPage {
    state: Entity<AppState>,
    providers: Loadable<Vec<Provider>>,
    expanded: Option<String>,
    collapsing: Option<String>,
    panel_epochs: HashMap<String, u64>,
    /// Per-organization selected variant (concrete provider id); falls back
    /// to the first variant when unset.
    selected_variant: HashMap<String, String>,
    inputs: HashMap<String, Entity<ComposerInput>>,
    models: HashMap<String, Loadable<Vec<Model>>>,
    model_tasks: HashMap<String, Task<()>>,
    /// Variants whose API-key input is currently unmasked; everything starts
    /// masked on every expansion and re-masks when the panel collapses or the
    /// page is left.
    revealed: HashSet<String>,
    /// The Add Provider dialog's open tab (design-v2); `None` = closed.
    add_dialog: Option<AddProviderTab>,
    new_provider_inputs: HashMap<&'static str, Entity<ComposerInput>>,
    new_provider_error: Option<String>,
    /// The mounted model-record form, targeting the expanded panel's active
    /// variant (ADR-0029's manual half of the write path).
    record_form: Option<RecordForm>,
    /// The record form's dialect dropdown; page-level because the form is
    /// an Option the popup accessor can't borrow through.
    record_api_menu: Popup<()>,
    /// The engine's registered API dialects (the dropdown's options),
    /// loaded once per page.
    api_dialects: Loadable<Vec<String>>,
    /// Hidden rows per variant, loaded beside the model list.
    hidden: HashMap<String, Loadable<Vec<HiddenModel>>>,
    /// Variants whose Hidden block is expanded; it starts collapsed on
    /// every panel expansion (like the masked key).
    hidden_expanded: HashSet<String>,
    /// Two-step per-provider reset confirmation: the first click arms, the
    /// second executes. The GLOBAL reset rides a confirm dialog instead
    /// (`confirm_reset_all`) — the armed-button copy never fit the row.
    armed_reset: Option<String>,
    /// The same two-step for removing a custom provider's definition —
    /// the armed copy states what stays behind (the key, the records).
    armed_remove: Option<String>,
    /// The global reset's confirm dialog is open.
    confirm_reset_all: bool,
    /// The AI tab (V2c): the session's setup chat id (a fresh chat per
    /// dialog open, deleted on close — no conversation memory persists),
    /// the real Transcript view pinned to it (fed by AppState's
    /// doc watch), the picker's catalog and popup, the composer input, and
    /// the review panel's proposals. The panel refreshes when the setup
    /// transcript's proposal-tool signature moves (observed off AppState).
    setup_chat: Option<String>,
    setup_transcript_view: Option<Entity<crate::transcript::Transcript>>,
    /// The session view's last-rendered emptiness — the observe hook's
    /// placeholder ↔ transcript flip detector.
    setup_doc_empty: bool,
    setup_proposal_signature: (usize, usize),
    setup_models: Loadable<Vec<Model>>,
    setup_model_menu: Popup<()>,
    setup_selected_model: Option<String>,
    /// The setup picker menu's provider rail selection. One provider's
    /// models show at a time: a single configured aggregator (openrouter)
    /// contributes hundreds of rows, so a flat catalog list is unusable.
    setup_model_provider: Option<ProviderId>,
    setup_input: Option<Entity<ComposerInput>>,
    setup_input_events: Option<Subscription>,
    setup_state_observe: Option<Subscription>,
    setup_proposals: Vec<serde_json::Value>,
    /// Proposals written this session, kept past the engine's consume so
    /// the panel can render the "written" terminal card instead of
    /// silently vanishing the user's action.
    setup_applied: Vec<serde_json::Value>,
    /// Per-proposal apply failures, rendered inline on the card — an apply
    /// error is a property of this proposal (usually the staleness gate),
    /// not a page-level fault, so it never rides the window-top modal.
    setup_apply_errors: HashMap<String, String>,
    /// The in-flight apply's proposal id; one apply at a time.
    setup_applying: Option<String>,
    /// The setup chat's queue snapshot (`WatchMessageQueue`). The send RPC
    /// returns before admission, so a turn that fails to start (missing
    /// key, unresolvable model, storage fault) never writes a doc entry —
    /// this frame is the only place its reason surfaces.
    setup_queue: Option<holt_proto::MessageQueue>,
    /// The setup tab's async work is slot-separated from the page's `task`:
    /// every page action assigns `task`, and dropping a `Task` cancels it,
    /// so sharing the slot let a panel refresh abort an in-flight send
    /// (its prompt was already cleared — a silent message loss) or the
    /// tab's own preparation. Preparation owns a slot; the queue watch
    /// owns one (cancelled on dialog close); the send and the proposal
    /// apply are `detach`ed instead — cancelling either loses the user's
    /// action silently, so they must run to completion.
    setup_task: Option<Task<()>>,
    setup_panel_task: Option<Task<()>>,
    setup_queue_task: Option<Task<()>>,
    task: Option<Task<()>>,
    collapse_task: Option<Task<()>>,
}

impl ProvidersPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let setup_observe = cx.observe(&state, |page: &mut Self, state, cx| {
            page.on_setup_state_changed(state, cx);
        });
        let mut page = Self {
            state,
            providers: Loadable::Idle,
            expanded: None,
            collapsing: None,
            panel_epochs: HashMap::new(),
            selected_variant: HashMap::new(),
            inputs: HashMap::new(),
            models: HashMap::new(),
            model_tasks: HashMap::new(),
            revealed: HashSet::new(),
            add_dialog: None,
            new_provider_inputs: HashMap::new(),
            new_provider_error: None,
            record_form: None,
            record_api_menu: Popup::default(),
            api_dialects: Loadable::Idle,
            hidden: HashMap::new(),
            hidden_expanded: HashSet::new(),
            armed_reset: None,
            armed_remove: None,
            confirm_reset_all: false,
            setup_chat: None,
            setup_transcript_view: None,
            setup_doc_empty: true,
            setup_proposal_signature: (0, 0),
            setup_models: Loadable::Idle,
            setup_model_menu: Popup::default(),
            setup_selected_model: None,
            setup_model_provider: None,
            setup_input: None,
            setup_input_events: None,
            setup_state_observe: None,
            setup_proposals: Vec::new(),
            setup_applied: Vec::new(),
            setup_apply_errors: HashMap::new(),
            setup_applying: None,
            setup_queue: None,
            setup_task: None,
            setup_panel_task: None,
            setup_queue_task: None,
            task: None,
            collapse_task: None,
        };
        // The setup chat's transcript lives in AppState's sub_transcripts;
        // its proposal-tool signature moving is the review panel's refresh
        // signal.
        page.setup_state_observe = Some(setup_observe);
        page.load(cx);
        page
    }

    /// AppState changed: the page re-renders when the session view's
    /// emptiness flips (the placeholder ↔ transcript mount decision lives
    /// here — a mounted Transcript re-renders itself), and the review panel
    /// re-reads when the proposal-tool signature moves.
    fn on_setup_state_changed(&mut self, state: Entity<AppState>, cx: &mut Context<Self>) {
        let Some(chat_id) = self.setup_chat.clone() else {
            return;
        };
        if self.setup_transcript_view.is_none() {
            return;
        }
        let (empty, signature) = {
            let doc = state.read(cx).sub_transcript(&chat_id);
            (doc.is_empty(), proposal_signature(doc))
        };
        if empty != self.setup_doc_empty {
            self.setup_doc_empty = empty;
            cx.notify();
        }
        if signature != self.setup_proposal_signature {
            self.setup_proposal_signature = signature;
            self.refresh_setup_proposals(cx);
        }
    }

    /// Re-mask every key input and forget which were unmasked — called when
    /// the panel collapses, the variant switches, or the page is left.
    fn conceal_keys(&mut self, cx: &mut Context<Self>) {
        self.revealed.clear();
        for input in self.inputs.values() {
            input.update(cx, |input, cx| input.set_masked(true, cx));
        }
    }

    /// The shell's leave-the-page hook: the page entity outlives the visit.
    pub fn clear_revealed(&mut self, cx: &mut Context<Self>) {
        self.conceal_keys(cx);
        cx.notify();
    }

    /// Surface a failure to the shell's window-top error modal.
    fn fail(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        cx.emit(ProvidersPageEvent::Error(message.into()));
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        // Keep an already-rendered catalog mounted while refreshing it. Save
        // and Remove call this path while a provider panel may be expanded;
        // replacing Ready with Loading would unmount that panel and replay its
        // open animation when the RPC returns.
        mark_provider_loading(&mut self.providers);
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_PROVIDERS, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                page.providers = match result {
                    Ok(value) => serde_json::from_value(value)
                        .map(Loadable::Ready)
                        .unwrap_or_else(|error| Loadable::Error(error.to_string())),
                    Err(error) => Loadable::Error(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// The concrete provider the expanded card acts on: the remembered pick
    /// or the organization's first variant. Every per-provider RPC (save /
    /// reveal / remove key, list models, add model) addresses this id.
    fn active_variant_id(&self, org_id: &str) -> Option<String> {
        if let Some(picked) = self.selected_variant.get(org_id) {
            return Some(picked.clone());
        }
        self.providers
            .ready()?
            .iter()
            .find(|row| row.id.as_str() == org_id)?
            .variants
            .first()
            .map(|variant| variant.id.to_string())
    }

    fn ensure_variant_inputs(&mut self, variant_id: &str, cx: &mut Context<Self>) {
        self.inputs
            .entry(variant_id.to_string())
            .or_insert_with(|| cx.new(|cx| ComposerInput::new_secret("API key", cx)));
    }

    /// Fetch the stored key and populate the variant's input. A return visit
    /// recreates the page with empty inputs, so without this the saved key is
    /// invisible. Only fills an untouched input — a fetch that lands after the
    /// user started typing (or already holds a draft) must not clobber it.
    fn load_key(&mut self, variant_id: &str, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let variant = variant_id.to_string();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::REVEAL_PROVIDER_KEY,
                    serde_json::json!({"providerId": variant.clone()}),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => {
                        let key = value
                            .get("key")
                            .and_then(serde_json::Value::as_str)
                            .filter(|key| !key.is_empty());
                        if let Some(key) = key
                            && let Some(input) = page.inputs.get(&variant)
                            && input.read(cx).text().is_empty()
                        {
                            input.update(cx, |input, cx| input.set_text(key, cx));
                        }
                    }
                    Err(error) => page.fail(error.to_string(), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The eye button: flip one variant's input between bullets and plain
    /// text. Purely a projection change — the content (and what Save writes)
    /// is identical either way.
    fn toggle_mask(&mut self, variant_id: String, cx: &mut Context<Self>) {
        let unmask = !self.revealed.contains(&variant_id);
        if unmask {
            self.revealed.insert(variant_id.clone());
        } else {
            self.revealed.remove(&variant_id);
        }
        if let Some(input) = self.inputs.get(&variant_id) {
            input.update(cx, |input, cx| input.set_masked(!unmask, cx));
        }
        cx.notify();
    }

    fn toggle(&mut self, org_id: &str, cx: &mut Context<Self>) {
        if self.expanded.as_deref() == Some(org_id) {
            self.expanded = None;
            self.conceal_keys(cx);
            self.close_panel_forms();
            self.begin_collapse(org_id.to_string(), cx);
        } else {
            if let Some(previous) = self.expanded.take() {
                self.close_panel_forms();
                self.begin_collapse(previous, cx);
            }
            if self.collapsing.as_deref() == Some(org_id) {
                self.collapsing = None;
            }
            self.expanded = Some(org_id.to_string());
            self.conceal_keys(cx);
            self.close_panel_forms();
            *self.panel_epochs.entry(org_id.to_string()).or_default() += 1;
            if let Some(variant_id) = self.active_variant_id(org_id) {
                self.ensure_variant_inputs(&variant_id, cx);
                self.load_key(&variant_id, cx);
                self.load_models(&variant_id, false, cx);
                self.load_hidden(&variant_id, false, cx);
            }
        }
        cx.notify();
    }

    fn switch_variant(&mut self, org_id: String, variant_id: String, cx: &mut Context<Self>) {
        if self.selected_variant.get(&org_id).map(String::as_str) == Some(variant_id.as_str()) {
            return;
        }
        self.selected_variant.insert(org_id, variant_id.clone());
        self.conceal_keys(cx);
        self.close_panel_forms();
        self.ensure_variant_inputs(&variant_id, cx);
        self.load_key(&variant_id, cx);
        self.load_models(&variant_id, false, cx);
        self.load_hidden(&variant_id, false, cx);
        cx.notify();
    }

    /// Retire the per-panel transients: the record form and any armed
    /// reset belong to the variant the panel was showing.
    fn close_panel_forms(&mut self) {
        self.record_form = None;
        self.armed_reset = None;
        self.armed_remove = None;
        self.hidden_expanded.clear();
    }

    /// The Hidden block's collapse state flips per variant.
    fn toggle_hidden(&mut self, variant_id: String, cx: &mut Context<Self>) {
        if !self.hidden_expanded.insert(variant_id.clone()) {
            self.hidden_expanded.remove(&variant_id);
        }
        cx.notify();
    }

    fn begin_collapse(&mut self, provider: String, cx: &mut Context<Self>) {
        self.collapsing = Some(provider.clone());
        *self.panel_epochs.entry(provider.clone()).or_default() += 1;
        let delay = if cx.reduce_motion() {
            std::time::Duration::ZERO
        } else {
            motion::PROVIDER_COLLAPSE
                .total()
                .mul_f32(motion::speed_scale())
        };
        self.collapse_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |page, cx| {
                if page.collapsing.as_deref() == Some(provider.as_str()) {
                    page.collapsing = None;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn load_models(&mut self, provider: &str, force: bool, cx: &mut Context<Self>) {
        if !force && matches!(self.models.get(provider), Some(Loadable::Ready(_))) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let provider = provider.to_string();
        if !matches!(self.models.get(&provider), Some(Loadable::Ready(_))) {
            self.models.insert(provider.clone(), Loadable::Loading);
        }
        let request_provider = provider.clone();
        let slot_provider = provider.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::LIST_MODELS,
                    serde_json::json!({"providerId": request_provider}),
                )
                .await;
            this.update(cx, |page, cx| {
                let models = match result {
                    Ok(value) => serde_json::from_value(value)
                        .map(Loadable::Ready)
                        .unwrap_or_else(|error| Loadable::Error(error.to_string())),
                    Err(error) => Loadable::Error(error.to_string()),
                };
                page.models.insert(slot_provider, models);
                cx.notify();
            })
            .ok();
        });
        self.model_tasks.insert(provider, task);
    }

    fn remove_model(&mut self, provider: String, model: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::REMOVE_MODEL_RECORD,
                    serde_json::json!({"providerId": provider, "modelId": model}),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        crate::pickers::bump_provider_catalog(cx);
                        page.load_models(&provider, true, cx);
                    }
                    Err(error) => page.fail(error.to_string(), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn load_hidden(&mut self, provider: &str, force: bool, cx: &mut Context<Self>) {
        if !force && matches!(self.hidden.get(provider), Some(Loadable::Ready(_))) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let provider = provider.to_string();
        self.hidden.insert(provider.clone(), Loadable::Loading);
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::LIST_HIDDEN_MODELS,
                    serde_json::json!({ "providerId": provider.clone() }),
                )
                .await;
            this.update(cx, |page, cx| {
                let rows = match result {
                    Ok(value) => serde_json::from_value(value)
                        .map(Loadable::Ready)
                        .unwrap_or_else(|error| Loadable::Error(error.to_string())),
                    Err(error) => Loadable::Error(error.to_string()),
                };
                page.hidden.insert(provider, rows);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Hides one catalog row: the engine takes the provider's whole hidden
    /// set, so the new set is the loaded one plus the id.
    fn hide_model(&mut self, provider: String, model_id: String, cx: &mut Context<Self>) {
        let mut ids: Vec<String> = self
            .hidden
            .get(&provider)
            .and_then(Loadable::ready)
            .map(|rows| rows.iter().map(|row| row.id.clone()).collect())
            .unwrap_or_default();
        if !ids.contains(&model_id) {
            ids.push(model_id);
        }
        self.set_hidden(provider, ids, cx);
    }

    fn unhide_model(&mut self, provider: String, model_id: String, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .hidden
            .get(&provider)
            .and_then(Loadable::ready)
            .map(|rows| {
                rows.iter()
                    .map(|row| row.id.clone())
                    .filter(|id| id != &model_id)
                    .collect()
            })
            .unwrap_or_default();
        self.set_hidden(provider, ids, cx);
    }

    fn set_hidden(&mut self, provider: String, model_ids: Vec<String>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SET_HIDDEN_MODELS,
                    serde_json::json!({
                        "providerId": provider.clone(),
                        "modelIds": model_ids,
                    }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        crate::pickers::bump_provider_catalog(cx);
                        page.load_models(&provider, true, cx);
                        page.load_hidden(&provider, true, cx);
                    }
                    Err(error) => page.fail(error.to_string(), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn open_record_form(&mut self, provider: String, cx: &mut Context<Self>) {
        let mut inputs = HashMap::new();
        for (key, _, placeholder) in RECORD_FIELDS {
            inputs.insert(
                key.to_string(),
                cx.new(|cx| ComposerInput::new(placeholder, cx)),
            );
        }
        inputs.insert(
            "advanced".to_string(),
            cx.new(|cx| {
                ComposerInput::new(
                    "{ \"thinkingLevelMap\": …, \"compat\": … } — optional JSON",
                    cx,
                )
            }),
        );
        // A stale menu from a previous form must not carry over.
        self.record_api_menu = Popup::default();
        self.load_api_dialects(cx);
        self.record_form = Some(RecordForm {
            provider,
            inputs,
            api: "openai-completions".into(),
            reasoning: false,
            image: false,
            error: None,
        });
        cx.notify();
    }

    /// The dialect dropdown's options: the engine's registered api
    /// dialects (pi-core's registry), loaded once per page. Detached — a
    /// cancelled load would wedge the list in Loading forever.
    fn load_api_dialects(&mut self, cx: &mut Context<Self>) {
        if matches!(self.api_dialects, Loadable::Ready(_) | Loadable::Loading) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.api_dialects = Loadable::Loading;
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_API_DIALECTS, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                page.api_dialects = match result {
                    Ok(value) => serde_json::from_value(value)
                        .map(Loadable::Ready)
                        .unwrap_or_else(|error| Loadable::Error(error.to_string())),
                    Err(error) => Loadable::Error(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_record_api_menu(&mut self, cx: &mut Context<Self>) {
        if self.record_api_menu.take_press_was_open() || self.record_api_menu.is_open() {
            self.close_record_api_menu(cx);
        } else {
            self.record_api_menu.open(());
        }
        cx.notify();
    }

    fn close_record_api_menu(&mut self, cx: &mut Context<Self>) {
        if self.record_api_menu.begin_close() {
            popover::reap_popup(cx, |page: &mut ProvidersPage| &mut page.record_api_menu);
            cx.notify();
        }
    }

    fn pick_record_api(&mut self, dialect: String, cx: &mut Context<Self>) {
        if let Some(form) = self.record_form.as_mut() {
            form.api = dialect;
        }
        self.close_record_api_menu(cx);
        cx.notify();
    }

    fn toggle_record_flag(&mut self, flag: &str, cx: &mut Context<Self>) {
        let Some(form) = self.record_form.as_mut() else {
            return;
        };
        if flag == "reasoning" {
            form.reasoning = !form.reasoning;
        } else if flag == "image" {
            form.image = !form.image;
        }
        cx.notify();
    }

    fn save_record(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(form) = self.record_form.as_ref() else {
            return;
        };
        let mut texts = HashMap::new();
        for (key, input) in &form.inputs {
            texts.insert(key.clone(), input.read(cx).text().trim().to_string());
        }
        let provider = form.provider.clone();
        texts.insert("api".to_string(), form.api.clone());
        let record = match build_record_json(&provider, &texts, form.reasoning, form.image) {
            Ok(record) => record,
            Err(problem) => {
                if let Some(form) = self.record_form.as_mut() {
                    form.error = Some(problem);
                }
                cx.notify();
                return;
            }
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SAVE_MODEL_RECORD,
                    serde_json::json!({
                        "providerId": provider.clone(),
                        "record": record,
                    }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.record_form = None;
                        crate::pickers::bump_provider_catalog(cx);
                        page.load_models(&provider, true, cx);
                        page.load_hidden(&provider, true, cx);
                    }
                    Err(error) => {
                        if let Some(form) = page.record_form.as_mut() {
                            form.error = Some(error.to_string());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn open_add_dialog(&mut self, tab: AddProviderTab, cx: &mut Context<Self>) {
        if tab == AddProviderTab::Ai {
            self.prepare_setup(cx);
        }
        self.add_dialog = Some(tab);
        for (key, placeholder) in NEW_PROVIDER_FIELDS {
            self.new_provider_inputs
                .entry(key)
                .or_insert_with(|| cx.new(|cx| ComposerInput::new(placeholder, cx)));
        }
        cx.notify();
    }

    fn close_add_dialog(&mut self, cx: &mut Context<Self>) {
        self.add_dialog = None;
        self.new_provider_error = None;
        // Dropping the view + the doc watch stops all background work; the
        // next open re-prepares from a fresh subscription. The session's
        // applied/error memories go with it — a reopened dialog lists only
        // what its own session wrote.
        self.setup_transcript_view = None;
        self.setup_doc_empty = true;
        self.setup_proposal_signature = (0, 0);
        self.setup_queue_task = None;
        self.setup_queue = None;
        self.setup_applied.clear();
        self.setup_apply_errors.clear();
        self.setup_applying = None;
        // The setup chat is session-scoped: closing the dialog ends it.
        // The delete cancels any in-flight turn and drops the transcript
        // and its stored proposals — nothing carries into the next open.
        if let Some(chat_id) = self.setup_chat.take() {
            self.state
                .update(cx, |state, _| state.unwatch_subagent_doc(&chat_id));
            if let Some(engine) = self.state.read(cx).engine().cloned() {
                cx.spawn(async move |_, _| {
                    let _ = engine
                        .client()
                        .call(
                            methods::MUTATE,
                            serde_json::json!({ "op": "deleteChat", "chatId": chat_id }),
                        )
                        .await;
                })
                .detach();
            }
        }
        cx.notify();
    }

    // -- The AI tab (V2c) --------------------------------------------------

    /// The default setup model: the selected chat's config, normalized to
    /// the provider-qualified id (older stored configs may hold a bare id —
    /// the engine's `wire_model_id` rule). The async preparation falls back
    /// to the first configured provider when no chat carries a config.
    fn default_setup_model(&self, cx: &Context<Self>) -> Option<(String, String)> {
        let config = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|chat| chat.config.clone())?;
        let provider = config.provider.0.clone();
        let model = format!(
            "{provider}/{}",
            config.model.rsplit('/').next().unwrap_or(&config.model)
        );
        Some((provider, model))
    }

    /// Prepares the AI tab: starts the session's setup chat, loads the
    /// picker catalog, and pins a real Transcript view to the chat (fed by
    /// AppState's doc watch). Idempotent — an already-prepared tab returns.
    fn prepare_setup(&mut self, cx: &mut Context<Self>) {
        if self.setup_input.is_none() {
            let input =
                cx.new(|cx| ComposerInput::new("Which provider or model should be set up?", cx));
            // Enter sends, like every other single-shot field; edits
            // repaint so the send circle dims/lights with the content.
            self.setup_input_events = Some(cx.subscribe(
                &input,
                |page: &mut Self, _, event, cx| match event {
                    crate::composer::ComposerInputEvent::Submitted => page.send_setup_message(cx),
                    crate::composer::ComposerInputEvent::Edited => cx.notify(),
                    _ => {}
                },
            ));
            self.setup_input = Some(input);
        }
        if self.setup_transcript_view.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if self.setup_chat.is_none() {
            self.setup_models = Loadable::Loading;
        }
        let selected_default = self.default_setup_model(cx);
        // The setup chat remembers the last pick (design-v2 decision 1):
        // a carried-over selection outranks re-inheriting the selected
        // chat's config — re-inheriting on every reopen made the user's
        // pick look like it never stuck.
        let remembered = self.setup_selected_model.clone();
        self.setup_task = Some(cx.spawn(async move |this, cx| {
            // Resolve the model: the last pick, else the selected chat's
            // config, else the first configured provider's first model.
            let candidate = remembered.or_else(|| selected_default.map(|(_, model)| model));
            let (provider, model) = match candidate.map(|model| {
                let provider = model.split_once('/').map(|(p, _)| p.to_string());
                (provider, model)
            }) {
                Some((provider, model)) => (provider.unwrap_or_default(), model),
                None => match first_configured_model(&engine).await {
                    Some(pair) => pair,
                    None => {
                        this.update(cx, |page, cx| {
                            page.setup_models = Loadable::Error(
                                "Configure a provider API key first — the AI tab needs a \
                                 working model."
                                    .into(),
                            );
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                },
            };
            let models = configured_model_catalog(&engine).await;
            // The inherited model can outlive the catalog entry it named —
            // a global reset drops custom models while chat configs keep
            // pointing at them. A model the catalog no longer knows cannot
            // admit a turn, so keep it only when it resolves; otherwise
            // take the catalog's first servable entry.
            let model = if models.iter().any(|row| row.id == model) {
                model
            } else {
                let Some(fallback) = models.first().map(|row| row.id.clone()) else {
                    this.update(cx, |page, cx| {
                        page.setup_models = Loadable::Error(
                            "Configure a provider API key first — the AI tab needs a \
                             working model."
                                .into(),
                        );
                        cx.notify();
                    })
                    .ok();
                    return;
                };
                fallback
            };
            let provider = model
                .split_once('/')
                .map(|(prefix, _)| prefix.to_string())
                .unwrap_or(provider);
            let chat_id = match engine
                .client()
                .call(
                    methods::START_MODEL_SETUP_CHAT,
                    serde_json::json!({ "provider": provider, "model": model }),
                )
                .await
            {
                Ok(value) => value
                    .get("chatId")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                Err(_) => None,
            };
            let Some(chat_id) = chat_id else {
                this.update(cx, |page, cx| {
                    page.setup_models = Loadable::Error("Could not create the setup chat".into());
                    cx.notify();
                })
                .ok();
                return;
            };
            let proposals = setup_proposal_rows(&engine, &chat_id).await;
            this.update(cx, |page, cx| {
                // The dialog closed while preparation was in flight: the
                // fresh chat has no owner — delete it so it never becomes
                // a leftover, and set nothing the next open would inherit.
                if page.add_dialog.is_none() {
                    let engine = page.state.read(cx).engine().cloned();
                    if let Some(engine) = engine {
                        let chat_id = chat_id.clone();
                        cx.spawn(async move |_, _| {
                            let _ = engine
                                .client()
                                .call(
                                    methods::MUTATE,
                                    serde_json::json!({ "op": "deleteChat", "chatId": chat_id }),
                                )
                                .await;
                        })
                        .detach();
                    }
                    return;
                }
                page.setup_chat = Some(chat_id.clone());
                // The qualified id is both the display value and the run
                // identity — no re-prefixing.
                page.setup_selected_model = Some(model.clone());
                page.setup_model_provider = model
                    .split_once('/')
                    .map(|(provider, _)| ProviderId(provider.into()));
                page.setup_models = Loadable::Ready(models);
                page.setup_proposals = proposals;
                // The chat surface is the real Transcript (full markdown,
                // thinking, tool chips, the working trailer). The chat is
                // fresh per session, so its doc holds only this session —
                // no filtering.
                page.state.update(cx, |state, cx| {
                    state.watch_subagent_doc(chat_id.clone(), cx);
                });
                let state = page.state.clone();
                page.setup_transcript_view =
                    Some(cx.new(|cx| {
                        crate::transcript::Transcript::for_doc(state, chat_id, true, cx)
                    }));
                page.watch_setup_queue(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// The setup chat's queue watch: the send RPC returns before admission,
    /// so a turn the engine cannot start never reaches the doc — the queue
    /// frame carries the failure instead. Resubscribes like every other
    /// watch; dropped (cancelled) when the dialog closes.
    fn watch_setup_queue(&mut self, cx: &mut Context<Self>) {
        if self.setup_queue_task.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(chat_id) = self.setup_chat.clone() else {
            return;
        };
        self.setup_queue_task = Some(cx.spawn(async move |this, cx| {
            loop {
                if let Ok(mut rx) = engine
                    .client()
                    .subscribe(
                        methods::WATCH_MESSAGE_QUEUE,
                        serde_json::json!({ "chatId": chat_id }),
                    )
                    .await
                {
                    while let Some(value) = rx.recv().await {
                        let Ok(queue) = crate::watch_coordinator::WatchCoordinator::decode::<
                            holt_proto::MessageQueue,
                        >(value) else {
                            break;
                        };
                        if this
                            .update(cx, |page, cx| {
                                page.setup_queue = Some(queue);
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                if this
                    .update(cx, |page, cx| {
                        page.setup_queue = None;
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
                cx.background_executor()
                    .timer(crate::watch_coordinator::WatchCoordinator::RETRY_DELAY)
                    .await;
            }
        }));
    }

    fn pick_setup_model(&mut self, qualified: String, cx: &mut Context<Self>) {
        self.close_setup_model_menu(cx);
        let Some((provider, _)) = qualified.split_once('/') else {
            return;
        };
        let provider = provider.to_string();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.setup_selected_model = Some(qualified.clone());
        self.setup_model_provider = qualified
            .split_once('/')
            .map(|(provider, _)| ProviderId(provider.into()));
        // The chat is session-scoped, so a mid-session pick rewrites the
        // current chat's config (scope preserved) — it must NOT start a
        // fresh chat, or the running conversation would be orphaned.
        let Some(chat_id) = self.setup_chat.clone() else {
            return;
        };
        self.setup_panel_task = Some(cx.spawn(async move |this, cx| {
            let _ = engine
                .client()
                .call(
                    methods::MUTATE,
                    serde_json::json!({
                        "op": "setChatConfig",
                        "chatId": chat_id,
                        "config": {
                            "provider": provider,
                            "model": qualified,
                            "reasoning": null,
                            "modelOptions": {},
                            "scope": "model-setup",
                        },
                    }),
                )
                .await;
            let _ = this.update(cx, |_, cx| cx.notify());
        }));
        cx.notify();
    }

    fn close_setup_model_menu(&mut self, cx: &mut Context<Self>) {
        if self.setup_model_menu.begin_close() {
            popover::reap_popup(cx, |page| &mut page.setup_model_menu);
            cx.notify();
        }
    }

    fn toggle_setup_model_menu(&mut self, cx: &mut Context<Self>) {
        if self.setup_model_menu.take_press_was_open() || self.setup_model_menu.is_open() {
            self.close_setup_model_menu(cx);
        } else {
            self.setup_model_menu.open(());
            cx.notify();
        }
    }

    /// Sends one message into the setup chat; the engine's queue serializes
    /// turns, so a send while the assistant works simply lines up.
    fn send_setup_message(&mut self, cx: &mut Context<Self>) {
        let (Some(chat_id), Some(input)) = (self.setup_chat.clone(), self.setup_input.clone())
        else {
            return;
        };
        let prompt = input.read(cx).text().trim().to_string();
        if prompt.is_empty() {
            return;
        }
        let Some(selected) = self.setup_selected_model.clone() else {
            return;
        };
        // `RunRequest.model` is the provider-qualified id (the composer's
        // convention); the provider half alone addresses the credential.
        let Some((provider, _)) = selected.split_once('/') else {
            return;
        };
        let provider = provider.to_string();
        let cwd = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|chat| chat.cwd.clone())
            .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/".to_string()));
        input.update(cx, |input, cx| input.set_text("", cx));
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let message_id = format!(
            "setup-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_millis())
                .unwrap_or(0)
        );
        // The queue snapshot may hold a previous turn's admission failure;
        // a fresh attended send re-admits past the pause, so drop the stale
        // reason — if this send fails too, the next frame brings it back.
        // An errored head parks forever (the pause protects it) — the retry
        // the strip promises must delete those items BEFORE re-enqueueing,
        // or the queue stays blocked and the failure strip never leaves.
        // Read the snapshot first: the clearing below must not erase the
        // very list the deletes are derived from.
        let errored: Vec<String> = self
            .setup_queue
            .as_ref()
            .map(|queue| {
                queue
                    .pending
                    .iter()
                    .filter(|item| item.error.is_some())
                    .map(|item| item.message_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.setup_queue = None;
        // Detached, not slotted: a second send (or any panel action) must
        // not cancel this one mid-flight — the prompt is already cleared,
        // so a cancelled send is a silently lost message.
        cx.spawn(async move |this, cx| {
            for message_id in errored {
                let _ = engine
                    .client()
                    .call(
                        methods::DELETE_QUEUED_MESSAGE,
                        serde_json::json!({ "chatId": chat_id, "messageId": message_id }),
                    )
                    .await;
            }
            let result = engine
                .client()
                .call(
                    methods::QUEUE_COMMAND,
                    serde_json::json!({
                        "chatId": chat_id,
                        "command": {
                            "kind": "run",
                            "messageId": message_id,
                            "request": {
                                "prompt": prompt,
                                "provider": provider,
                                "model": selected,
                                "reasoning": null,
                                "modelOptions": {},
                                "cwd": cwd,
                                "sandbox": "workspace-write",
                            },
                        },
                    }),
                )
                .await;
            if let Err(error) = result {
                this.update(cx, |page, cx| page.fail(error.to_string(), cx))
                    .ok();
            }
        })
        .detach();
        cx.notify();
    }

    /// The review panel's write button: applies a stored proposal, then
    /// refreshes everything the page shows. Success remembers the card so
    /// the panel renders a "written" terminal state; failure lands inline
    /// on the card — the usual cause is the apply-time staleness gate, a
    /// property of this proposal, not a page-level fault.
    fn apply_setup_proposal(&mut self, proposal_id: String, cx: &mut Context<Self>) {
        let Some(chat_id) = self.setup_chat.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if self.setup_applying.is_some() {
            return;
        }
        // The card's data: the engine consumes the proposal on success, so
        // the panel keeps its own copy for the terminal render.
        let written = self
            .setup_proposals
            .iter()
            .find(|proposal| proposal["id"] == proposal_id.as_str())
            .cloned();
        self.setup_applying = Some(proposal_id.clone());
        self.setup_apply_errors.remove(&proposal_id);
        cx.notify();
        // Detached: a proposal-signature refresh (or any panel action) must
        // not cancel a Write in flight — the user's click would be lost
        // silently.
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::APPLY_MODEL_PROPOSAL,
                    serde_json::json!({ "chatId": chat_id, "proposalId": proposal_id.clone() }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.setup_applying = None;
                match result {
                    Ok(_) => {
                        if let Some(proposal) = written {
                            page.setup_applied.push(proposal);
                        }
                        page.setup_apply_errors.remove(&proposal_id);
                        crate::pickers::bump_provider_catalog(cx);
                        page.load(cx);
                        page.refresh_setup_proposals(cx);
                        // The write lands outside the panel's own actions, so
                        // the cached models/hidden rows are stale — refresh
                        // every visited variant (the cache only holds those).
                        let refresh: Vec<String> = page.models.keys().cloned().collect();
                        for provider in refresh {
                            page.load_models(&provider, true, cx);
                            page.load_hidden(&provider, true, cx);
                        }
                    }
                    Err(error) => {
                        page.setup_apply_errors
                            .insert(proposal_id, error.to_string());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn discard_setup_proposal(&mut self, proposal_id: String, cx: &mut Context<Self>) {
        let Some(chat_id) = self.setup_chat.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.setup_panel_task = Some(cx.spawn(async move |this, cx| {
            let _ = engine
                .client()
                .call(
                    methods::DISCARD_MODEL_PROPOSAL,
                    serde_json::json!({ "chatId": chat_id, "proposalId": proposal_id }),
                )
                .await;
            this.update(cx, |page, cx| page.refresh_setup_proposals(cx))
                .ok();
        }));
    }

    fn refresh_setup_proposals(&mut self, cx: &mut Context<Self>) {
        let Some(chat_id) = self.setup_chat.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.setup_panel_task = Some(cx.spawn(async move |this, cx| {
            let proposals = setup_proposal_rows(&engine, &chat_id).await;
            this.update(cx, |page, cx| {
                page.setup_proposals = proposals;
                cx.notify();
            })
            .ok();
        }));
    }

    fn save_custom_provider(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let field = |page: &Self, key: &str, cx: &Context<Self>| {
            page.new_provider_inputs
                .get(key)
                .map(|input| input.read(cx).text().trim().to_string())
                .unwrap_or_default()
        };
        let id = field(self, "id", cx);
        let name = {
            let name = field(self, "name", cx);
            if name.is_empty() { id.clone() } else { name }
        };
        let base_url = field(self, "baseUrl", cx);
        let default_api = field(self, "defaultApi", cx);
        if let Some(problem) = new_provider_problem(&id, &base_url, &default_api) {
            self.new_provider_error = Some(problem);
            cx.notify();
            return;
        }
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SAVE_CUSTOM_PROVIDER,
                    serde_json::json!({
                        "id": id,
                        "name": name,
                        "baseUrl": base_url,
                        "defaultApi": default_api,
                    }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.close_add_dialog(cx);
                        for input in page.new_provider_inputs.values() {
                            input.update(cx, |input, cx| input.set_text("", cx));
                        }
                        crate::pickers::bump_provider_catalog(cx);
                        page.load(cx);
                    }
                    Err(error) => page.new_provider_error = Some(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn remove_custom_provider(&mut self, provider: String, org_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::REMOVE_CUSTOM_PROVIDER,
                    serde_json::json!({ "providerId": provider }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.close_panel_forms();
                        page.expanded = None;
                        page.begin_collapse(org_id, cx);
                        crate::pickers::bump_provider_catalog(cx);
                        page.load(cx);
                    }
                    Err(error) => page.fail(error.to_string(), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Two-step per-provider reset: first click arms, second executes.
    fn arm_or_reset(&mut self, provider: String, cx: &mut Context<Self>) {
        if self.armed_reset.as_deref() == Some(provider.as_str()) {
            self.armed_reset = None;
            self.reset_provider(provider, cx);
        } else {
            self.armed_reset = Some(provider);
            cx.notify();
        }
    }

    fn arm_or_remove(&mut self, provider: String, org_id: String, cx: &mut Context<Self>) {
        if self.armed_remove.as_deref() == Some(provider.as_str()) {
            self.armed_remove = None;
            self.remove_custom_provider(provider, org_id, cx);
        } else {
            self.armed_remove = Some(provider);
            cx.notify();
        }
    }

    fn reset_provider(&mut self, provider: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::RESET_PROVIDER_CATALOG,
                    serde_json::json!({ "providerId": provider }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.close_panel_forms();
                        crate::pickers::bump_provider_catalog(cx);
                        page.load(cx);
                        page.load_models(&provider, true, cx);
                        page.load_hidden(&provider, true, cx);
                    }
                    Err(error) => page.fail(error.to_string(), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn reset_all(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::RESET_PROVIDER_CATALOG, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.close_panel_forms();
                        crate::pickers::bump_provider_catalog(cx);
                        page.load(cx);
                        let refresh: Vec<String> = page.models.keys().cloned().collect();
                        for provider in refresh {
                            page.load_models(&provider, true, cx);
                            page.load_hidden(&provider, true, cx);
                        }
                    }
                    Err(error) => page.fail(error.to_string(), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn save(&mut self, provider: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let key = self
            .inputs
            .get(&provider)
            .map(|input| input.read(cx).text().to_string())
            .unwrap_or_default();
        if key.trim().is_empty() {
            self.fail("API key is required", cx);
            return;
        }
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SAVE_PROVIDER_KEY,
                    serde_json::json!({"providerId": provider, "key": key}),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        crate::pickers::bump_provider_catalog(cx);
                        page.load(cx);
                    }
                    Err(error) => page.fail(error.to_string(), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn remove(&mut self, provider: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::REMOVE_PROVIDER_KEY,
                    serde_json::json!({"providerId": provider}),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.conceal_keys(cx);
                        if let Some(input) = page.inputs.get(&provider) {
                            input.update(cx, |input, cx| input.set_text("", cx));
                        }
                        crate::pickers::bump_provider_catalog(cx);
                        page.load(cx);
                    }
                    Err(error) => page.fail(error.to_string(), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }
}

impl Render for ProvidersPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let rows = self
            .providers
            .ready()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(index, provider)| {
                let id = provider.id.to_string();
                let brand_mark = match crate::pickers::provider_brand_icon(&provider.id) {
                    Some((path, tint)) => crate::icons::icon(path)
                        .size(px(22.))
                        .text_color(tint.unwrap_or(theme.text))
                        .into_any_element(),
                    None => div()
                        .text_center()
                        .child(SharedString::from(provider.abbreviation.clone()))
                        .into_any_element(),
                };
                let expanded = self.expanded.as_deref() == Some(id.as_str());
                let collapsing = self.collapsing.as_deref() == Some(id.as_str());
                let panel_mounted = expanded || collapsing;
                let status = if provider.configured {
                    "Configured"
                } else {
                    "Not configured"
                };
                let controls: Option<AnyElement> = panel_mounted.then(|| {
                    let variant_id = self.active_variant_id(&id).unwrap_or_else(|| id.clone());
                    let input = self.inputs.get(&variant_id).cloned();
                    let models = self
                        .models
                        .get(&variant_id)
                        .cloned()
                        .unwrap_or(Loadable::Idle);
                    let hidden = self
                        .hidden
                        .get(&variant_id)
                        .cloned()
                        .unwrap_or(Loadable::Idle);
                    let hidden_count = hidden.ready().map(|rows| rows.len()).unwrap_or(0);
                    let hidden_expanded = self.hidden_expanded.contains(&variant_id);
                    let revealed = self.revealed.contains(&variant_id);
                    let panel_height = provider_controls_height(
                        &models,
                        hidden_count,
                        hidden_expanded,
                        provider.variants.len() > 1,
                    );
                    let panel_epoch = self.panel_epochs.get(&id).copied().unwrap_or_default();
                    let save_id = variant_id.clone();
                    let remove_id = variant_id.clone();
                    let model_count = models.ready().map(|models| models.len());
                    let danger = theme.danger;
                    let danger_muted = theme.danger_muted;
                    let model_list =
                        provider_model_list(index, &variant_id, models, &theme, cx.entity(), cx);
                    let hidden_list =
                        hidden_rows(index, &variant_id, hidden, hidden_expanded, &theme, cx);
                    let add_record_id = variant_id.clone();
                    let danger_row = panel_danger_row(
                        index,
                        &variant_id,
                        provider.custom,
                        self.armed_reset.as_deref() == Some(variant_id.as_str()),
                        self.armed_remove.as_deref() == Some(variant_id.as_str()),
                        &theme,
                        cx,
                    );
                    let variant_selector = variant_selector(&provider, &variant_id, &theme, cx);
                    let content = div()
                        .pl(px(56.0))
                        .pr(px(8.0))
                        .pt(px(8.0))
                        .pb(px(16.0))
                        .flex()
                        .flex_col()
                        .gap(px(16.0))
                        .children(variant_selector)
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(8.0))
                                .child(widgets::field_label(&theme, "API key"))
                                .children(input.map(|input| {
                                    secret_field(&theme, input, revealed, index, &variant_id, cx)
                                        .w_full()
                                        .into_any_element()
                                }))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(8.0))
                                        .child(
                                            action_button(&theme)
                                                .id(("save-provider", index))
                                                .hover(|style| style.bg(crate::theme::ink(0.04)))
                                                .on_click(cx.listener(move |page, _, _, cx| {
                                                    page.save(save_id.clone(), cx)
                                                }))
                                                .child("Save"),
                                        )
                                        .child(
                                            widgets::ghost_action(&theme)
                                                .id(("remove-provider", index))
                                                .hover(move |style| {
                                                    style
                                                        .bg(danger.opacity(0.10))
                                                        .text_color(danger_muted)
                                                })
                                                .on_click(cx.listener(move |page, _, _, cx| {
                                                    page.remove(remove_id.clone(), cx)
                                                }))
                                                .child("Remove"),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(8.0))
                                        .child(widgets::field_label(&theme, "Models"))
                                        .children(model_count.map(|count| {
                                            div()
                                                .text_size(crate::typography::ui_rems(11.0))
                                                .text_color(theme.text_muted.opacity(0.7))
                                                .child(SharedString::from(format!("{count}")))
                                                .into_any_element()
                                        }))
                                        .child(div().flex_1())
                                        .child(
                                            widgets::ghost_action(&theme)
                                                .id(("toggle-record-form", index))
                                                .debug_selector(|| "toggle-record-form".into())
                                                .hover(|style| style.bg(crate::theme::ink(0.04)))
                                                .on_click(cx.listener(move |page, _, _, cx| {
                                                    page.open_record_form(
                                                        add_record_id.clone(),
                                                        cx,
                                                    );
                                                }))
                                                .child(
                                                    crate::icons::icon(crate::icons::PLUS)
                                                        .size(px(12.0))
                                                        .text_color(theme.text_muted),
                                                )
                                                .child("Add model"),
                                        ),
                                )
                                .child(model_list),
                        )
                        .children(hidden_list)
                        .child(danger_row);
                    let panel = div().w_full().overflow_hidden().child(content);
                    if collapsing {
                        panel
                            .with_animation(
                                ("provider-panel-close", panel_epoch),
                                motion::PROVIDER_COLLAPSE.animation(),
                                move |panel, progress| {
                                    panel
                                        .h(px(motion::lerp(panel_height, 0.0, progress)))
                                        .opacity(1.0 - progress)
                                        .relative()
                                        .top(px(-2.0 * progress))
                                },
                            )
                            .into_any_element()
                    } else {
                        panel
                            .with_animation(
                                ("provider-panel-open", panel_epoch),
                                motion::PROVIDER_EXPAND.animation(),
                                move |panel, progress| {
                                    panel
                                        .h(px(motion::lerp(0.0, panel_height, progress)))
                                        .opacity(progress)
                                        .relative()
                                        .top(px(-4.0 * (1.0 - progress)))
                                },
                            )
                            .into_any_element()
                    }
                });
                let expand_id = id.clone();
                let header_children: Vec<AnyElement> = vec![
                    div()
                        .w(px(36.))
                        .flex()
                        .justify_center()
                        .child(brand_mark)
                        .into_any_element(),
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(widgets::row_title(&theme, provider.name))
                        .child(widgets::row_description(&theme, status))
                        .into_any_element(),
                    // The row's affordance: the whole row toggles, so the
                    // chevron carries the expanded/collapsing state.
                    crate::icons::icon(if panel_mounted {
                        crate::icons::ALT_ARROW_DOWN
                    } else {
                        crate::icons::ALT_ARROW_RIGHT
                    })
                    .size(px(13.0))
                    .flex_none()
                    .text_color(theme.text_muted)
                    .into_any_element(),
                ];
                div()
                    .flex()
                    .flex_col()
                    .child(
                        widgets::flat_row()
                            .id(("provider-row", index))
                            .debug_selector(move || format!("provider-row-{index}").into())
                            .cursor_pointer()
                            .px(px(8.0))
                            .rounded(px(8.0))
                            .hover(|style| style.bg(crate::theme::ink(0.04)))
                            .gap(px(12.))
                            .on_click(
                                cx.listener(move |page, _, _, cx| page.toggle(&expand_id, cx)),
                            )
                            .children(header_children),
                    )
                    .children(controls)
                    .into_any_element()
            });
        let body = match &self.providers {
            Loadable::Idle | Loadable::Loading => {
                popover::skeleton_rows("providers-skeleton", &theme, 6, cx.entity_id(), cx)
                    .into_any_element()
            }
            Loadable::Error(error) => {
                widgets::error_strip(&theme, error.clone()).into_any_element()
            }
            Loadable::Ready(_) => {
                // Materialized first: the row builder borrows `cx` lazily,
                // and the action row needs it mutably next.
                let rows: Vec<AnyElement> = rows.collect();
                div()
                    .mt(px(18.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(top_action_row(&theme, cx))
                    .children(rows)
                    .into_any_element()
            }
        };
        let page = div()
            .id("providers-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Providers", None))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Configure API keys for the providers Holt can use. Organizations with \
                         several endpoints are configured per endpoint.",
                    ))
                    .child(body),
            );
        // The page hosts its own deferred-layer modals (the appearance
        // library's review dialog does the same).
        if self.confirm_reset_all {
            let card = reset_all_dialog(&theme, cx);
            return div()
                .child(page)
                .child(popover::modal(
                    "reset-all-providers-dialog",
                    window.viewport_size(),
                    card,
                ))
                .into_any_element();
        }
        if self.add_dialog.is_some() {
            let card = add_provider_dialog(self, &theme, cx);
            return div()
                .child(page)
                .child(popover::modal(
                    "add-provider-dialog",
                    window.viewport_size(),
                    card,
                ))
                .into_any_element();
        }
        if self.record_form.is_some() {
            let card = record_form_dialog(self, &theme, cx);
            return div()
                .child(page)
                .child(popover::modal(
                    "record-form-dialog",
                    window.viewport_size(),
                    card,
                ))
                .into_any_element();
        }
        page.into_any_element()
    }
}

/// The API-key field: the bordered input carrying the stored key, masked to
/// bullets, with the in-field eye toggle that flips the projection.
fn secret_field(
    theme: &Theme,
    input: Entity<ComposerInput>,
    revealed: bool,
    index: usize,
    variant_id: &str,
    cx: &mut Context<ProvidersPage>,
) -> gpui::Div {
    let hover_theme = theme.clone();
    let toggle_id = variant_id.to_string();
    div()
        .h(px(36.0))
        .pl(px(12.0))
        .pr(px(6.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(Theme::CONTROL_RADIUS))
        .border_1()
        .border_color(theme.border)
        .bg(theme.input_glass_bg())
        // The input's root is `w_full`: without a shrinkable track it claims
        // the whole content box and pushes the flex-none eye past the border.
        .child(div().flex_1().min_w_0().child(input))
        .child(
            widgets::ghost_action(theme)
                .flex_none()
                .id(("toggle-key-mask", index))
                .hover(move |style| widgets::ghost_hover(&hover_theme, style))
                .on_click(
                    cx.listener(move |page, _, _, cx| page.toggle_mask(toggle_id.clone(), cx)),
                )
                .child(
                    crate::icons::icon(if revealed {
                        crate::icons::EYE_SLASH
                    } else {
                        crate::icons::EYE
                    })
                    .size(px(15.0))
                    .text_color(theme.text_muted),
                ),
        )
}

fn bordered_input(theme: &Theme, input: Entity<ComposerInput>) -> gpui::Div {
    div()
        .h(px(36.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .rounded(px(Theme::CONTROL_RADIUS))
        .border_1()
        .border_color(theme.border)
        .bg(theme.input_glass_bg())
        .child(input)
}

/// The record form's API-dialect dropdown: a closed set (the engine's
/// registered dialects), so a picker instead of a typo-prone text field.
/// The trigger mirrors [`bordered_input`]'s shape to sit flush in the
/// form's grid; the menu opens downward at [`popover::ABOVE_MODAL_PRIORITY`]
/// because the dialog itself is a modal.
fn record_api_dropdown(
    page: &ProvidersPage,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let selected = page
        .record_form
        .as_ref()
        .map(|form| form.api.clone())
        .unwrap_or_default();
    let rows: Vec<AnyElement> = page
        .api_dialects
        .ready()
        .map(|dialects| {
            dialects
                .iter()
                .map(|dialect| {
                    let picked = dialect.clone();
                    let selector = format!("record-api-option-{dialect}");
                    popover::menu_row(
                        theme,
                        *dialect == selected,
                        format!("record-api-row-{dialect}"),
                    )
                    .id(SharedString::from(format!("record-api-{dialect}")))
                    .debug_selector(move || selector.clone().into())
                    .on_click(
                        cx.listener(move |page, _, _, cx| page.pick_record_api(picked.clone(), cx)),
                    )
                    .child(
                        div()
                            .font_family(theme.font_mono.clone())
                            .text_size(crate::typography::ui_rems(12.0))
                            .child(SharedString::from(dialect.clone())),
                    )
                    .into_any_element()
                })
                .collect()
        })
        .unwrap_or_default();
    let menu = popover::popover_card(theme)
        .w(px(260.0))
        .occlude()
        .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_record_api_menu(cx)))
        .children(rows)
        .into_any_element();
    div()
        .id("record-api-dropdown")
        .debug_selector(|| "record-api-dropdown".into())
        .relative()
        .h(px(36.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(Theme::CONTROL_RADIUS))
        .border_1()
        .border_color(theme.border)
        .bg(theme.input_glass_bg())
        .cursor_pointer()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|page, _, _, _| page.record_api_menu.note_trigger_press()),
        )
        .on_click(cx.listener(|page, _, _, cx| page.toggle_record_api_menu(cx)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(theme.font_mono.clone())
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text)
                .child(SharedString::from(selected)),
        )
        .child(
            crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                .size(px(12.0))
                .flex_none()
                .text_color(theme.text_muted),
        )
        .when_some(page.record_api_menu.get(), |trigger, _| {
            trigger.child(popover::anchored_menu_below_end_with_priority(
                "record-api-menu",
                menu,
                page.record_api_menu.closing_since(),
                popover::ABOVE_MODAL_PRIORITY,
            ))
        })
        .into_any_element()
}

/// A bordered button matching [`bordered_input`]'s 36px height, so the two sit
/// flush on the same row. Caller adds id + hover + click.
fn action_button(theme: &Theme) -> gpui::Div {
    div()
        .h(px(36.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .rounded(px(Theme::CONTROL_RADIUS))
        .border_1()
        .border_color(theme.border)
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(theme.text)
        .cursor_pointer()
}

fn provider_controls_height(
    models: &Loadable<Vec<Model>>,
    hidden_count: usize,
    hidden_expanded: bool,
    variants: bool,
) -> f32 {
    let list_height = match models {
        Loadable::Idle | Loadable::Loading => 138.0,
        Loadable::Error(_) => 40.0,
        Loadable::Ready(models) if models.is_empty() => 32.0,
        Loadable::Ready(models) => (models.len() as f32 * 32.0).min(192.0),
    };
    // The Hidden block's header always shows; its rows only when expanded.
    let hidden_height = if hidden_count > 0 {
        30.0 + if hidden_expanded {
            (hidden_count as f32 * 32.0).min(96.0)
        } else {
            0.0
        }
    } else {
        0.0
    };
    // The key section is label + full-width input + its own Save/Remove
    // row. The danger row is always mounted; the hidden block adds its own
    // height when present. The Add-model action rides the Models header.
    180.0 + list_height + hidden_height + if variants { 34.0 } else { 0.0 } + 44.0
}

fn mark_provider_loading(providers: &mut Loadable<Vec<Provider>>) {
    if !matches!(providers, Loadable::Ready(_)) {
        *providers = Loadable::Loading;
    }
}

/// The variant pills at the top of an expanded organization card — the region
/// / edition picker that decides which concrete provider the key and model
/// sections below act on. Absent for single-variant rows.
fn variant_selector(
    provider: &Provider,
    active_variant: &str,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> Option<AnyElement> {
    if provider.variants.len() <= 1 {
        return None;
    }
    let org_id = provider.id.to_string();
    let mut row = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(6.0))
        .child(widgets::field_label(theme, "Provider"));
    for variant in &provider.variants {
        let variant_id = variant.id.to_string();
        let selected = variant_id == active_variant;
        let hover_theme = theme.clone();
        let mut pill = div()
            .id(SharedString::from(format!(
                "provider-variant-{org_id}-{variant_id}"
            )))
            .cursor_pointer()
            .px(px(10.0))
            .py(px(5.0))
            .rounded(px(Theme::CONTROL_RADIUS))
            .border_1()
            .text_size(crate::typography::ui_rems(11.5))
            .child(SharedString::from(variant.name.clone()));
        pill = if selected {
            pill.border_color(theme.border_strong)
                .bg(crate::theme::ink(0.05))
                .text_color(theme.text)
        } else {
            pill.border_color(theme.border)
                .text_color(theme.text_muted)
                .hover(move |style| {
                    style
                        .bg(crate::theme::ink(0.03))
                        .text_color(hover_theme.text)
                })
        };
        row = row.child(pill.on_click(cx.listener({
            let org_id = org_id.clone();
            let variant_id = variant_id.clone();
            move |page, _, _, cx| page.switch_variant(org_id.clone(), variant_id.clone(), cx)
        })));
    }
    Some(row.into_any_element())
}

fn provider_model_list(
    index: usize,
    provider_id: &str,
    models: Loadable<Vec<Model>>,
    theme: &Theme,
    page: gpui::Entity<ProvidersPage>,
    cx: &mut gpui::App,
) -> AnyElement {
    match models {
        Loadable::Idle | Loadable::Loading => {
            popover::skeleton_rows("provider-model-skeleton", theme, 4, page.entity_id(), cx)
        }
        Loadable::Error(error) => widgets::error_strip(theme, error).into_any_element(),
        Loadable::Ready(models) if models.is_empty() => div()
            .py(px(8.0))
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.text_muted)
            .child("No models available")
            .into_any_element(),
        Loadable::Ready(models) => {
            let count = models.len();
            let height = (count as f32 * 32.0).min(192.0);
            let provider_prefix = format!("{provider_id}/");
            let provider_for_removal = provider_id.to_string();
            let models = Arc::new(models);
            let row_models = Arc::clone(&models);
            let row_theme = theme.clone();
            gpui::uniform_list(
                ("provider-model-list", index),
                count,
                move |range, _window, cx| {
                    page.update(cx, |_page, cx| {
                        range
                            .filter_map(|row| row_models.get(row))
                            .map(|model| {
                                let raw_id =
                                    model.id.strip_prefix(&provider_prefix).unwrap_or(&model.id);
                                let mut list_row = div()
                                    .h(px(32.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(12.0))
                                    .child(
                                        div()
                                            .w(px(200.0))
                                            .flex_none()
                                            .truncate()
                                            .text_size(crate::typography::ui_rems(12.0))
                                            .text_color(row_theme.text)
                                            .child(SharedString::from(model.label.clone())),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .font_family(row_theme.font_mono.clone())
                                            .text_size(crate::typography::ui_rems(11.0))
                                            .text_color(row_theme.text_muted)
                                            .child(SharedString::from(raw_id.to_string())),
                                    );
                                // Only user-added rows are deletable; builtin
                                // catalog rows get the hide action instead —
                                // hiding is reversible, deleting a catalog
                                // entry is not expressible.
                                if model.custom {
                                    let provider = provider_for_removal.clone();
                                    let model_id = model.id.clone();
                                    let idle_icon = row_theme.text_muted;
                                    let hover_icon = row_theme.text;
                                    list_row = list_row.child(
                                        widgets::ghost_action(&row_theme)
                                            .id((
                                                gpui::ElementId::from(("remove-model", index)),
                                                raw_id.to_string(),
                                            ))
                                            .on_click(cx.listener(move |page, _, _, cx| {
                                                page.remove_model(
                                                    provider.clone(),
                                                    model_id.clone(),
                                                    cx,
                                                )
                                            }))
                                            // Svg reads only its own text color,
                                            // so the tint and hover live on the
                                            // icon — no background wash.
                                            .child(
                                                crate::icons::icon(crate::icons::CLOSE)
                                                    .size(px(12.0))
                                                    .text_color(idle_icon)
                                                    .hover(move |style| {
                                                        style.text_color(hover_icon)
                                                    }),
                                            ),
                                    );
                                } else {
                                    let provider = provider_for_removal.clone();
                                    let model_id = model.id.clone();
                                    let idle_icon = row_theme.text_muted;
                                    let hover_icon = row_theme.text;
                                    list_row = list_row.child(
                                        widgets::ghost_action(&row_theme)
                                            .id((
                                                gpui::ElementId::from(("hide-model", index)),
                                                raw_id.to_string(),
                                            ))
                                            .on_click(cx.listener(move |page, _, _, cx| {
                                                page.hide_model(
                                                    provider.clone(),
                                                    model_id.clone(),
                                                    cx,
                                                )
                                            }))
                                            .child(
                                                crate::icons::icon(crate::icons::EYE_SLASH)
                                                    .size(px(13.0))
                                                    .text_color(idle_icon)
                                                    .hover(move |style| {
                                                        style.text_color(hover_icon)
                                                    }),
                                            ),
                                    );
                                }
                                list_row.into_any_element()
                            })
                            .collect()
                    })
                },
            )
            .h(px(height))
            .w_full()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .into_any_element()
        }
    }
}

/// The Add-model-record dialog (`popover::modal` from the page), opened
/// from the Models header's "+ Add model" action. Basic fields up front;
/// `thinkingLevelMap`/`compat`/`headers` ride the advanced JSON textarea —
/// a full structured editor is not worth the surface.
fn record_form_dialog(
    page: &mut ProvidersPage,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let Some(form) = page.record_form.as_ref() else {
        return div().into_any_element();
    };
    let field_input = |key: &str| form.inputs.get(key).cloned();
    // Built ahead of the closures below: they hold `cx` for their
    // listeners, so the dropdown (which registers its own) must borrow
    // `cx` first.
    let api_dropdown = record_api_dropdown(page, theme, cx);
    let pair = |left: (&str, &str), right: Option<(&str, &str)>| {
        div()
            .flex()
            .gap(px(8.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(widgets::field_label(theme, left.1))
                    .children(
                        field_input(left.0)
                            .map(|input| bordered_input(theme, input).w_full().into_any_element()),
                    ),
            )
            .children(right.map(|(key, label)| {
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(widgets::field_label(theme, label))
                    .children(
                        field_input(key)
                            .map(|input| bordered_input(theme, input).w_full().into_any_element()),
                    )
                    .into_any_element()
            }))
    };
    let flag_row = |key: &'static str, label: &str, on: bool| {
        div()
            .id(SharedString::from(format!("record-flag-{key}")))
            .cursor_pointer()
            .flex()
            .items_center()
            .gap(px(8.0))
            .on_click(cx.listener(move |page, _, _, cx| page.toggle_record_flag(key, cx)))
            .child(widgets::checkbox(
                theme,
                if on {
                    widgets::CheckboxState::Checked
                } else {
                    widgets::CheckboxState::Unchecked
                },
            ))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child(label.to_string()),
            )
    };
    let mut card = popover::dialog_card(theme)
        .w(px(560.0))
        .gap(px(14.0))
        .child(
            div()
                .flex()
                .items_center()
                .child(popover::dialog_title(theme, "Add model record"))
                .child(div().flex_1())
                .child(
                    widgets::ghost_action(theme)
                        .id("record-form-close")
                        .hover(move |style| widgets::ghost_hover(theme, style))
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.record_form = None;
                            cx.notify();
                        }))
                        .child(
                            crate::icons::icon(crate::icons::CLOSE)
                                .size(px(13.0))
                                .text_color(theme.text_muted),
                        ),
                ),
        );
    card = card
        .child(pair(("id", "Model ID"), Some(("name", "Name"))))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(widgets::field_label(theme, "API dialect"))
                .child(api_dropdown),
        )
        .child(pair(
            ("contextWindow", "Context window"),
            Some(("maxTokens", "Max tokens")),
        ))
        .child(pair(
            ("inputCost", "Input cost /M"),
            Some(("outputCost", "Output cost /M")),
        ))
        .child(pair(
            ("cacheReadCost", "Cache read /M"),
            Some(("cacheWriteCost", "Cache write /M")),
        ))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(20.0))
                .child(flag_row("reasoning", "Reasoning", form.reasoning))
                .child(flag_row("image", "Image input", form.image)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(widgets::field_label(theme, "Advanced JSON"))
                .children(field_input("advanced").map(|input| {
                    div()
                        .id("record-advanced")
                        .h(px(80.0))
                        .px(px(12.0))
                        .py(px(6.0))
                        .flex()
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.input_glass_bg())
                        .overflow_y_scroll()
                        .occlude()
                        .child(input)
                        .into_any_element()
                })),
        )
        .children(form.error.clone().map(|message| {
            div()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.danger_muted.opacity(0.9))
                .child(SharedString::from(message))
        }))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    action_button(theme)
                        .id("save-record")
                        .debug_selector(|| "save-record".into())
                        .hover(|style| style.bg(crate::theme::ink(0.04)))
                        .on_click(cx.listener(|page: &mut ProvidersPage, _, _, cx| {
                            page.save_record(cx);
                        }))
                        .child("Save record"),
                )
                .child(
                    widgets::ghost_action(theme)
                        .id("cancel-record")
                        .hover(move |style| widgets::ghost_hover(theme, style))
                        .on_click(cx.listener(move |page: &mut ProvidersPage, _, _, cx| {
                            page.record_form = None;
                            cx.notify();
                        }))
                        .child("Cancel"),
                ),
        );
    card.into_any_element()
}

/// The panel's bottom row: reset to the catalog (two-step), and for custom
/// providers the definition removal (two-step; the armed copy names what
/// stays behind — the key and model records survive a definition removal).
fn panel_danger_row(
    index: usize,
    provider_id: &str,
    custom: bool,
    armed: bool,
    remove_armed: bool,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let danger = theme.danger;
    let danger_muted = theme.danger_muted;
    let reset_provider = provider_id.to_string();
    let remove_provider = provider_id.to_string();
    let remove_org = provider_id.to_string();
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(
            widgets::ghost_action(theme)
                .id(("reset-provider", index))
                .hover(move |style| style.bg(danger.opacity(0.10)).text_color(danger_muted))
                .on_click(
                    cx.listener(move |page, _, _, cx| {
                        page.arm_or_reset(reset_provider.clone(), cx)
                    }),
                )
                .child(if armed {
                    "Confirm reset — drops this provider's user-written entries"
                } else {
                    "Reset to catalog"
                }),
        )
        .children(custom.then(|| {
            widgets::ghost_action(theme)
                .id(("remove-custom-provider", index))
                .hover(move |style| style.bg(danger.opacity(0.10)).text_color(danger_muted))
                .on_click(cx.listener(move |page, _, _, cx| {
                    page.arm_or_remove(remove_provider.clone(), remove_org.clone(), cx)
                }))
                .child(if remove_armed {
                    "Confirm remove — the API key and model records stay"
                } else {
                    "Remove provider"
                })
        }))
        .into_any_element()
}

/// The page's top action row: the Add Provider primary next to the global
/// reset ghost button, both pushed right by a spacer; the reset opens the
/// confirm dialog.
fn top_action_row(theme: &Theme, cx: &mut Context<ProvidersPage>) -> AnyElement {
    let danger = theme.danger;
    let danger_muted = theme.danger_muted;
    let hover_theme = theme.clone();
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .pb(px(6.0))
        .child(div().flex_1())
        .child(
            action_button(theme)
                .id("open-add-provider")
                .debug_selector(|| "open-add-provider".into())
                .hover(move |style| widgets::ghost_hover(&hover_theme, style))
                .on_click(
                    cx.listener(|page, _, _, cx| page.open_add_dialog(AddProviderTab::Manual, cx)),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            crate::icons::icon(crate::icons::PLUS)
                                .size(px(13.0))
                                .text_color(theme.text_muted),
                        )
                        .child("Add Provider"),
                ),
        )
        .child(
            widgets::ghost_action(theme)
                .id("reset-all-providers")
                .debug_selector(|| "reset-all-providers".into())
                .hover(move |style| style.bg(danger.opacity(0.10)).text_color(danger_muted))
                .on_click(cx.listener(|page, _, _, cx| {
                    page.confirm_reset_all = true;
                    cx.notify();
                }))
                .child("Reset all providers"),
        )
        .into_any_element()
}

/// The global reset's confirm dialog (the archived page's clear-all
/// pattern): destructive, counted-out, explicit.
fn reset_all_dialog(theme: &Theme, cx: &mut Context<ProvidersPage>) -> AnyElement {
    popover::dialog_card(theme)
        .child(popover::dialog_title(theme, "Reset all providers?"))
        .child(div().mt(px(6.0)).child(popover::dialog_body(
            theme,
            "Custom models, model records, hidden lists, and endpoint overrides return to \
                 the compiled catalog for every provider. API keys are kept. This can\u{2019}t \
                 be undone.",
        )))
        .child(
            div()
                .mt(px(16.0))
                .flex()
                .flex_row()
                .justify_end()
                .gap(px(8.0))
                .child(
                    popover::btn_ghost(theme, "Cancel", "reset-all-cancel")
                        .id("reset-all-cancel")
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.confirm_reset_all = false;
                            cx.notify();
                        })),
                )
                .child(
                    popover::btn_danger(theme, "Reset all")
                        .id("reset-all-confirm")
                        .debug_selector(|| "reset-all-confirm".into())
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.confirm_reset_all = false;
                            page.reset_all(cx);
                        })),
                ),
        )
        .into_any_element()
}

/// The Add Provider dialog (design-v2): tabs over the manual form and the
/// AI setup chat. Rendered through `popover::modal` from the page.
fn add_provider_dialog(
    page: &mut ProvidersPage,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let tab = page.add_dialog.unwrap_or(AddProviderTab::Manual);
    // The AI tab is a chat surface: near-window size so the transcript has
    // room; the Manual tab keeps the compact form card.
    let mut card = popover::dialog_card(theme)
        .w(if tab == AddProviderTab::Ai {
            px(760.0)
        } else {
            px(560.0)
        })
        .when(tab == AddProviderTab::Ai, |card| card.h(px(640.0)))
        .gap(px(14.0))
        .child(
            div()
                .flex()
                .items_center()
                .child(popover::dialog_title(theme, "Add Provider"))
                .child(div().flex_1())
                .child(
                    widgets::ghost_action(theme)
                        .id("add-provider-close")
                        .debug_selector(|| "add-provider-close".into())
                        .hover(move |style| widgets::ghost_hover(theme, style))
                        .on_click(cx.listener(|page, _, _, cx| page.close_add_dialog(cx)))
                        .child(
                            crate::icons::icon(crate::icons::CLOSE)
                                .size(px(13.0))
                                .text_color(theme.text_muted),
                        ),
                ),
        )
        .child(tab_row(tab, theme, cx));
    card = match tab {
        AddProviderTab::Manual => card.child(manual_tab(
            page.new_provider_error.clone(),
            &page.new_provider_inputs,
            theme,
            cx,
        )),
        AddProviderTab::Ai => card.child(ai_tab(page, theme, cx)),
    };
    card.into_any_element()
}

/// The dialog's tab pills.
fn tab_row(tab: AddProviderTab, theme: &Theme, cx: &mut Context<ProvidersPage>) -> AnyElement {
    let mut row = div().flex().items_center().gap(px(6.0));
    for (candidate, label) in [
        (AddProviderTab::Manual, "Manual"),
        (AddProviderTab::Ai, "AI"),
    ] {
        let selected = candidate == tab;
        let hover_theme = theme.clone();
        let mut pill = div()
            .id(match candidate {
                AddProviderTab::Manual => "add-provider-tab-manual",
                AddProviderTab::Ai => "add-provider-tab-ai",
            })
            .debug_selector(move || match candidate {
                AddProviderTab::Manual => "add-provider-tab-manual".into(),
                AddProviderTab::Ai => "add-provider-tab-ai".into(),
            })
            .cursor_pointer()
            .px(px(10.0))
            .py(px(5.0))
            .rounded(px(Theme::CONTROL_RADIUS))
            .border_1()
            .text_size(crate::typography::ui_rems(11.5))
            .child(label);
        pill = if selected {
            pill.border_color(theme.border_strong)
                .bg(crate::theme::ink(0.05))
                .text_color(theme.text)
        } else {
            pill.border_color(theme.border)
                .text_color(theme.text_muted)
                .hover(move |style| {
                    style
                        .bg(crate::theme::ink(0.03))
                        .text_color(hover_theme.text)
                })
        };
        row = row.child(pill.on_click(cx.listener(move |page, _, _, cx| {
            if page.add_dialog != Some(candidate) {
                page.add_dialog = Some(candidate);
                if candidate == AddProviderTab::Ai {
                    page.prepare_setup(cx);
                }
                cx.notify();
            }
        })));
    }
    row.into_any_element()
}

/// The manual tab: the custom-provider definition form (moved from the old
/// page-bottom section).
fn manual_tab(
    error: Option<String>,
    inputs: &HashMap<&'static str, Entity<ComposerInput>>,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let field = |key: &'static str, label: &str| {
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(widgets::field_label(theme, label))
            .children(inputs.get(key).map(|input| {
                bordered_input(theme, input.clone())
                    .w_full()
                    .into_any_element()
            }))
    };
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(
            div()
                .flex()
                .gap(px(8.0))
                .child(field("id", "Provider id"))
                .child(field("name", "Display name")),
        )
        .child(field("baseUrl", "Base URL"))
        .child(field("defaultApi", "Default API dialect"))
        .children(error.map(|message| {
            div()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.danger_muted.opacity(0.9))
                .child(SharedString::from(message))
        }))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    action_button(theme)
                        .id("save-custom-provider")
                        .hover(|style| style.bg(crate::theme::ink(0.04)))
                        .on_click(cx.listener(move |page, _, _, cx| page.save_custom_provider(cx)))
                        .child("Save provider"),
                )
                .child(
                    widgets::ghost_action(theme)
                        .id("cancel-custom-provider")
                        .hover(move |style| widgets::ghost_hover(theme, style))
                        .on_click(cx.listener(move |page, _, _, cx| page.close_add_dialog(cx)))
                        .child("Cancel"),
                ),
        )
        .into_any_element()
}

/// The AI tab (V2c): the setup chat's mini transcript, the model picker,
/// and the review panel. Deliberately spare — text rows, tool chips, and
/// proposal cards are the whole surface.
fn ai_tab(page: &mut ProvidersPage, theme: &Theme, cx: &mut Context<ProvidersPage>) -> AnyElement {
    let input = page.setup_input.clone();
    let models_ready = matches!(page.setup_models, Loadable::Ready(_));
    let view = page.setup_transcript_view.clone();
    let doc_empty = page
        .setup_chat
        .as_deref()
        .map(|id| page.state.read(cx).sub_transcript(id).is_empty())
        .unwrap_or(true);
    let mut column = div().flex().flex_col().gap(px(10.0)).flex_1().min_h_0();
    if let Loadable::Error(reason) = page.setup_models.clone() {
        // The AI tab needs a working model of its own — a fresh install has
        // none. Guide the way out instead of dead-ending on an error strip:
        // the key lives on this page's provider panels, the manual form is
        // one tab away.
        return column
            .child(widgets::error_strip(theme, reason))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .pt(px(4.0))
                    .max_w(px(420.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted.opacity(0.8))
                    .child(
                        "Enter an API key for any provider in the list behind this \
                         dialog first, or add the provider by hand.",
                    )
                    .child(
                        action_button(theme)
                            .id("setup-bootstrap-manual")
                            .debug_selector(|| "setup-bootstrap-manual".into())
                            .w(px(180.0))
                            .hover(|style| style.bg(crate::theme::ink(0.04)))
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.add_dialog = Some(AddProviderTab::Manual);
                                cx.notify();
                            }))
                            .child("Use the Manual tab"),
                    ),
            )
            .into_any_element();
    }
    if models_ready || page.setup_chat.is_some() {
        // The chat surface fills the dialog like the new-chat canvas: the
        // transcript grows, the composer card stays pinned at the bottom.
        let surface: AnyElement = if let Some(view) = view.filter(|_| !doc_empty) {
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(view)
                .into_any_element()
        } else {
            div()
                .id("setup-empty-state")
                .debug_selector(|| "setup-empty-state".into())
                .flex_1()
                .min_h(px(200.0))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .max_w(px(420.0))
                        .text_align(gpui::TextAlign::Center)
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted.opacity(0.8))
                        .child(
                            "Tell the assistant which provider or model to set up — it \
                             researches the official docs, compares the local catalog, and \
                             prepares a proposal you can review here.",
                        ),
                )
                .into_any_element()
        };
        column = column.child(surface);
        // The review panel pops in when a proposal lands — the whole point
        // of the run — and stays out of the way otherwise.
        if !page.setup_proposals.is_empty() || !page.setup_applied.is_empty() {
            column = column.child(setup_review_panel(page, theme, cx));
        }
        // A turn the engine could not admit never writes a doc entry —
        // without this strip the send reads as dead silence (issue 03).
        if let Some(reason) = page
            .setup_queue
            .as_ref()
            .and_then(setup_queue_error)
            .map(|reason| format!("{reason} Send again to retry."))
        {
            column = column.child(
                widgets::error_strip(theme, reason)
                    .id("setup-queue-error")
                    .debug_selector(|| "setup-queue-error".into()),
            );
        }
        if let Some(input) = input {
            column = column.child(setup_composer(page, theme, input, cx));
        }
    }
    column.into_any_element()
}

/// The canvas-style composer card: the multiline input on top, the model
/// chip and the send circle in the toolbar row — the new-chat canvas in
/// miniature.
fn setup_composer(
    page: &mut ProvidersPage,
    theme: &Theme,
    input: Entity<ComposerInput>,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let empty = input.read(cx).text().trim().is_empty();
    div()
        .rounded(px(16.0))
        .bg(theme.input_glass_bg())
        .border_1()
        .border_color(theme.border)
        .px(px(12.0))
        .pt(px(10.0))
        .pb(px(8.0))
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(input)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(div().flex_1())
                // The chip picks the ASSISTANT's model — the brain researching
                // the catalog — not the model being added. Users kept reading
                // it as the target; the label settles that.
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(theme.text_muted.opacity(0.8))
                        .child("Assistant"),
                )
                .child(setup_model_picker(page, theme, cx))
                .child(
                    div()
                        .id("setup-send")
                        .size(px(28.0))
                        .flex_none()
                        .rounded_full()
                        .bg(theme.text)
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(empty, |el| el.opacity(0.35))
                        .when(!empty, |el| {
                            el.cursor_pointer()
                                .hover(|style| style.opacity(0.85))
                                .on_click(cx.listener(|page, _, _, cx| page.send_setup_message(cx)))
                        })
                        .child(
                            crate::icons::icon(crate::icons::ARROW_UP)
                                .size(px(14.0))
                                .text_color(theme.bg),
                        ),
                ),
        )
        .into_any_element()
}

/// The setup picker's catalog, grouped by provider in catalog order — the
/// rail the menu walks. The catalog is built per configured provider
/// ([`configured_model_catalog`]), so every group here has a stored key.
struct ModelGroup {
    id: ProviderId,
    models: Vec<Model>,
}

fn setup_model_groups(models: &[Model]) -> Vec<ModelGroup> {
    let mut groups: Vec<ModelGroup> = Vec::new();
    for model in models {
        match groups.iter_mut().find(|group| group.id == model.provider) {
            Some(group) => group.models.push(model.clone()),
            None => groups.push(ModelGroup {
                id: model.provider.clone(),
                models: vec![model.clone()],
            }),
        }
    }
    groups
}

/// A rail tab's label: the provider row's display name when the page's own
/// list carries it, else the raw id.
fn provider_label(id: &ProviderId, providers: Option<&Vec<Provider>>) -> String {
    providers
        .and_then(|rows| {
            rows.iter()
                .flat_map(|row| row.variants.iter())
                .find(|variant| variant.id == *id)
                .map(|variant| variant.name.clone())
        })
        .or_else(|| {
            providers.and_then(|rows| {
                rows.iter()
                    .find(|row| row.id == *id)
                    .map(|row| row.name.clone())
            })
        })
        .unwrap_or_else(|| id.0.clone())
}

/// The failure a queue frame surfaces: the queue-level error (storage /
/// checkpoint faults) or the first errored pending item (a turn that could
/// not be admitted — missing key, unresolvable model). `None` while the
/// queue is healthy.
fn setup_queue_error(queue: &holt_proto::MessageQueue) -> Option<String> {
    queue
        .error
        .clone()
        .or_else(|| queue.pending.iter().find_map(|item| item.error.clone()))
}

/// The composer card's model chip (the new-chat canvas' pattern): quiet
/// text + chevron, the menu opening ABOVE — the composer sits at the
/// dialog's bottom.
fn setup_model_picker(
    page: &mut ProvidersPage,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let catalog: Vec<Model> = page
        .setup_models
        .ready()
        .map(|models| models.to_vec())
        .unwrap_or_default();
    let selected_id = page.setup_selected_model.clone();
    let groups = setup_model_groups(&catalog);
    // The rail's active provider: the last rail pick, else the current
    // selection's provider, else the first group. A one-provider catalog
    // (the tests' and most fresh installs') renders no rail at all.
    let active_provider = page
        .setup_model_provider
        .clone()
        .filter(|id| groups.iter().any(|group| &group.id == id))
        .or_else(|| {
            selected_id
                .as_deref()
                .and_then(|id| id.split_once('/'))
                .map(|(provider, _)| ProviderId(provider.into()))
                .filter(|id| groups.iter().any(|group| &group.id == id))
        })
        .or_else(|| groups.first().map(|group| group.id.clone()));
    let mut rows = Vec::new();
    for (index, model) in groups
        .iter()
        .find(|group| Some(&group.id) == active_provider.as_ref())
        .map(|group| group.models.as_slice())
        .unwrap_or(&[])
        .iter()
        .enumerate()
    {
        let qualified = model.id.clone();
        let selected = selected_id.as_deref() == Some(model.id.as_str());
        rows.push(
            popover::menu_row(theme, selected, format!("setup-model-option-{index}"))
                .id(SharedString::from(format!("setup-model-option-{index}")))
                .debug_selector(move || format!("setup-model-option-{index}"))
                .on_click(
                    cx.listener(move |page, _, _, cx| page.pick_setup_model(qualified.clone(), cx)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(crate::typography::ui_rems(12.0))
                        .child(SharedString::from(model.label.clone())),
                )
                .child(
                    div()
                        .flex_none()
                        .font_family(theme.font_mono.clone())
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(
                            model
                                .id
                                .split_once('/')
                                .map(|(_, tail)| tail.to_string())
                                .unwrap_or_else(|| model.id.clone()),
                        )),
                )
                .when(selected, |row| {
                    row.child(
                        crate::icons::icon(crate::icons::CHECK)
                            .size(px(13.0))
                            .text_color(theme.accent),
                    )
                })
                .into_any_element(),
        );
    }
    let list = div()
        .id("setup-model-scroll")
        .flex_1()
        .min_w_0()
        .max_h(px(280.0))
        .overflow_y_scroll()
        .occlude()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .children(rows)
        .into_any_element();
    // One provider configured: the rail would be a single dead tab — the
    // flat list is the whole menu.
    let menu: AnyElement = if groups.len() > 1 {
        let mut rail = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .pr(px(6.0))
            .mr(px(6.0))
            .border_r_1()
            .border_color(theme.border);
        for (index, group) in groups.iter().enumerate() {
            let id = group.id.clone();
            let active = Some(&group.id) == active_provider.as_ref();
            let label = provider_label(&group.id, page.providers.ready());
            rail = rail.child(
                div()
                    .id(SharedString::from(format!("setup-model-rail-{index}")))
                    .debug_selector(move || format!("setup-model-rail-{index}"))
                    .w_full()
                    .px(px(6.0))
                    .py(px(5.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .when(active, |tab| tab.bg(crate::theme::ink(0.06)))
                    .when(!active, |tab| {
                        tab.hover(|style| style.bg(crate::theme::ink(0.03)))
                    })
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(if active { theme.text } else { theme.text_muted })
                    .child(SharedString::from(label))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.setup_model_provider = Some(id.clone());
                        cx.notify();
                    })),
            );
        }
        div()
            .flex()
            .flex_row()
            .child(rail)
            .child(list)
            .into_any_element()
    } else {
        list
    };
    let menu = popover::popover_card(theme)
        .w(px(460.0))
        .occlude()
        .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_setup_model_menu(cx)))
        .child(menu)
        .into_any_element();
    let selected_label = SharedString::from(
        catalog
            .iter()
            .find(|model| Some(model.id.as_str()) == selected_id.as_deref())
            .map(|model| model.label.clone())
            // A stored default from another variant won't match this
            // catalog — fall back to the bare model id, not the qualified.
            .or_else(|| {
                selected_id
                    .as_deref()
                    .and_then(|id| id.rsplit('/').next())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "Select a model".into()),
    );
    div()
        .id("setup-model-dropdown")
        .debug_selector(|| "setup-model-dropdown".into())
        .relative()
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(6.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .cursor_pointer()
        .hover(|style| style.bg(crate::theme::ink(0.05)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|page, _, _, _| page.setup_model_menu.note_trigger_press()),
        )
        .on_click(cx.listener(|page, _, _, cx| page.toggle_setup_model_menu(cx)))
        .child(
            div()
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text)
                .child(selected_label),
        )
        .child(
            crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                .size(px(12.0))
                .flex_none()
                .text_color(theme.text_muted),
        )
        .when_some(page.setup_model_menu.get(), |trigger, _| {
            trigger.child(popover::anchored_menu_above_end_with_priority(
                "setup-model-menu",
                menu,
                page.setup_model_menu.closing_since(),
                popover::ABOVE_MODAL_PRIORITY,
            ))
        })
        .into_any_element()
}

/// The review panel's refresh signal: (total, resolved) `model_proposal`
/// tool parts in the setup transcript. The panel re-reads only when this
/// moves — text/tool ticks alone never trigger the RPC.
fn proposal_signature(transcript: &[SessionMessageEntry]) -> (usize, usize) {
    let mut total = 0;
    let mut resolved = 0;
    for entry in transcript {
        for part in &entry.parts {
            if let MessagePart::Tool {
                call,
                resolved: done,
                ..
            } = part
                && matches!(call, ToolCall::Unknown { name, .. } if name == "model_proposal")
            {
                total += 1;
                resolved += *done as usize;
            }
        }
    }
    (total, resolved)
}

/// The URL's host — the part that decides where a key would be sent, and
/// the only part worth the row's width.
fn url_host(url: &str) -> &str {
    url.split("//")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .filter(|host| !host.is_empty())
        .unwrap_or(url)
}

/// Token counts in picker shorthand: 321000 → "321k", 2000000 → "2M".
fn fmt_tokens(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{}M", value / 1_000_000)
    } else if value >= 1_000 {
        format!("{}k", value / 1_000)
    } else {
        format!("{value}")
    }
}

/// One change row's render data: the subject and the fields that decide
/// whether it is safe to write — the endpoint a key would ride to, the
/// window the record bills against, the cost the ledger books. Engine
/// data only (the `proposal_views` JSON), never the agent's prose.
fn change_subject(change: &serde_json::Value) -> (String, Option<String>) {
    let str_field = |key: &str| change[key].as_str().map(str::to_string).unwrap_or_default();
    let action = str_field("action");
    match action.as_str() {
        "upsert_model_record" => {
            let record = &change["record"];
            let mut detail = Vec::new();
            if let Some(url) = record["baseUrl"].as_str() {
                detail.push(url_host(url).to_string());
            }
            if let Some(window) = record["contextWindow"].as_u64() {
                detail.push(format!("ctx {}", fmt_tokens(window)));
            }
            if let Some(max) = record["maxTokens"].as_u64() {
                detail.push(format!("out {}", fmt_tokens(max)));
            }
            if let (Some(input), Some(output)) = (
                record["cost"]["input"].as_f64(),
                record["cost"]["output"].as_f64(),
            ) {
                detail.push(format!("${input}/${output} per M"));
            }
            (
                format!("± {}/{}", str_field("providerId"), str_field("modelId")),
                (!detail.is_empty()).then(|| detail.join(" · ")),
            )
        }
        "upsert_custom_provider" => {
            let provider = &change["provider"];
            let mut detail = Vec::new();
            if let Some(url) = provider["baseUrl"].as_str() {
                detail.push(url_host(url).to_string());
            }
            if let Some(api) = provider["defaultApi"].as_str() {
                detail.push(api.to_string());
            }
            (
                format!("+ provider {}", str_field("providerId")),
                (!detail.is_empty()).then(|| detail.join(" · ")),
            )
        }
        "remove_custom_provider" => (format!("− provider {}", str_field("providerId")), None),
        "remove_model_record" => (
            format!("− {}/{}", str_field("providerId"), str_field("modelId")),
            None,
        ),
        "set_hidden_models" => {
            let ids: Vec<&str> = change["modelIds"]
                .as_array()
                .map(|ids| ids.iter().filter_map(|id| id.as_str()).collect())
                .unwrap_or_default();
            let shown = if ids.len() > 6 {
                format!("{} …", ids[..6].join(", "))
            } else {
                ids.join(", ")
            };
            (
                format!("! {}: hide {}", str_field("providerId"), shown),
                None,
            )
        }
        other => (other.to_string(), None),
    }
}

/// The review panel: one card per pending proposal — the engine-built
/// summary, one structured row per change (the fields that decide whether
/// writing is safe), and Write/Discard on the card. Failures land inline
/// on the card they belong to; a written proposal keeps a terminal card
/// instead of vanishing. Proposals live on the session-scoped chat and die
/// with it, so every stored proposal belongs to this session.
fn setup_review_panel(
    page: &mut ProvidersPage,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let danger = theme.danger;
    let danger_muted = theme.danger_muted;
    let accent = theme.accent;
    let applying = page.setup_applying.clone();
    let pending: Vec<serde_json::Value> = page.setup_proposals.clone();
    let applied = page.setup_applied.clone();
    let mut panel = div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(widgets::field_label(theme, "Pending proposals"));
    if pending.is_empty() && applied.is_empty() {
        panel = panel.child(
            div()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_muted.opacity(0.7))
                .child("None yet — a proposal appears here once the assistant prepares one."),
        );
    }
    for (index, proposal) in applied.iter().enumerate() {
        let summary = proposal["summary"].as_str().unwrap_or_default().to_string();
        panel = panel.child(
            div()
                .id(("setup-applied-card", index))
                .debug_selector(move || format!("setup-applied-card-{index}"))
                .flex()
                .items_center()
                .gap(px(8.0))
                .p(px(8.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border.opacity(0.5))
                .text_size(crate::typography::ui_rems(11.5))
                .text_color(theme.text_muted)
                .child(
                    crate::icons::icon(crate::icons::CHECK)
                        .size(px(13.0))
                        .flex_none()
                        .text_color(accent),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(format!("Written — {summary}"))),
                ),
        );
    }
    for (index, proposal) in pending.iter().enumerate() {
        let id = proposal["id"].as_str().unwrap_or_default().to_string();
        let summary = proposal["summary"].as_str().unwrap_or_default().to_string();
        let apply_id = id.clone();
        let discard_id = id.clone();
        let busy = applying.as_deref() == Some(id.as_str());
        let error = page.setup_apply_errors.get(&id).cloned();
        let mut card = div()
            .id(("setup-proposal", index))
            .debug_selector(move || format!("setup-proposal-card-{index}"))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .p(px(8.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.5))
                            .child(SharedString::from(summary)),
                    )
                    .child(
                        action_button(theme)
                            .id(("setup-proposal-apply", index))
                            .debug_selector(move || format!("setup-proposal-apply-{index}"))
                            .when(busy, |button| button.opacity(0.4))
                            .hover(|style| style.bg(crate::theme::ink(0.04)))
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.apply_setup_proposal(apply_id.clone(), cx)
                            }))
                            .child("Write"),
                    )
                    .child(
                        widgets::ghost_action(theme)
                            .id(("setup-proposal-discard", index))
                            .debug_selector(move || format!("setup-proposal-discard-{index}"))
                            .hover(move |style| {
                                style.bg(danger.opacity(0.10)).text_color(danger_muted)
                            })
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.discard_setup_proposal(discard_id.clone(), cx)
                            }))
                            .child("Discard"),
                    ),
            );
        for (row, change) in proposal["changes"]
            .as_array()
            .map(|changes| changes.as_slice())
            .unwrap_or(&[])
            .iter()
            .enumerate()
        {
            let (subject, detail) = change_subject(change);
            let selector = format!("setup-change-row-{index}-{row}");
            card = card.child(
                div()
                    .id(SharedString::from(format!(
                        "setup-proposal-change-{index}-{row}"
                    )))
                    .debug_selector(move || selector.clone())
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .pl(px(8.0))
                    .border_l_2()
                    .border_color(theme.border.opacity(0.6))
                    .child(
                        div()
                            .font_family(theme.font_mono.clone())
                            .text_size(crate::typography::ui_rems(11.0))
                            .child(SharedString::from(subject)),
                    )
                    .when_some(detail, |card, detail| {
                        card.child(
                            div()
                                .font_family(theme.font_mono.clone())
                                .text_size(crate::typography::ui_rems(10.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(detail)),
                        )
                    }),
            );
        }
        if let Some(error) = error {
            card = card.child(
                div()
                    .id(("setup-proposal-error", index))
                    .debug_selector(move || format!("setup-proposal-error-{index}"))
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(danger)
                    .child(SharedString::from(format!(
                        "{error} Ask the assistant to propose again."
                    ))),
            );
        }
        panel = panel.child(card);
    }
    panel.into_any_element()
}

/// The configured catalog for the picker: one ListModels per configured
/// provider variant (the title-settings picker's discovery).
async fn configured_model_catalog(engine: &crate::state::EngineHandle) -> Vec<Model> {
    let Ok(value) = engine
        .client()
        .call(methods::LIST_PROVIDERS, serde_json::json!({}))
        .await
    else {
        return Vec::new();
    };
    let Ok(providers) = serde_json::from_value::<Vec<Provider>>(value) else {
        return Vec::new();
    };
    let mut models = Vec::new();
    for provider in &providers {
        for variant in &provider.variants {
            if !variant.configured {
                continue;
            }
            if let Ok(value) = engine
                .client()
                .call(
                    methods::LIST_MODELS,
                    serde_json::json!({ "providerId": variant.id.0 }),
                )
                .await
                && let Ok(mut provider_models) = serde_json::from_value::<Vec<Model>>(value)
            {
                models.append(&mut provider_models);
            }
        }
    }
    models
}

/// The first configured provider's first model — the fallback when no chat
/// carries a config.
async fn first_configured_model(engine: &crate::state::EngineHandle) -> Option<(String, String)> {
    let value = engine
        .client()
        .call(methods::LIST_PROVIDERS, serde_json::json!({}))
        .await
        .ok()?;
    let providers = serde_json::from_value::<Vec<Provider>>(value).ok()?;
    let provider = providers
        .iter()
        .flat_map(|row| &row.variants)
        .find(|variant| variant.configured)?;
    let value = engine
        .client()
        .call(
            methods::LIST_MODELS,
            serde_json::json!({ "providerId": provider.id.0 }),
        )
        .await
        .ok()?;
    let models = serde_json::from_value::<Vec<Model>>(value).ok()?;
    let first = models.first()?;
    // Catalog ids are already provider-qualified.
    Some((provider.id.0.clone(), first.id.clone()))
}

/// The review panel's current proposals.
async fn setup_proposal_rows(
    engine: &crate::state::EngineHandle,
    chat_id: &str,
) -> Vec<serde_json::Value> {
    engine
        .client()
        .call(
            methods::LIST_MODEL_PROPOSALS,
            serde_json::json!({ "chatId": chat_id }),
        )
        .await
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

/// Client-side checks for a new custom provider — the quick feedback before
/// the engine's authoritative validation replies.
/// Plaintext http would carry the key and the conversation in the clear,
/// so the form mirrors the engine's rule: http is for local servers only.
fn base_url_problem(base_url: &str) -> Option<String> {
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Some("Base URL must start with http:// or https://".into());
    }
    if let Some(rest) = base_url.strip_prefix("http://") {
        let host = rest.split(['/', '?']).next().unwrap_or_default();
        let host = host.rsplit_once(':').map_or(host, |(host, _)| host);
        let host = host.trim_matches(|character| character == '[' || character == ']');
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .map(|ip| ip.is_loopback())
                .unwrap_or(false);
        if !loopback {
            return Some("Plaintext http is allowed only for localhost endpoints".into());
        }
    }
    None
}

fn new_provider_problem(id: &str, base_url: &str, default_api: &str) -> Option<String> {
    if id.is_empty() {
        return Some("Provider id is required".into());
    }
    if id.contains('/') {
        return Some("Provider id cannot contain '/'".into());
    }
    if let Some(problem) = base_url_problem(base_url) {
        return Some(problem);
    }
    if default_api.is_empty() {
        return Some("Default API dialect is required".into());
    }
    None
}

/// Builds the model record the engine expects from the form's texts:
/// numbers parse, costs default to zero, and the advanced JSON (when
/// present) must be an object whose keys ride along — except the form's
/// own fields, which the advanced object can never override. `baseUrl`
/// is not a form field: empty means omitted, and the engine fills the
/// provider's default endpoint (Advanced JSON may still set it).
fn build_record_json(
    provider: &str,
    texts: &HashMap<String, String>,
    reasoning: bool,
    image: bool,
) -> Result<serde_json::Value, String> {
    let text = |key: &str| texts.get(key).cloned().unwrap_or_default();
    let id = text("id");
    if id.is_empty() {
        return Err("Model ID is required".into());
    }
    let name = {
        let name = text("name");
        if name.is_empty() { id.clone() } else { name }
    };
    let base_url = text("baseUrl");
    if !base_url.is_empty() && !base_url.starts_with("http://") && !base_url.starts_with("https://")
    {
        return Err("Base URL must start with http:// or https://".into());
    }
    let api = text("api");
    if api.is_empty() {
        return Err("API dialect is required".into());
    }
    let number = |key: &str| -> Result<u64, String> {
        let raw = text(key);
        raw.parse::<u64>()
            .map_err(|_| format!("{key} must be a whole number"))
    };
    let context_window = number("contextWindow")?;
    if context_window == 0 {
        return Err("Context window must be greater than zero".into());
    }
    let max_tokens = number("maxTokens")?;
    let cost = |key: &str| -> Result<f64, String> {
        let raw = text(key);
        if raw.is_empty() {
            return Ok(0.0);
        }
        raw.parse::<f64>()
            .map_err(|_| format!("{key} must be a number"))
    };
    let mut input = vec!["text".to_string()];
    if image {
        input.push("image".to_string());
    }
    let mut record = serde_json::json!({
        "id": id,
        "name": name,
        "api": api,
        "provider": provider,
        "reasoning": reasoning,
        "input": input,
        "cost": {
            "input": cost("inputCost")?,
            "output": cost("outputCost")?,
            "cacheRead": cost("cacheReadCost")?,
            "cacheWrite": cost("cacheWriteCost")?,
        },
        "contextWindow": context_window,
        "maxTokens": max_tokens,
    });
    // No baseUrl input in the form: omitted means the engine fills the
    // provider's default endpoint; the advanced JSON below is the override
    // hatch (per-model gateways keep working).
    if !base_url.is_empty() {
        record["baseUrl"] = serde_json::Value::String(base_url);
    }
    let advanced = text("advanced");
    if !advanced.is_empty() {
        let parsed: serde_json::Value = serde_json::from_str(&advanced)
            .map_err(|error| format!("Advanced JSON does not parse: {error}"))?;
        let Some(object) = parsed.as_object() else {
            return Err("Advanced JSON must be an object".into());
        };
        if let Some(target) = record.as_object_mut() {
            for (key, value) in object {
                // The form's fields are authoritative — the advanced object
                // carries only what the form does not own (baseUrl included:
                // omitting it is the default-endpoint case).
                if ![
                    "id",
                    "name",
                    "api",
                    "provider",
                    "reasoning",
                    "input",
                    "cost",
                    "contextWindow",
                    "maxTokens",
                ]
                .contains(&key.as_str())
                {
                    target.insert(key.clone(), value.clone());
                }
            }
        }
    }
    Ok(record)
}

// ---------------------------------------------------------------------------
// The P3 sections: hidden rows, the record form, the danger row, the
// custom-provider form, and the global reset (ADR-0028's manual half).
// ---------------------------------------------------------------------------

/// The hidden block: a collapsible header (chevron + count, collapsed by
/// default) over one greyed row per hidden model with an unhide action.
/// `None` when the provider hides nothing.
fn hidden_rows(
    index: usize,
    provider_id: &str,
    hidden: Loadable<Vec<HiddenModel>>,
    expanded: bool,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> Option<AnyElement> {
    let rows = hidden.ready()?;
    if rows.is_empty() {
        return None;
    }
    let count = rows.len();
    let toggle_id = provider_id.to_string();
    let chevron = if expanded {
        crate::icons::ALT_ARROW_DOWN
    } else {
        crate::icons::ALT_ARROW_RIGHT
    };
    let header = div()
        .id(("hidden-header", index))
        .cursor_pointer()
        .w_auto()
        .flex()
        .items_center()
        .gap(px(6.0))
        .on_click(cx.listener(move |page, _, _, cx| page.toggle_hidden(toggle_id.clone(), cx)))
        .child(
            crate::icons::icon(chevron)
                .size(px(11.0))
                .text_color(theme.text_muted),
        )
        .child(widgets::field_label(theme, "Hidden"))
        .child(
            div()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_muted.opacity(0.7))
                .child(SharedString::from(format!("{count}"))),
        );
    let block = div()
        .flex()
        .flex_col()
        .items_start()
        .gap(px(6.0))
        .child(header);
    if !expanded {
        return Some(block.into_any_element());
    }
    let height = (count as f32 * 32.0).min(96.0);
    let provider_prefix = format!("{provider_id}/");
    let provider = provider_id.to_string();
    let rows = Arc::new(rows.clone());
    let row_theme = theme.clone();
    let page = cx.entity();
    let list = gpui::uniform_list(
        ("provider-hidden-list", index),
        count,
        move |range, _window, cx| {
            page.update(cx, |_page, cx| {
                range
                    .filter_map(|row| rows.get(row))
                    .map(|model| {
                        let raw_id = model.id.strip_prefix(&provider_prefix).unwrap_or(&model.id);
                        let unhide_provider = provider.clone();
                        let unhide_id = model.id.clone();
                        let idle = row_theme.text_muted.opacity(0.55);
                        let hover = row_theme.text_muted;
                        div()
                            .h(px(32.0))
                            .flex()
                            .items_center()
                            .gap(px(12.0))
                            .opacity(0.55)
                            .child(
                                div()
                                    .w(px(200.0))
                                    .flex_none()
                                    .truncate()
                                    .text_size(crate::typography::ui_rems(12.0))
                                    .text_color(row_theme.text_muted)
                                    .child(SharedString::from(
                                        model.label.clone().unwrap_or_else(|| raw_id.to_string()),
                                    )),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(row_theme.font_mono.clone())
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(row_theme.text_muted.opacity(0.7))
                                    .child(SharedString::from(raw_id.to_string())),
                            )
                            .child(
                                widgets::ghost_action(&row_theme)
                                    .id((
                                        gpui::ElementId::from(("unhide-model", index)),
                                        raw_id.to_string(),
                                    ))
                                    .hover(move |style| style.text_color(hover))
                                    .on_click(cx.listener(move |page, _, _, cx| {
                                        page.unhide_model(
                                            unhide_provider.clone(),
                                            unhide_id.clone(),
                                            cx,
                                        )
                                    }))
                                    .child(
                                        crate::icons::icon(crate::icons::EYE)
                                            .size(px(13.0))
                                            .text_color(idle),
                                    ),
                            )
                            .into_any_element()
                    })
                    .collect()
            })
        },
    )
    .h(px(height))
    .w_full()
    .occlude()
    .on_scroll_wheel(|_, _, cx| cx.stop_propagation());
    Some(block.child(list).into_any_element())
}

impl Drop for ProvidersPage {
    fn drop(&mut self) {
        self.revealed.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_keeps_ready_provider_rows_mounted() {
        let mut providers = Loadable::Ready(Vec::<Provider>::new());

        mark_provider_loading(&mut providers);

        assert!(matches!(providers, Loadable::Ready(_)));
    }

    fn record_texts(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn a_complete_form_builds_a_servable_record() {
        let texts = record_texts(&[
            ("id", "acme-1"),
            ("baseUrl", "https://acme.example/v1"),
            ("api", "openai-completions"),
            ("contextWindow", "321000"),
            ("maxTokens", "16384"),
            ("inputCost", "1.5"),
            ("advanced", "{\"thinkingLevelMap\": {\"high\": \"high\"}}"),
        ]);
        let record = build_record_json("acme", &texts, true, true).unwrap();
        assert_eq!(record["id"], "acme-1");
        // A blank name falls back to the id; costs default to zero.
        assert_eq!(record["name"], "acme-1");
        assert_eq!(record["provider"], "acme");
        // An explicit baseUrl (the endpoint-fix case) rides along.
        assert_eq!(record["baseUrl"], "https://acme.example/v1");
        assert_eq!(record["input"], serde_json::json!(["text", "image"]));
        assert_eq!(record["cost"]["output"], 0.0);
        assert_eq!(record["cost"]["input"], 1.5);
        assert_eq!(record["contextWindow"], 321_000);
        assert_eq!(
            record["thinkingLevelMap"]["high"],
            serde_json::json!("high")
        );
    }

    #[test]
    fn a_form_without_a_base_url_leaves_it_to_the_engine() {
        // The form no longer asks for baseUrl: adding a model to a provider
        // rides the provider's endpoint, so the record omits the field and
        // the engine fills the default.
        let texts = record_texts(&[
            ("id", "acme-1"),
            ("api", "openai-completions"),
            ("contextWindow", "1000"),
            ("maxTokens", "100"),
        ]);
        let record = build_record_json("acme", &texts, false, false).unwrap();
        assert!(record.get("baseUrl").is_none());
        // Advanced JSON remains the override hatch for per-model gateways.
        let gateway = record_texts(&[
            ("id", "acme-1"),
            ("api", "openai-completions"),
            ("contextWindow", "1000"),
            ("maxTokens", "100"),
            ("advanced", "{\"baseUrl\": \"https://gateway.example/v1\"}"),
        ]);
        let record = build_record_json("acme", &gateway, false, false).unwrap();
        assert_eq!(record["baseUrl"], "https://gateway.example/v1");
    }

    #[test]
    fn record_forms_reject_bad_input_before_the_rpc() {
        let base = |pairs: &[(&str, &str)]| record_texts(pairs);
        let good = |extra: &[(&str, &str)]| {
            let mut texts = base(&[
                ("id", "acme-1"),
                ("baseUrl", "https://acme.example/v1"),
                ("api", "openai-completions"),
                ("contextWindow", "1000"),
                ("maxTokens", "100"),
            ]);
            for (key, value) in extra {
                texts.insert(key.to_string(), value.to_string());
            }
            texts
        };
        assert!(
            build_record_json("acme", &base(&[]), false, false)
                .unwrap_err()
                .contains("Model ID is required")
        );
        assert!(
            build_record_json("acme", &good(&[("baseUrl", "ftp://nope")]), false, false).is_err()
        );
        // No baseUrl is fine — the engine inherits the provider's endpoint.
        assert!(build_record_json("acme", &good(&[("baseUrl", "")]), false, false).is_ok());
        assert!(build_record_json("acme", &good(&[("contextWindow", "0")]), false, false).is_err());
        assert!(
            build_record_json("acme", &good(&[("contextWindow", "lots")]), false, false)
                .unwrap_err()
                .contains("whole number")
        );
        assert!(
            build_record_json("acme", &good(&[("outputCost", "cheap")]), false, false)
                .unwrap_err()
                .contains("must be a number")
        );
        assert!(
            build_record_json("acme", &good(&[("advanced", "[1,2]")]), false, false)
                .unwrap_err()
                .contains("must be an object")
        );
        // The advanced object cannot smuggle the record's identity fields.
        let smuggled = good(&[("advanced", "{\"provider\": \"evil\", \"id\": \"evil-1\"}")]);
        let record = build_record_json("acme", &smuggled, false, false).unwrap();
        assert_eq!(record["provider"], "acme");
    }

    #[test]
    fn new_provider_forms_check_shape_before_the_rpc() {
        assert!(new_provider_problem("", "https://x.example", "openai-completions").is_some());
        assert!(new_provider_problem("a/b", "https://x.example", "openai-completions").is_some());
        assert!(new_provider_problem("acme", "x.example", "openai-completions").is_some());
        assert!(new_provider_problem("acme", "https://x.example", "").is_some());
        assert!(
            new_provider_problem("acme", "https://x.example/v1", "openai-completions").is_none()
        );
    }

    fn proposal_part(resolved: bool) -> MessagePart {
        MessagePart::Tool {
            id: "t".into(),
            call: ToolCall::Unknown {
                name: "model_proposal".into(),
                input: None,
            },
            is_error: false,
            resolved,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
            subagent_usage: None,
            gate: None,
        }
    }

    fn entry_with(id: &str, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: holt_doc::MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "local".into(),
            status: None,
            continuation_of: None,
        }
    }

    #[test]
    fn proposal_signature_counts_only_proposal_parts() {
        assert_eq!(proposal_signature(&[]), (0, 0));
        let transcript = vec![
            entry_with("m1", vec![proposal_part(false)]),
            entry_with("m2", vec![proposal_part(true), proposal_part(true)]),
        ];
        assert_eq!(proposal_signature(&transcript), (3, 2));
        // Other tools never move the signature (no panel refresh storm).
        let mut other = proposal_part(true);
        if let MessagePart::Tool { call, .. } = &mut other {
            *call = ToolCall::WebSearch { query: "x".into() };
        }
        assert_eq!(proposal_signature(&[entry_with("m3", vec![other])]), (0, 0));
    }

    #[test]
    fn panel_heights_account_for_the_new_sections() {
        let models = Loadable::Ready(Vec::<Model>::new());
        let bare = provider_controls_height(&models, 0, false, false);
        // Hidden rows and their absence move the panel's animated height;
        // the collapsed Hidden block adds only its header.
        let collapsed = provider_controls_height(&models, 2, false, false);
        let expanded = provider_controls_height(&models, 2, true, false);
        assert!(collapsed > bare);
        assert!(expanded > collapsed);
    }

    // ---- The AI tab's headless repro (issue 03: no conversation renders
    // after send) -----------------------------------------------

    fn entry_json(
        id: &str,
        role: &str,
        text: &str,
        created_at: i64,
        status: Option<&str>,
    ) -> serde_json::Value {
        let mut entry = serde_json::json!({
            "id": id,
            "role": role,
            "parts": [{"kind": "text", "id": "p0", "text": text}],
            "createdAt": created_at,
            "deviceId": "local",
        });
        if let Some(status) = status {
            entry["status"] = serde_json::json!(status);
        }
        entry
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis() as i64)
            .unwrap_or(0)
    }

    /// The fake engine behind the dialog repro: the providers/models reads,
    /// a fresh setup chat per `StartModelSetupChat` (incrementing ids,
    /// `deleteChat` ops recorded), and a `WatchDocMessages` stream that
    /// advances when `QueueCommand` lands — the engine's admission shape
    /// (the user entry stamped at now, the assistant streaming).
    struct FakeSetupEngine {
        doc: tokio::sync::watch::Sender<serde_json::Value>,
        queue: tokio::sync::watch::Sender<serde_json::Value>,
        /// The picker's catalog — the second entry is the user's custom
        /// model, dropped when `ResetProviderCatalog` lands.
        catalog: std::sync::Mutex<Vec<serde_json::Value>>,
        resets: std::sync::Mutex<Vec<serde_json::Value>>,
        queued: std::sync::Mutex<Vec<serde_json::Value>>,
        /// True = the NEXT turn never admits (the engine's post-QueueCommand
        /// failure shape: no doc entry, the queue frame carries the reason);
        /// consumed by the first send, so a retry succeeds.
        fail_first_admission: std::sync::atomic::AtomicBool,
        /// The review panel's stored proposals, served verbatim by
        /// ListModelProposals (the engine's consume-on-apply included).
        proposals: std::sync::Mutex<Vec<serde_json::Value>>,
        applies: std::sync::Mutex<Vec<serde_json::Value>>,
        /// chatIds the page deleted (the dialog close's deleteChat op).
        deleted_chats: std::sync::Mutex<Vec<String>>,
        /// StartModelSetupChat's fresh id sequence.
        chat_seq: std::sync::atomic::AtomicUsize,
        /// SaveModelRecord params, in call order.
        records: std::sync::Mutex<Vec<serde_json::Value>>,
        /// True = ApplyModelProposal fails with the staleness error instead
        /// of applying.
        fail_applies: std::sync::atomic::AtomicBool,
        /// True = no provider is configured (the fresh-install dead end the
        /// AI tab must guide out of).
        unconfigured: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl holt_rpc::RpcService for FakeSetupEngine {
        async fn handle(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<holt_rpc::RpcReply, holt_rpc::RpcError> {
            use futures::StreamExt as _;
            use holt_rpc::{RpcError, RpcReply};
            match method {
                methods::LIST_PROVIDERS => {
                    let configured = !self.unconfigured.load(std::sync::atomic::Ordering::SeqCst);
                    let flag = |value: bool| serde_json::json!(value);
                    RpcReply::value(&serde_json::json!([
                        {
                            "id": "acme",
                            "name": "Acme",
                            "abbreviation": "A",
                            "configured": flag(configured),
                            "variants": [{
                                "id": "acme",
                                "name": "Acme",
                                "configured": flag(configured),
                            }],
                            "custom": false,
                        },
                        {
                            "id": "beta",
                            "name": "Beta Labs",
                            "abbreviation": "B",
                            "configured": flag(configured),
                            "variants": [{
                                "id": "beta",
                                "name": "Beta Labs",
                                "configured": flag(configured),
                            }],
                            "custom": false,
                        },
                    ]))
                }
                methods::LIST_MODELS => {
                    let provider = params["providerId"].as_str().unwrap_or_default();
                    let rows: Vec<serde_json::Value> = self
                        .catalog
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|row| row["provider"] == serde_json::json!(provider))
                        .cloned()
                        .collect();
                    RpcReply::value(&serde_json::Value::Array(rows))
                }
                methods::RESET_PROVIDER_CATALOG => {
                    self.resets.lock().unwrap().push(params.clone());
                    self.catalog
                        .lock()
                        .unwrap()
                        .retain(|row| row["id"] != "acme/acme-custom");
                    RpcReply::value(&serde_json::json!({}))
                }
                methods::START_MODEL_SETUP_CHAT => {
                    let seq = self
                        .chat_seq
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    RpcReply::value(&serde_json::json!({ "chatId": format!("setup-chat-{seq}") }))
                }
                methods::LIST_API_DIALECTS => RpcReply::value(&serde_json::json!([
                    "anthropic-messages",
                    "openai-completions",
                    "openai-responses"
                ])),
                methods::SAVE_MODEL_RECORD => {
                    self.records.lock().unwrap().push(params.clone());
                    RpcReply::value(&serde_json::json!({}))
                }
                methods::LIST_HIDDEN_MODELS => {
                    RpcReply::value(&serde_json::Value::Array(Vec::new()))
                }
                methods::MUTATE => {
                    if params["op"].as_str() == Some("deleteChat") {
                        self.deleted_chats
                            .lock()
                            .unwrap()
                            .push(params["chatId"].as_str().unwrap_or_default().to_string());
                    }
                    RpcReply::value(&serde_json::json!({}))
                }
                methods::LIST_MODEL_PROPOSALS => RpcReply::value(&serde_json::Value::Array(
                    self.proposals.lock().unwrap().clone(),
                )),
                methods::APPLY_MODEL_PROPOSAL => {
                    if self.fail_applies.load(std::sync::atomic::Ordering::SeqCst) {
                        return Err(RpcError::Failed(
                            "the catalog changed since this proposal (it is now a no-op); \
                             run model_proposal again"
                                .into(),
                        ));
                    }
                    self.applies.lock().unwrap().push(params.clone());
                    let proposal_id = params["proposalId"].as_str().unwrap_or_default();
                    self.proposals
                        .lock()
                        .unwrap()
                        .retain(|proposal| proposal["id"] != serde_json::json!(proposal_id));
                    RpcReply::value(&serde_json::json!({ "applied": ["acme/acme-2"] }))
                }
                methods::DISCARD_MODEL_PROPOSAL => {
                    let proposal_id = params["proposalId"].as_str().unwrap_or_default();
                    self.proposals
                        .lock()
                        .unwrap()
                        .retain(|proposal| proposal["id"] != serde_json::json!(proposal_id));
                    RpcReply::value(&serde_json::json!({ "discarded": true }))
                }
                methods::DELETE_QUEUED_MESSAGE => {
                    let message_id = params["messageId"].as_str().unwrap_or_default();
                    let mut entries = self.queue.subscribe().borrow_and_update().clone();
                    if let Some(pending) = entries["pending"].as_array_mut() {
                        pending.retain(|item| item["messageId"] != serde_json::json!(message_id));
                    }
                    if entries["pending"].as_array().is_some_and(|p| p.is_empty()) {
                        entries["paused"] = serde_json::json!(false);
                    }
                    self.queue.send_replace(entries);
                    RpcReply::value(&serde_json::json!({}))
                }
                methods::QUEUE_COMMAND => {
                    self.queued.lock().unwrap().push(params.clone());
                    let prompt = params["command"]["request"]["prompt"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let turn_user_id = params["command"]["messageId"]
                        .as_str()
                        .unwrap_or("setup-turn")
                        .to_string();
                    if self
                        .fail_first_admission
                        .swap(false, std::sync::atomic::Ordering::SeqCst)
                    {
                        // The driver's settle: the head keeps its error, the
                        // queue pauses — nothing reaches the doc.
                        self.queue.send_replace(serde_json::json!({
                            "pending": [{
                                "messageId": turn_user_id,
                                "request": params["command"]["request"],
                                "kind": "ordinary",
                                "submittedAt": 0,
                                "error": "provider deepseek is not configured",
                            }],
                            "paused": true,
                            "activeMessageId": null,
                            "error": null,
                        }));
                        return RpcReply::value(&serde_json::json!({}));
                    }
                    let mut entries = self
                        .doc
                        .subscribe()
                        .borrow_and_update()
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    entries.push(entry_json(&turn_user_id, "user", &prompt, now_ms(), None));
                    entries.push(entry_json(
                        "setup-turn-assistant",
                        "assistant",
                        "Researching the catalog…",
                        now_ms(),
                        Some("streaming"),
                    ));
                    self.doc.send_replace(serde_json::json!(entries));
                    // The queue state is re-published untouched: whatever
                    // the sender left parked (an undeleted errored head)
                    // must re-surface on the strip, or the retry test
                    // cannot tell a real delete from a hidden one.
                    let queue = self.queue.subscribe().borrow_and_update().clone();
                    self.queue.send_replace(queue);
                    RpcReply::value(&serde_json::json!({}))
                }
                methods::WATCH_MESSAGE_QUEUE => {
                    let rx = self.queue.subscribe();
                    let stream =
                        futures::stream::unfold((rx, true), |(mut rx, first)| async move {
                            if !first && rx.changed().await.is_err() {
                                return None;
                            }
                            let frame = rx.borrow_and_update().clone();
                            Some((frame, (rx, false)))
                        });
                    Ok(RpcReply::Stream(stream.boxed()))
                }
                methods::WATCH_DOC_MESSAGES => {
                    let rx = self.doc.subscribe();
                    let stream =
                        futures::stream::unfold((rx, true), |(mut rx, first)| async move {
                            if !first && rx.changed().await.is_err() {
                                return None;
                            }
                            let entries = rx.borrow_and_update().clone();
                            Some((serde_json::json!({ "reset": entries }), (rx, false)))
                        });
                    Ok(RpcReply::Stream(stream.boxed()))
                }
                _ => Err(RpcError::UnknownMethod(method.to_string())),
            }
        }
    }

    struct SetupHarness<'a> {
        page: Entity<ProvidersPage>,
        state: Entity<AppState>,
        visual: &'a mut gpui::VisualTestContext,
        engine: std::sync::Arc<FakeSetupEngine>,
        runtime: tokio::runtime::Runtime,
        _dir: tempfile::TempDir,
    }

    impl SetupHarness<'_> {
        fn pump(&self) {
            for _ in 0..8 {
                self.runtime
                    .block_on(async { tokio::task::yield_now().await });
                self.visual.run_until_parked();
            }
        }

        fn click(&mut self, selector: &'static str) {
            let bounds = self
                .visual
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} renders"));
            self.visual
                .simulate_click(bounds.center(), Default::default());
            self.pump();
        }
    }

    fn setup_dialog_harness<'a>(cx: &'a mut gpui::TestAppContext) -> SetupHarness<'a> {
        setup_dialog_harness_with(cx, false)
    }

    fn setup_dialog_harness_with<'a>(
        cx: &'a mut gpui::TestAppContext,
        fail_first_admission: bool,
    ) -> SetupHarness<'a> {
        let (doc, _doc_rx) = tokio::sync::watch::channel(serde_json::json!([]));
        let (queue, _queue_rx) = tokio::sync::watch::channel(serde_json::json!({
            "pending": [],
            "paused": false,
            "activeMessageId": null,
            "error": null,
        }));
        let engine = std::sync::Arc::new(FakeSetupEngine {
            doc,
            queue,
            catalog: std::sync::Mutex::new(vec![
                serde_json::json!({
                    "id": "acme/acme-1",
                    "provider": "acme",
                    "label": "Acme 1",
                }),
                serde_json::json!({
                    "id": "acme/acme-custom",
                    "provider": "acme",
                    "label": "Acme custom",
                    "custom": true,
                }),
                serde_json::json!({
                    "id": "beta/beta-1",
                    "provider": "beta",
                    "label": "Beta 1",
                }),
            ]),
            resets: std::sync::Mutex::new(Vec::new()),
            queued: std::sync::Mutex::new(Vec::new()),
            fail_first_admission: std::sync::atomic::AtomicBool::new(fail_first_admission),
            proposals: std::sync::Mutex::new(Vec::new()),
            applies: std::sync::Mutex::new(Vec::new()),
            fail_applies: std::sync::atomic::AtomicBool::new(false),
            unconfigured: std::sync::atomic::AtomicBool::new(false),
            deleted_chats: std::sync::Mutex::new(Vec::new()),
            chat_seq: std::sync::atomic::AtomicUsize::new(0),
            records: std::sync::Mutex::new(Vec::new()),
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::default());
            crate::settings::init(crate::settings::UiSettings::default(), dir.path(), cx);
        });
        let app_state = cx.new(|_| AppState::new());
        let client = {
            let _guard = runtime.enter();
            holt_rpc::memory_client(engine.clone())
        };
        app_state.update(cx, |state, cx| state.attach_test_engine(client, cx));
        let (page, visual) =
            cx.add_window_view(|_window, cx| ProvidersPage::new(app_state.clone(), cx));
        let harness = SetupHarness {
            page,
            state: app_state,
            visual,
            engine,
            runtime,
            _dir: dir,
        };
        harness.pump();
        harness
    }

    /// Issue 03's repro: open the AI tab, send, and the placeholder must
    /// flip to the real transcript with the sent turn visible.
    #[gpui::test]
    fn the_setup_dialog_renders_the_sent_turn(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        let chat_id = harness.page.update(&mut *harness.visual, |page, _| {
            assert!(page.setup_transcript_view.is_some(), "the view mounts");
            page.setup_chat.clone().expect("the setup chat resolved")
        });
        // A fresh session: the doc starts empty, the placeholder shows.
        assert!(harness.visual.debug_bounds("setup-empty-state").is_some());
        assert!(
            harness.visual.debug_bounds("setup-queue-error").is_none(),
            "a healthy queue renders no failure strip"
        );

        // Send like the composer does: draft + submit.
        harness.page.update(&mut *harness.visual, |page, cx| {
            let input = page.setup_input.clone().expect("the composer input");
            input.update(cx, |input, cx| input.set_text("add acme-2", cx));
            page.send_setup_message(cx);
        });
        harness.pump();

        // The turn reached the engine addressed to the setup chat.
        let queued = harness.engine.queued.lock().unwrap().clone();
        assert_eq!(queued.len(), 1, "one QueueCommand");
        assert_eq!(queued[0]["chatId"], chat_id);
        let turn_user_id = queued[0]["command"]["messageId"]
            .as_str()
            .expect("the message id")
            .to_string();

        // The session view holds exactly the fresh turn.
        let rows = harness
            .visual
            .read(|cx| harness.state.read(cx).sub_transcript(&chat_id).to_vec());
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec![turn_user_id.as_str(), "setup-turn-assistant"],
            "the pump published the fresh turn"
        );

        // The placeholder unmounted and the view holds real rows.
        assert!(harness.visual.debug_bounds("setup-empty-state").is_none());
        let view_rows = harness.page.update(&mut *harness.visual, |page, cx| {
            page.setup_transcript_view
                .as_ref()
                .expect("the view")
                .read(cx)
                .rows()
                .to_vec()
        });
        assert!(
            view_rows
                .iter()
                .any(|row| row.id.as_ref().starts_with("setup-turn-assistant#p0")),
            "the turn renders as transcript rows: {:?}",
            view_rows
                .iter()
                .map(|row| row.id.clone())
                .collect::<Vec<_>>()
        );
    }

    /// The silent-failure hole (issue 03's persistence mechanism): the send
    /// RPC replies Ok, but the turn never admits — no doc entry, so the
    /// placeholder would sit forever. The queue frame's reason must render,
    /// and the retry the strip promises must actually clear the failure:
    /// the errored head is deleted before the re-send, or it would park the
    /// queue (and the strip) forever.
    #[gpui::test]
    fn a_failed_admission_surfaces_and_the_retry_clears_it(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness_with(cx, true);
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        let send = |harness: &mut SetupHarness, prompt: &str| {
            harness.page.update(&mut *harness.visual, |page, cx| {
                let input = page.setup_input.clone().expect("the composer input");
                input.update(cx, |input, cx| input.set_text(prompt, cx));
                page.send_setup_message(cx);
            });
            harness.pump();
        };

        // First send: the turn never admits — the placeholder is honest,
        // but the failure is no longer silent.
        send(&mut harness, "add deepseek-flash");
        assert!(harness.visual.debug_bounds("setup-empty-state").is_some());
        assert!(
            harness.visual.debug_bounds("setup-queue-error").is_some(),
            "the admission failure renders a strip"
        );
        let reason = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_queue
                .as_ref()
                .and_then(setup_queue_error)
                .unwrap_or_default()
        });
        assert_eq!(reason, "provider deepseek is not configured");

        // The retry: the errored head is deleted, the turn admits, the
        // strip is gone and the conversation renders.
        send(&mut harness, "add deepseek-flash");
        assert!(
            harness.visual.debug_bounds("setup-queue-error").is_none(),
            "the retry cleared the failure strip"
        );
        assert!(harness.visual.debug_bounds("setup-empty-state").is_none());
        let chat_id = harness
            .page
            .update(&mut *harness.visual, |page, _| page.setup_chat.clone());
        let rows = harness.visual.read(|cx| {
            harness
                .state
                .read(cx)
                .sub_transcript(chat_id.as_deref().expect("the setup chat"))
                .to_vec()
        });
        assert!(!rows.is_empty(), "the retried turn landed in the doc");
    }

    /// The picker's menu must clear the modal's scrim: deferred layers sort
    /// by priority, and the modal occludes at priority 2 — a priority-1
    /// dropdown paints under the scrim and its rows never receive clicks.
    #[gpui::test]
    fn the_model_menu_picks_from_above_the_modal(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        harness.click("setup-model-dropdown");
        let row = harness
            .visual
            .debug_bounds("setup-model-option-1")
            .expect("the menu row renders");
        harness
            .visual
            .simulate_click(row.center(), Default::default());
        harness.pump();

        let picked = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_selected_model.clone()
        });
        assert_eq!(picked.as_deref(), Some("acme/acme-custom"));
        // Picking starts the menu's exit (the row unmounts when the close
        // animation's reap timer lands — animation timing, not this test's
        // subject).
        let closing = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_model_menu.closing_since().is_some()
        });
        assert!(closing, "the pick closed the menu");
    }

    /// The picker groups the catalog by provider (the composer picker's
    /// rail): one configured aggregator contributes hundreds of models, and
    /// a flat list of them is unusable. The rail switches which provider's
    /// models the menu lists; picking sets the selection from that rail.
    #[gpui::test]
    fn the_model_menu_scopes_to_the_rail_provider(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        harness.click("setup-model-dropdown");
        // Two configured providers → two rail tabs; the list opens on the
        // selection's provider (acme) with only acme's models.
        assert!(harness.visual.debug_bounds("setup-model-rail-0").is_some());
        assert!(harness.visual.debug_bounds("setup-model-rail-1").is_some());
        assert!(
            harness
                .visual
                .debug_bounds("setup-model-option-1")
                .is_some()
        );
        assert!(
            harness
                .visual
                .debug_bounds("setup-model-option-2")
                .is_none(),
            "beta's model is not listed under acme"
        );

        // Switch the rail: beta's models replace acme's in the same menu.
        harness.click("setup-model-rail-1");
        assert!(
            harness
                .visual
                .debug_bounds("setup-model-option-0")
                .is_some()
        );
        assert!(
            harness
                .visual
                .debug_bounds("setup-model-option-1")
                .is_none(),
            "acme's second model left the list"
        );
        let row = harness
            .visual
            .debug_bounds("setup-model-option-0")
            .expect("the beta row renders");
        harness
            .visual
            .simulate_click(row.center(), Default::default());
        harness.pump();

        let picked = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_selected_model.clone()
        });
        assert_eq!(picked.as_deref(), Some("beta/beta-1"));
    }

    fn chat_with_config(id: &str, model: &str) -> holt_proto::Chat {
        let provider = model.split('/').next().unwrap_or("acme").to_string();
        holt_proto::Chat {
            id: id.into(),
            device_id: "dev".into(),
            title: None,
            title_source: Default::default(),
            title_task_started: false,
            archived: false,
            pinned: false,
            cwd: Some("/project".into()),
            branch: None,
            checkout_id: None,
            source_context: None,
            config: Some(holt_proto::ChatConfig {
                provider: holt_proto::ProviderId(provider.into()),
                model: model.into(),
                reasoning: None,
                model_options: Default::default(),
                permission_mode: Default::default(),
                scope: Default::default(),
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

    /// A global reset drops custom models while the selected chat's config
    /// keeps naming one — the dialog must not inherit a model the catalog
    /// no longer knows (the chip would show a ghost and every send would
    /// fail admission).
    #[gpui::test]
    fn a_ghost_default_falls_back_to_the_catalog(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness.state.update(&mut *harness.visual, |state, _| {
            state
                .chats
                .push(chat_with_config("chat-1", "acme/acme-ghost"));
            state.selected_chat = Some("chat-1".into());
        });
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        let picked = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_selected_model.clone()
        });
        assert_eq!(
            picked.as_deref(),
            Some("acme/acme-1"),
            "the ghost default falls back to a servable catalog entry"
        );
    }

    /// The last pick must survive a dialog reopen (design-v2 decision 1):
    /// re-inheriting the selected chat's config every open made the pick
    /// look like it never stuck.
    #[gpui::test]
    fn the_last_pick_survives_a_dialog_reopen(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness.state.update(&mut *harness.visual, |state, _| {
            state
                .chats
                .push(chat_with_config("chat-1", "acme/acme-custom"));
            state.selected_chat = Some("chat-1".into());
        });
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();
        // The first open inherits the selected chat's model.
        let picked = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_selected_model.clone()
        });
        assert_eq!(picked.as_deref(), Some("acme/acme-custom"));

        // Pick the OTHER catalog entry, close, reopen.
        harness.click("setup-model-dropdown");
        let row = harness
            .visual
            .debug_bounds("setup-model-option-0")
            .expect("the menu row renders");
        harness
            .visual
            .simulate_click(row.center(), Default::default());
        harness.pump();
        harness.click("add-provider-close");
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        let picked = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_selected_model.clone()
        });
        assert_eq!(
            picked.as_deref(),
            Some("acme/acme-1"),
            "the reopen keeps the pick instead of re-inheriting the chat config"
        );
    }

    /// The full story the user hit (2026-09-18): the selected chat runs a
    /// custom model, the dialog inherits it, a global reset drops the
    /// custom entry — the reopened dialog must fall back to a servable
    /// model instead of pinning the ghost.
    #[gpui::test]
    fn a_reset_custom_model_does_not_ghost_the_reopened_dialog(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness.state.update(&mut *harness.visual, |state, _| {
            state
                .chats
                .push(chat_with_config("chat-1", "acme/acme-custom"));
            state.selected_chat = Some("chat-1".into());
        });
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();
        // Pre-reset the inherited custom model is a legitimate pick.
        let picked = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_selected_model.clone()
        });
        assert_eq!(picked.as_deref(), Some("acme/acme-custom"));

        // Close, reset globally (the page's two-step confirm), reopen.
        harness.click("add-provider-close");
        harness.click("reset-all-providers");
        harness.click("reset-all-confirm");
        assert_eq!(
            harness.engine.resets.lock().unwrap().len(),
            1,
            "the confirm fires the reset RPC"
        );
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        let picked = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_selected_model.clone()
        });
        assert_eq!(
            picked.as_deref(),
            Some("acme/acme-1"),
            "the reset custom model no longer pins the chip"
        );
    }

    /// A stored proposal as the engine's `proposal_views` serves it.
    fn proposal_json(id: &str, created_at: i64) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "summary": "Apply 1 catalog change for acme",
            "createdAt": created_at,
            "changes": [{
                "action": "upsert_model_record",
                "providerId": "acme",
                "modelId": "acme-2",
                "record": {
                    "id": "acme-2",
                    "name": "Acme 2",
                    "api": "openai-completions",
                    "provider": "acme",
                    "baseUrl": "https://acme.example/v1",
                    "reasoning": false,
                    "input": ["text"],
                    "cost": { "input": 1.5, "output": 3.0, "cacheRead": 0.0, "cacheWrite": 0.0 },
                    "contextWindow": 200_000,
                    "maxTokens": 8_192,
                },
            }],
        })
    }

    /// The write decision shows its own data: each pending card carries the
    /// structured change rows (endpoint, window, cost) beside the summary,
    /// and a successful Write lands as a terminal "written" card instead
    /// of silently vanishing.
    #[gpui::test]
    fn the_review_panel_renders_change_rows_and_writes(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness
            .engine
            .proposals
            .lock()
            .unwrap()
            .push(proposal_json("prop-1", now_ms() + 30_000));
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        let (card, row) = (
            harness.visual.debug_bounds("setup-proposal-card-0"),
            harness.visual.debug_bounds("setup-change-row-0-0"),
        );
        assert!(card.is_some(), "the proposal card renders");
        assert!(row.is_some(), "the change's structured row renders");

        harness.click("setup-proposal-apply-0");
        assert_eq!(harness.engine.applies.lock().unwrap().len(), 1);
        assert!(
            harness
                .visual
                .debug_bounds("setup-applied-card-0")
                .is_some(),
            "the write lands as a visible terminal card"
        );
        assert!(
            harness
                .visual
                .debug_bounds("setup-proposal-card-0")
                .is_none(),
            "the written proposal stops offering Write"
        );
    }

    /// An apply failure is a property of the proposal (usually the
    /// staleness gate), not a page fault: it renders inline on the card it
    /// belongs to, with the Write still available for a retry after the
    /// assistant re-proposes — never as the window-top modal.
    #[gpui::test]
    fn an_apply_failure_lands_inline_on_the_card(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness
            .engine
            .fail_applies
            .store(true, std::sync::atomic::Ordering::SeqCst);
        harness
            .engine
            .proposals
            .lock()
            .unwrap()
            .push(proposal_json("prop-1", now_ms() + 30_000));
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        harness.click("setup-proposal-apply-0");
        assert!(
            harness
                .visual
                .debug_bounds("setup-proposal-error-0")
                .is_some(),
            "the failure renders on the card"
        );
        assert!(
            harness
                .visual
                .debug_bounds("setup-proposal-card-0")
                .is_some(),
            "the card stays for a retry"
        );
        let inline = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_apply_errors.get("prop-1").cloned()
        });
        assert!(
            inline
                .unwrap_or_default()
                .contains("changed since this proposal"),
            "the inline error carries the engine's reason"
        );
    }

    /// The setup chat is session-scoped: closing the dialog deletes it
    /// (turn cancelled, transcript and proposals dropped with it), and the
    /// next open starts a fresh chat — no conversation memory persists.
    #[gpui::test]
    fn closing_the_dialog_deletes_the_setup_chat(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();
        let first = harness
            .page
            .update(&mut *harness.visual, |page, _| page.setup_chat.clone())
            .expect("the setup chat resolved");

        harness.click("add-provider-close");
        assert_eq!(
            harness.engine.deleted_chats.lock().unwrap().as_slice(),
            &[first.clone()],
            "closing the dialog deletes the session's chat"
        );

        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();
        let second = harness
            .page
            .update(&mut *harness.visual, |page, _| page.setup_chat.clone())
            .expect("the setup chat resolved");
        assert_ne!(first, second, "the reopen starts a fresh chat");
    }

    /// The record form's API dialect is a dropdown over the engine's
    /// registered dialects, not a free-text field: the pick lands in the
    /// saved record.
    #[gpui::test]
    fn the_record_form_picks_the_api_dialect_from_a_dropdown(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        // The panel clips its content behind the expand animation's height
        // (wall-clock driven — a parked test clock never finishes it), so
        // the rows' hitboxes stay clipped away. Reduce motion: animations
        // complete on the first layout.
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("toggle-record-form");

        // The dropdown defaults to openai-completions.
        let api = harness.page.update(&mut *harness.visual, |page, _| {
            page.record_form.as_ref().map(|form| form.api.clone())
        });
        assert_eq!(api.as_deref(), Some("openai-completions"));

        harness.click("record-api-dropdown");
        harness.click("record-api-option-anthropic-messages");
        let api = harness.page.update(&mut *harness.visual, |page, _| {
            page.record_form.as_ref().map(|form| form.api.clone())
        });
        assert_eq!(api.as_deref(), Some("anthropic-messages"));

        harness.page.update(&mut *harness.visual, |page, cx| {
            let form = page.record_form.as_ref().expect("the record form");
            for (key, value) in [
                ("id", "acme-9"),
                ("contextWindow", "200000"),
                ("maxTokens", "8192"),
            ] {
                form.inputs[key].update(cx, |input, cx| input.set_text(value, cx));
            }
        });
        harness.click("save-record");
        harness.pump();

        let records = harness.engine.records.lock().unwrap().clone();
        assert_eq!(records.len(), 1, "one SaveModelRecord");
        assert_eq!(records[0]["record"]["api"], "anthropic-messages");
    }

    /// The fresh-install dead end: no provider configured means the setup
    /// assistant has no model to run on. The tab must offer the way out —
    /// the manual form — not just the dead-end error strip.
    #[gpui::test]
    fn the_manual_tab_escape_hatch_when_no_provider_is_configured(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness
            .engine
            .unconfigured
            .store(true, std::sync::atomic::Ordering::SeqCst);
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        assert!(
            harness
                .visual
                .debug_bounds("setup-bootstrap-manual")
                .is_some(),
            "the dead end offers the manual form"
        );
        harness.click("setup-bootstrap-manual");
        let tab = harness
            .page
            .update(&mut *harness.visual, |page, _| page.add_dialog);
        assert_eq!(
            tab,
            Some(AddProviderTab::Manual),
            "the escape hatch switches to the manual tab"
        );
    }
}
