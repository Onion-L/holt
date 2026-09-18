use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use gpui::{
    AnyElement, Context, Entity, EventEmitter, IntoElement, Render, SharedString, Task, Window,
    div, prelude::*, px,
};
use holt_proto::{Model, Provider};
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
/// (key, label, placeholder). Laid out two per row; `baseUrl` spans.
const RECORD_FIELDS: [(&str, &str, &str); 10] = [
    ("id", "Model ID", "acme-1"),
    ("name", "Name", "Acme 1"),
    ("baseUrl", "Base URL", "https://acme.example/v1"),
    ("api", "API dialect", "openai-completions"),
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
    model_inputs: HashMap<String, Entity<ComposerInput>>,
    /// Per-variant hint for a rejected "Add model" attempt (duplicate ID,
    /// engine rejection) — small inline text, not the page error strip.
    model_errors: HashMap<String, String>,
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
    /// Hidden rows per variant, loaded beside the model list.
    hidden: HashMap<String, Loadable<Vec<HiddenModel>>>,
    /// Two-step reset confirmations: the first click arms, the second
    /// executes. Per provider (variant id) and global.
    armed_reset: Option<String>,
    armed_reset_all: bool,
    /// The AI tab (V2c): the hidden setup chat's id (kept across dialog
    /// opens), its transcript rows, the picker's catalog and popup, the
    /// composer input, the review panel's proposals, and the watch task
    /// keeping transcript and proposals current while the dialog lives.
    setup_chat: Option<String>,
    setup_transcript: Vec<serde_json::Value>,
    setup_models: Loadable<Vec<Model>>,
    setup_model_menu: Popup<()>,
    setup_selected_model: Option<String>,
    setup_input: Option<Entity<ComposerInput>>,
    setup_proposals: Vec<serde_json::Value>,
    setup_task: Option<Task<()>>,
    task: Option<Task<()>>,
    collapse_task: Option<Task<()>>,
}

impl ProvidersPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            providers: Loadable::Idle,
            expanded: None,
            collapsing: None,
            panel_epochs: HashMap::new(),
            selected_variant: HashMap::new(),
            inputs: HashMap::new(),
            models: HashMap::new(),
            model_inputs: HashMap::new(),
            model_errors: HashMap::new(),
            model_tasks: HashMap::new(),
            revealed: HashSet::new(),
            add_dialog: None,
            new_provider_inputs: HashMap::new(),
            new_provider_error: None,
            record_form: None,
            hidden: HashMap::new(),
            armed_reset: None,
            armed_reset_all: false,
            setup_chat: None,
            setup_transcript: Vec::new(),
            setup_models: Loadable::Idle,
            setup_model_menu: Popup::default(),
            setup_selected_model: None,
            setup_input: None,
            setup_proposals: Vec::new(),
            setup_task: None,
            task: None,
            collapse_task: None,
        };
        page.load(cx);
        page
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
        self.model_inputs
            .entry(variant_id.to_string())
            .or_insert_with(|| cx.new(|cx| ComposerInput::new("Model ID", cx)));
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

    /// Retire the per-panel transients: the record form and any armed reset
    /// belong to the variant the panel was showing.
    fn close_panel_forms(&mut self) {
        self.record_form = None;
        self.armed_reset = None;
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

    fn add_model(&mut self, provider: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let model = self
            .model_inputs
            .get(&provider)
            .map(|input| input.read(cx).text().trim().to_string())
            .unwrap_or_default();
        if model.is_empty() {
            self.model_errors
                .insert(provider, "Model ID is required".to_string());
            cx.notify();
            return;
        }
        self.model_errors.remove(&provider);
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::ADD_PROVIDER_MODEL,
                    serde_json::json!({"providerId": provider, "modelId": model}),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.model_errors.remove(&provider);
                        if let Some(input) = page.model_inputs.get(&provider) {
                            input.update(cx, |input, cx| input.set_text("", cx));
                        }
                        crate::pickers::bump_provider_catalog(cx);
                        page.load_models(&provider, true, cx);
                    }
                    Err(error) => {
                        page.model_errors
                            .insert(provider.clone(), error.to_string());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
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
        self.record_form = Some(RecordForm {
            provider,
            inputs,
            reasoning: false,
            image: false,
            error: None,
        });
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
        cx.notify();
    }

    // -- The AI tab (V2c) --------------------------------------------------

    /// The default setup model: the selected chat's config. The async
    /// preparation falls back to the first configured provider when no
    /// chat carries a config.
    fn default_setup_model(&self, cx: &Context<Self>) -> Option<(String, String)> {
        let config = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|chat| chat.config.clone())?;
        Some((config.provider.0.clone(), config.model.clone()))
    }

    /// Prepares the AI tab: ensures the singleton setup chat, loads the
    /// picker catalog, and starts the transcript watch. Idempotent — an
    /// already-prepared tab only re-subscribes.
    fn prepare_setup(&mut self, cx: &mut Context<Self>) {
        if self.setup_input.is_none() {
            self.setup_input = Some(
                cx.new(|cx| ComposerInput::new("Which provider or model should be set up?", cx)),
            );
        }
        if self.setup_chat.is_some() && self.setup_task.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if self.setup_chat.is_none() {
            self.setup_models = Loadable::Loading;
        }
        let selected_default = self.default_setup_model(cx);
        self.task = Some(cx.spawn(async move |this, cx| {
            // Resolve the model: the selected chat's config, else the first
            // configured provider's first model.
            let (provider, model) = match selected_default {
                Some(pair) => pair,
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
            let chat_id = match engine
                .client()
                .call(
                    methods::ENSURE_MODEL_SETUP_CHAT,
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
            let models = configured_model_catalog(&engine).await;
            let mut rx = engine
                .client()
                .subscribe(
                    methods::WATCH_DOC_MESSAGES,
                    serde_json::json!({ "chatId": chat_id }),
                )
                .await
                .ok();
            let first = match rx.as_mut() {
                Some(rx) => rx.recv().await,
                None => None,
            };
            let proposals = setup_proposal_rows(&engine, &chat_id).await;
            let watch_engine = engine.clone();
            let watch_chat_id = chat_id.clone();
            this.update(cx, |page, cx| {
                page.setup_chat = Some(chat_id);
                page.setup_selected_model = Some(format!("{provider}/{model}"));
                page.setup_models = Loadable::Ready(models);
                page.setup_transcript = transcript_reset_rows(first.as_ref());
                page.setup_proposals = proposals;
                if let Some(rx) = rx {
                    page.start_setup_watch(rx, watch_engine, watch_chat_id, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The watch loop: every transcript frame refreshes the mini chat and
    /// the review panel's proposals.
    fn start_setup_watch(
        &mut self,
        mut rx: tokio::sync::mpsc::Receiver<serde_json::Value>,
        engine: crate::state::EngineHandle,
        chat_id: String,
        cx: &mut Context<Self>,
    ) {
        self.setup_task = Some(cx.spawn(async move |this, cx| {
            while let Some(frame) = rx.recv().await {
                let proposals = setup_proposal_rows(&engine, &chat_id).await;
                let rows = transcript_reset_rows(Some(&frame));
                let alive = this
                    .update(cx, |page, cx| {
                        if page.setup_chat.as_deref() != Some(chat_id.as_str()) {
                            return false;
                        }
                        page.setup_transcript = rows;
                        page.setup_proposals = proposals;
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        }));
    }

    fn pick_setup_model(&mut self, qualified: String, cx: &mut Context<Self>) {
        self.close_setup_model_menu(cx);
        let Some((provider, model)) = qualified
            .split_once('/')
            .map(|(provider, model)| (provider.to_string(), model.to_string()))
        else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.setup_selected_model = Some(qualified);
        self.task = Some(cx.spawn(async move |this, cx| {
            let _ = engine
                .client()
                .call(
                    methods::ENSURE_MODEL_SETUP_CHAT,
                    serde_json::json!({ "provider": provider, "model": model }),
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
        let Some((provider, model)) = selected
            .split_once('/')
            .map(|(provider, model)| (provider.to_string(), model.to_string()))
        else {
            return;
        };
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
        self.task = Some(cx.spawn(async move |this, cx| {
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
                                "model": model,
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
        }));
        cx.notify();
    }

    /// The review panel's write button: applies a stored proposal, then
    /// refreshes everything the page shows.
    fn apply_setup_proposal(&mut self, proposal_id: String, cx: &mut Context<Self>) {
        let Some(chat_id) = self.setup_chat.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::APPLY_MODEL_PROPOSAL,
                    serde_json::json!({ "chatId": chat_id, "proposalId": proposal_id }),
                )
                .await;
            this.update(cx, |page, cx| match result {
                Ok(_) => {
                    crate::pickers::bump_provider_catalog(cx);
                    page.load(cx);
                    page.refresh_setup_proposals(cx);
                }
                Err(error) => page.fail(error.to_string(), cx),
            })
            .ok();
        }));
    }

    fn discard_setup_proposal(&mut self, proposal_id: String, cx: &mut Context<Self>) {
        let Some(chat_id) = self.setup_chat.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
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
        self.task = Some(cx.spawn(async move |this, cx| {
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
                        page.add_dialog = None;
                        page.new_provider_error = None;
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
            self.armed_reset_all = false;
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

    /// Two-step global reset: first click arms, second executes.
    fn arm_or_reset_all(&mut self, cx: &mut Context<Self>) {
        if self.armed_reset_all {
            self.armed_reset_all = false;
            self.reset_all(cx);
        } else {
            self.armed_reset_all = true;
            self.armed_reset = None;
            cx.notify();
        }
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
                    let model_input = self.model_inputs.get(&variant_id).cloned();
                    let model_error = self.model_errors.get(&variant_id).cloned();
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
                    let record_open = self
                        .record_form
                        .as_ref()
                        .is_some_and(|form| form.provider == variant_id);
                    let revealed = self.revealed.contains(&variant_id);
                    let panel_height = provider_controls_height(
                        &models,
                        hidden_count,
                        provider.variants.len() > 1,
                        model_error.is_some(),
                        record_open,
                    );
                    let panel_epoch = self.panel_epochs.get(&id).copied().unwrap_or_default();
                    let save_id = variant_id.clone();
                    let remove_id = variant_id.clone();
                    let add_model_id = variant_id.clone();
                    let model_count = models.ready().map(|models| models.len());
                    let danger = theme.danger;
                    let danger_muted = theme.danger_muted;
                    let model_list =
                        provider_model_list(index, &variant_id, models, &theme, cx.entity(), cx);
                    let hidden_list = hidden_rows(index, &variant_id, hidden, &theme, cx);
                    let record_section = record_section(
                        index,
                        &variant_id,
                        record_open,
                        self.record_form.as_ref(),
                        &theme,
                        cx,
                    );
                    let danger_row = panel_danger_row(
                        index,
                        &variant_id,
                        provider.custom,
                        self.armed_reset.as_deref() == Some(variant_id.as_str()),
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
                                        .items_baseline()
                                        .gap(px(8.0))
                                        .child(widgets::field_label(&theme, "Models"))
                                        .children(model_count.map(|count| {
                                            div()
                                                .text_size(crate::typography::ui_rems(11.0))
                                                .text_color(theme.text_muted.opacity(0.7))
                                                .child(SharedString::from(format!("{count}")))
                                                .into_any_element()
                                        })),
                                )
                                .child(model_list)
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(8.0))
                                        .children(model_input.map(|input| {
                                            bordered_input(&theme, input)
                                                .flex_1()
                                                .min_w_0()
                                                .into_any_element()
                                        }))
                                        .child(
                                            action_button(&theme)
                                                .id(("add-provider-model", index))
                                                .hover(|style| style.bg(crate::theme::ink(0.04)))
                                                .on_click(cx.listener(move |page, _, _, cx| {
                                                    page.add_model(add_model_id.clone(), cx)
                                                }))
                                                .child("Add model"),
                                        ),
                                )
                                .children(model_error.map(|message| {
                                    div()
                                        .text_size(crate::typography::ui_rems(11.0))
                                        .text_color(theme.danger_muted.opacity(0.9))
                                        .child(SharedString::from(message))
                                })),
                        )
                        .children(hidden_list)
                        .child(record_section)
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
                ];
                div()
                    .flex()
                    .flex_col()
                    .child(
                        widgets::flat_row()
                            .id(("provider-row", index))
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
                    .child(top_action_row(self.armed_reset_all, &theme, cx))
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
        // The Add Provider dialog rides a deferred layer, so the page can
        // host it directly (the appearance library's review dialog does the
        // same).
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
    variants: bool,
    hint: bool,
    record_open: bool,
) -> f32 {
    let list_height = match models {
        Loadable::Idle | Loadable::Loading => 138.0,
        Loadable::Error(_) => 40.0,
        Loadable::Ready(models) if models.is_empty() => 32.0,
        Loadable::Ready(models) => (models.len() as f32 * 32.0).min(192.0),
    };
    let hidden_height = if hidden_count > 0 {
        30.0 + (hidden_count as f32 * 32.0).min(96.0)
    } else {
        0.0
    };
    // The hint adds one 11px text line plus the section's 8px flex gap; the
    // key section is label + full-width input + its own Save/Remove row.
    // The record expander row and the danger row are always mounted; the
    // form and the hidden block add their own heights when present.
    224.0
        + list_height
        + hidden_height
        + if variants { 34.0 } else { 0.0 }
        + if hint { 24.0 } else { 0.0 }
        + 44.0
        + if record_open { RECORD_FORM_HEIGHT } else { 0.0 }
        + 44.0
}

/// The mounted record form's rendered height — the animated panel clips via
/// overflow-hidden, so this must cover the tallest state (all rows, the
/// advanced textarea, one error line, the action row).
const RECORD_FORM_HEIGHT: f32 = 468.0;

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

/// The record-form expander row plus, when open, the form itself. Basic
/// fields up front; `thinkingLevelMap`/`compat`/`headers` ride the advanced
/// JSON textarea — a full structured editor is not worth the surface.
fn record_section(
    index: usize,
    provider_id: &str,
    open: bool,
    form: Option<&RecordForm>,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let toggle_provider = provider_id.to_string();
    let mut section = div().flex().flex_col().gap(px(8.0)).child(
        action_button(theme)
            .id(("toggle-record-form", index))
            .w_auto()
            .hover(|style| style.bg(crate::theme::ink(0.04)))
            .on_click(cx.listener(move |page, _, _, cx| {
                if page
                    .record_form
                    .as_ref()
                    .is_some_and(|form| form.provider == toggle_provider)
                {
                    page.record_form = None;
                    cx.notify();
                } else {
                    page.open_record_form(toggle_provider.clone(), cx);
                }
            }))
            .child(if open {
                "Close record form"
            } else {
                "Add model record"
            }),
    );
    let Some(form) = form else {
        return section.into_any_element();
    };
    let field_input = |key: &str| form.inputs.get(key).cloned();
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
    let labelled = |key: &str, label: &str| {
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(widgets::field_label(theme, label))
            .children(
                field_input(key)
                    .map(|input| bordered_input(theme, input).w_full().into_any_element()),
            )
    };
    let flag_row = |key: &'static str, label: &str, on: bool| {
        div()
            .id((SharedString::from(format!("record-flag-{key}")), index))
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
    let save_click = cx.listener(move |page: &mut ProvidersPage, _, _, cx| {
        page.save_record(cx);
    });
    section = section
        .child(pair(("id", "Model ID"), Some(("name", "Name"))))
        .child(labelled("baseUrl", "Base URL"))
        .child(labelled("api", "API dialect"))
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
                        .id(("record-advanced", index))
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
                        .id(("save-record", index))
                        .hover(|style| style.bg(crate::theme::ink(0.04)))
                        .on_click(save_click)
                        .child("Save record"),
                )
                .child(
                    widgets::ghost_action(theme)
                        .id(("cancel-record", index))
                        .hover(move |style| widgets::ghost_hover(theme, style))
                        .on_click(cx.listener(move |page: &mut ProvidersPage, _, _, cx| {
                            page.record_form = None;
                            cx.notify();
                        }))
                        .child("Cancel"),
                ),
        );
    section.into_any_element()
}

/// The panel's bottom row: reset to the catalog (two-step), and for custom
/// providers the definition removal.
fn panel_danger_row(
    index: usize,
    provider_id: &str,
    custom: bool,
    armed: bool,
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
                    page.remove_custom_provider(remove_provider.clone(), remove_org.clone(), cx)
                }))
                .child("Remove provider")
        }))
        .into_any_element()
}

/// The page's top action row (design-v2): the Add Provider primary on the
/// left, the global reset (two-step) on the right. The reset stays a compact
/// button — a spacer pushes it right; stretching the button itself would
/// paint its hover/armed wash across the whole row.
fn top_action_row(armed: bool, theme: &Theme, cx: &mut Context<ProvidersPage>) -> AnyElement {
    let danger = theme.danger;
    let danger_muted = theme.danger_muted;
    let hover_theme = theme.clone();
    let mut reset = widgets::ghost_action(theme)
        .id("reset-all-providers")
        .on_click(cx.listener(move |page, _, _, cx| page.arm_or_reset_all(cx)))
        .child(if armed {
            "Confirm reset ALL providers — keeps keys, drops every user-written entry"
        } else {
            "Reset all providers"
        });
    reset = if armed {
        // The armed state reads without hovering: a persistent danger tint.
        reset
            .bg(danger.opacity(0.10))
            .text_color(danger_muted)
            .hover(move |style| style.bg(danger.opacity(0.16)).text_color(danger_muted))
    } else {
        reset.hover(move |style| style.bg(danger.opacity(0.10)).text_color(danger_muted))
    };
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .pb(px(6.0))
        .child(
            action_button(theme)
                .id("open-add-provider")
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
        .child(div().flex_1())
        .child(reset)
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
    let mut card = popover::dialog_card(theme)
        .w(if tab == AddProviderTab::Ai {
            px(620.0)
        } else {
            px(560.0)
        })
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
    let transcript = page.setup_transcript.clone();
    let proposals = page.setup_proposals.clone();
    let models_ready = matches!(page.setup_models, Loadable::Ready(_));
    let column = div().flex().flex_col().gap(px(10.0));
    // The model row: picker (or why it is unavailable).
    let model_row = match &page.setup_models {
        Loadable::Error(reason) => div()
            .text_size(crate::typography::ui_rems(11.5))
            .text_color(theme.danger_muted.opacity(0.9))
            .child(SharedString::from(reason.clone()))
            .into_any_element(),
        _ => setup_model_picker(page, theme, cx),
    };
    let mut column = column.child(model_row);
    if models_ready || page.setup_chat.is_some() {
        column = column
            .child(setup_transcript(&transcript, theme))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .children(input.map(|input| {
                        bordered_input(theme, input)
                            .flex_1()
                            .min_w_0()
                            .into_any_element()
                    }))
                    .child(
                        action_button(theme)
                            .id("setup-send")
                            .hover(|style| style.bg(crate::theme::ink(0.04)))
                            .on_click(cx.listener(|page, _, _, cx| page.send_setup_message(cx)))
                            .child("Send"),
                    ),
            )
            .child(setup_review_panel(&proposals, theme, cx));
    }
    column.into_any_element()
}

/// The compact model dropdown (the title-settings picker's pattern).
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
    let mut rows = Vec::new();
    for (index, model) in catalog.iter().enumerate() {
        let qualified = model.id.clone();
        let selected = selected_id.as_deref() == Some(model.id.as_str());
        rows.push(
            popover::menu_row(theme, selected, format!("setup-model-option-{index}"))
                .id(SharedString::from(format!("setup-model-option-{index}")))
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
                        .child(SharedString::from(model.id.clone())),
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
    let menu = popover::popover_card(theme)
        .id("setup-model-scroll")
        .w(px(420.0))
        .max_h(px(280.0))
        .overflow_y_scroll()
        .occlude()
        .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_setup_model_menu(cx)))
        .flex()
        .flex_col()
        .gap(px(2.0))
        .children(rows)
        .into_any_element();
    let selected_label = SharedString::from(
        selected_id
            .clone()
            .unwrap_or_else(|| "Select a model".into()),
    );
    let border = if page.setup_model_menu.is_open() {
        theme.border_strong
    } else {
        theme.border
    };
    div()
        .id("setup-model-dropdown")
        .relative()
        .w(px(300.0))
        .h(px(30.0))
        .px(px(10.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(border)
        .bg(theme.input_glass_bg())
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .cursor_pointer()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|page, _, _, _| page.setup_model_menu.note_trigger_press()),
        )
        .on_click(cx.listener(|page, _, _, cx| page.toggle_setup_model_menu(cx)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(theme.font_mono.clone())
                .text_size(crate::typography::ui_rems(11.0))
                .child(selected_label),
        )
        .child(
            crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                .size(px(14.0))
                .flex_none()
                .text_color(theme.text_muted),
        )
        .when_some(page.setup_model_menu.get(), |trigger, _| {
            trigger.child(popover::anchored_menu_below(
                "setup-model-menu",
                menu,
                page.setup_model_menu.closing_since(),
            ))
        })
        .into_any_element()
}

/// The mini transcript: text rows and tool chips only (the fixed flow's
/// three row kinds — design-v2's "简洁" contract).
fn setup_transcript(entries: &[serde_json::Value], theme: &Theme) -> AnyElement {
    let mut rows: Vec<AnyElement> = Vec::new();
    for entry in entries {
        let role = entry["role"].as_str().unwrap_or_default();
        for part in entry["parts"].as_array().unwrap_or(&Vec::new()) {
            if let Some(text) = part["text"].as_str().filter(|text| !text.is_empty()) {
                let user = role == "user";
                rows.push(
                    div()
                        .id(("setup-row-text", rows.len()))
                        .max_w(px(480.0))
                        .px(px(10.0))
                        .py(px(6.0))
                        .rounded(px(8.0))
                        .when(user, |row| row.bg(crate::theme::ink(0.05)))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(if user { theme.text } else { theme.text_muted })
                        .child(SharedString::from(text.to_string()))
                        .into_any_element(),
                );
            } else if part["call"].is_object() {
                let error = part["isError"].as_bool() == Some(true);
                rows.push(
                    div()
                        .id(("setup-row-tool", rows.len()))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .px(px(10.0))
                        .py(px(3.0))
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(if error {
                            theme.danger_muted.opacity(0.5)
                        } else {
                            theme.border
                        })
                        .font_family(theme.font_mono.clone())
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(if error {
                            theme.danger_muted
                        } else {
                            theme.text_muted
                        })
                        .child(format!("{}()", tool_display_name(&part["call"])))
                        .into_any_element(),
                );
            } else if let Some(notice) = part["message"].as_str() {
                rows.push(
                    div()
                        .id(("setup-row-notice", rows.len()))
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_muted.opacity(0.7))
                        .child(SharedString::from(notice.to_string()))
                        .into_any_element(),
                );
            }
        }
    }
    let list = if rows.is_empty() {
        div()
            .py(px(10.0))
            .text_size(crate::typography::ui_rems(11.5))
            .text_color(theme.text_muted.opacity(0.8))
            .child(
                "Nothing yet — tell the assistant which provider or model to set up. \
                 It researches the official docs, compares the local catalog, and \
                 prepares a proposal for the review panel below.",
            )
            .into_any_element()
    } else {
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .children(rows)
            .into_any_element()
    };
    div()
        .id("setup-transcript")
        .h(px(240.0))
        .p(px(4.0))
        .overflow_y_scroll()
        // ADR-0013: the page scrolls too — occlude or one wheel moves both.
        .occlude()
        .child(list)
        .into_any_element()
}

/// A tool chip's name: the engine's decode keeps known shapes as `kind`
/// (`webFetch`) and unknown ones as `name`; the agent-facing spelling wins.
fn tool_display_name(call: &serde_json::Value) -> String {
    let raw = call
        .get("name")
        .or_else(|| call.get("kind"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("tool");
    match raw {
        "webFetch" => "web_fetch".to_string(),
        "webSearch" => "web_search".to_string(),
        other => other.to_string(),
    }
}

/// The review panel: one row per stored proposal — summary, write, discard.
fn setup_review_panel(
    proposals: &[serde_json::Value],
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let danger = theme.danger;
    let danger_muted = theme.danger_muted;
    let mut panel = div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(widgets::field_label(theme, "Pending proposals"));
    if proposals.is_empty() {
        panel = panel.child(
            div()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_muted.opacity(0.7))
                .child("None yet — a proposal appears here once the assistant prepares one."),
        );
    }
    for (index, proposal) in proposals.iter().enumerate() {
        let id = proposal["id"].as_str().unwrap_or_default().to_string();
        let summary = proposal["summary"].as_str().unwrap_or_default().to_string();
        let apply_id = id.clone();
        let discard_id = id.clone();
        panel = panel.child(
            div()
                .id(("setup-proposal", index))
                .flex()
                .items_center()
                .gap(px(8.0))
                .p(px(8.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border)
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
                        .hover(|style| style.bg(crate::theme::ink(0.04)))
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.apply_setup_proposal(apply_id.clone(), cx)
                        }))
                        .child("Write"),
                )
                .child(
                    widgets::ghost_action(theme)
                        .id(("setup-proposal-discard", index))
                        .hover(move |style| style.bg(danger.opacity(0.10)).text_color(danger_muted))
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.discard_setup_proposal(discard_id.clone(), cx)
                        }))
                        .child("Discard"),
                ),
        );
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
    let bare = first
        .id
        .strip_prefix(&format!("{}/", provider.id.0))
        .unwrap_or(&first.id);
    Some((provider.id.0.clone(), bare.to_string()))
}

/// A transcript frame's reset rows.
fn transcript_reset_rows(frame: Option<&serde_json::Value>) -> Vec<serde_json::Value> {
    frame
        .and_then(|frame| frame.get("reset"))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
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
fn new_provider_problem(id: &str, base_url: &str, default_api: &str) -> Option<String> {
    if id.is_empty() {
        return Some("Provider id is required".into());
    }
    if id.contains('/') {
        return Some("Provider id cannot contain '/'".into());
    }
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Some("Base URL must start with http:// or https://".into());
    }
    if default_api.is_empty() {
        return Some("Default API dialect is required".into());
    }
    None
}

/// Builds the complete model record the engine expects from the form's
/// texts: numbers parse, costs default to zero, and the advanced JSON (when
/// present) must be an object whose keys ride along — except the form's own
/// fields, which the advanced object can never override.
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
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
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
        "baseUrl": base_url,
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
                // carries only what the form does not own.
                if ![
                    "id",
                    "name",
                    "api",
                    "provider",
                    "baseUrl",
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

/// The hidden block: one greyed row per hidden model with an unhide action.
/// `None` when the provider hides nothing.
fn hidden_rows(
    index: usize,
    provider_id: &str,
    hidden: Loadable<Vec<HiddenModel>>,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> Option<AnyElement> {
    let rows = hidden.ready()?;
    if rows.is_empty() {
        return None;
    }
    let count = rows.len();
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
    Some(
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(widgets::field_label(theme, "Hidden"))
            .child(list)
            .into_any_element(),
    )
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

    #[test]
    fn tool_chips_show_agent_facing_names() {
        assert_eq!(
            tool_display_name(&serde_json::json!({ "kind": "webFetch", "url": "x" })),
            "web_fetch"
        );
        assert_eq!(
            tool_display_name(&serde_json::json!({ "kind": "unknown", "name": "model_proposal" })),
            "model_proposal"
        );
        assert_eq!(tool_display_name(&serde_json::json!({})), "tool");
    }

    #[test]
    fn transcript_reset_rows_read_the_reset_array() {
        let frame = serde_json::json!({ "reset": [{ "role": "user" }] });
        assert_eq!(transcript_reset_rows(Some(&frame)).len(), 1);
        assert!(transcript_reset_rows(Some(&serde_json::json!({ "delta": 1 }))).is_empty());
        assert!(transcript_reset_rows(None).is_empty());
    }

    #[test]
    fn panel_heights_account_for_the_new_sections() {
        let models = Loadable::Ready(Vec::<Model>::new());
        let bare = provider_controls_height(&models, 0, false, false, false);
        // Hidden rows, the record form, and their absence all move the
        // panel's animated height.
        let with_hidden = provider_controls_height(&models, 2, false, false, false);
        let with_form = provider_controls_height(&models, 0, false, false, true);
        assert!(with_hidden > bare);
        assert!(with_form - bare >= RECORD_FORM_HEIGHT);
    }
}
