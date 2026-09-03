//! Composer pickers (feature-inventory §1.7): RepoPicker (recents + search +
//! in-app folder browser + clone/create), BranchPicker (search + create row +
//! immediate safe switches of the target working directory — live in
//! sessions too, ADR-0007), ProviderModelPicker (provider rail + model list,
//! provider locked once the chat exists), TraitsPicker (reasoning ladder +
//! advertised model options; trigger shows the non-default summary
//! "High · 1M · Fast").
//!
//! Draft selections accumulate into a [`DraftConfig`] the composer threads
//! into the Run command and the `Mutate createChat` call on first send;
//! session picks switch the chat's working directory right away.
//!
//! Pure logic (repo ordering, folder-browser navigation, pick routing,
//! traits summary) lives in free functions with unit tests; RPC results
//! land in [`Loadable`] slots rendered as skeletons / inline errors with
//! Retry.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable as _, KeyDownEvent, SharedString,
    Subscription, Task, Window, div, prelude::*, px,
};

use holt_proto::{Model, Provider, ProviderId, ReasoningLevel, RepoRef};

/// Display cap for the ref list (t3code shows pages of 100 with a status
/// footer; a flat cap + "Showing X of Y refs" reads the same without
/// pagination plumbing).
const MAX_REF_ROWS: usize = 300;
const PROVIDER_TOOLTIP_DELAY: Duration = Duration::from_secs(1);

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::{self, Loadable, MenuKey};
use crate::settings::composer::ComposerDefaults;
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Module layout: this file is the facade (`Pickers` entity, open/close, chip
// orchestration); pure domain logic lives in `logic`, catalog loads in
// `catalog`, the provider/model picker in `provider_model`, branch/checkout
// in `checkout`, the project picker in `space`, and shared frame/chip/search/
// scrollbar helpers in `common`. Public names keep their `crate::pickers`
// paths through the re-exports below.
// ---------------------------------------------------------------------------

mod catalog;
mod checkout;
mod common;
mod logic;
mod provider_model;
mod space;

use common::{attach_overlay, attach_overlay_end};
pub(crate) use logic::normalize_model_rows;
pub use logic::{
    CheckoutKind, CheckoutPlan, DraftConfig, ResolvedRunConfig, SwitchDialogContent, breadcrumbs,
    browser_rows, child_path, clamp_reasoning, completion_prefix_len, default_model,
    default_reasoning, offered_providers, parent_path, reasoning_label, segment_target,
    traits_customized, traits_summary, typed_path_target,
};
pub(crate) use provider_model::provider_brand_icon;
use provider_model::{ModelRail, ModelRowData, ModelRowsKey};

/// Dev/testing knob: `HOLT_SLOW_CATALOG_MS=<ms>` delays every provider and
/// model catalog result app-side — the chip/tab/list loading states are
/// sub-second against a warm local daemon and unstageable otherwise
/// (headless-rig captures; same family as `HOLT_OPEN_PICKER`).
fn slow_catalog_delay() -> Option<std::time::Duration> {
    std::env::var("HOLT_SLOW_CATALOG_MS")
        .ok()
        .and_then(|ms| ms.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
}

// ---------------------------------------------------------------------------
// Catalog invalidation (Settings → Providers changes)
// ---------------------------------------------------------------------------

/// Marker global: [`bump_provider_catalog`] pokes it whenever a Settings →
/// A provider credential change updates availability, and every [`Pickers`] observes it
/// to force-refresh its cached provider catalog — without this the composer
/// served the boot-time list until restart (user report).
#[derive(Default)]
pub struct ProviderCatalogChanged;

impl gpui::Global for ProviderCatalogChanged {}

/// Notify all composers that the provider catalog changed. The global carries
/// no data — `default_global` pushes the observer effect, and the observers
/// re-fetch from the engine (the source of truth).
pub fn bump_provider_catalog(cx: &mut App) {
    cx.default_global::<ProviderCatalogChanged>();
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

/// Sentinel for "no keyboard-highlighted row" (`active`): matches no index,
/// and `usize::MAX as isize == -1` — `menu_step` treats it like `None`, so
/// the first Down lands on row 0.
const NO_ACTIVE_ROW: usize = usize::MAX;

/// Which picker popover is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Branch,
    /// The checkout-kind dropdown in the composer footer (Current
    /// checkout/worktree | New worktree).
    Checkout,
    /// The combined agent/model/traits popover: provider tabs across the top,
    /// the tab's model list beneath the search, and the pinned traits tray
    /// (reasoning ladder + model options) at the bottom — one trigger, one
    /// card (the separate Traits popover folded in here).
    ProviderModel,
    /// New-session canvas only: which project the session mints into. A pick
    /// re-keys everything project-derived (refs) via the state observer.
    Space,
}

pub struct Pickers {
    state: Entity<AppState>,
    config: DraftConfig,
    /// Sticky last-used picks (holt `holt.composer.defaults:v1`): seeds the
    /// new-chat chips and is rewritten on every new-chat pick.
    defaults: ComposerDefaults,
    /// Where [`Self::defaults`] persists (`{data_dir}/composer-defaults.json`);
    /// `None` before bootstrap stamps the state (writes are skipped).
    data_dir: Option<PathBuf>,
    /// Selection the draft picks belong to — switching chats drops them so a
    /// pick made in one chat never leaks into another.
    draft_owner: Option<String>,
    /// Space the branch draft/cache belong to (see the state observer).
    space_owner: Option<String>,
    open: popover::Popup<PickerKind>,
    /// The provider/model picker's rail selection (provider catalog vs the effective
    /// provider's list). Re-primed on every open.
    model_rail: ModelRail,
    providers: Loadable<Vec<Provider>>,
    models: HashMap<ProviderId, Loadable<Vec<Model>>>,
    refs: Loadable<Vec<RepoRef>>,
    /// Working directory the `refs` slot was listed against (invalidated
    /// when the target moves — a chat with its own folder, a space switch).
    refs_target: Option<String>,
    /// Highlighted row in the open list (keyboard nav).
    active: usize,
    /// Models-list scroll — keyboard nav keeps the highlighted row in view.
    /// A `UniformListScrollHandle`: the model list virtualizes (7k-model
    /// catalogs must scroll smoothly), and this is its handle; the plain
    /// base handle inside serves the floating scrollbar's metrics.
    model_scroll: gpui::UniformListScrollHandle,
    /// Flattened rows the list/keyboard/⌘N all walk, cached per
    /// [`ModelRowsKey`]: a 7k-model catalog rebuilt+ranked on every
    /// keystroke, arrow press AND render was the picker's open/scroll lag.
    model_rows_cache: std::cell::RefCell<Option<(ModelRowsKey, std::sync::Arc<Vec<ModelRowData>>)>>,
    /// Bumped on every catalog/provider catalog mutation; invalidates the cache.
    catalog_rev: u64,
    /// Hover/drag state of the floating model-list scrollbar.
    model_bar: popover::MenuScrollbarState,
    /// Shared search / URL / name input, reused across popovers.
    search: Entity<ComposerInput>,
    /// One-shot mute for the next Edited event's highlight reset — armed by
    /// [`Self::toggle`]'s programmatic clear (see the subscription).
    search_reset_muted: bool,
    focus: FocusHandle,
    /// `HOLT_OPEN_PICKER` boot: keep claiming focus until it sticks, so
    /// keyboard nav drives the data-side-opened popover (headless rigs have
    /// no synthetic pointer, but synthetic keys do arrive).
    boot_focus_pending: bool,
    load_task: Option<Task<()>>,
    /// Own slot: the refs load runs concurrently with the eager
    /// provider/model loads — sharing `load_task` would abort one mid-flight.
    refs_task: Option<Task<()>>,
    /// In-flight branch switch (the ref being switched to): one at a time,
    /// draft or session alike (ADR-0007).
    switching: Option<String>,
    switch_task: Option<Task<()>>,
    /// The raised switch-failure dialog (ADR-0007): inform-only, dismissed
    /// explicitly. Set by the pick/create failure paths; `None` = quiet.
    switch_dialog: Option<SwitchDialogContent>,
    mutate_task: Option<Task<()>>,
    /// The branch picker's create row: an inline name input collapsed behind
    /// an affordance until engaged (`CreateBranch` — create-and-switch).
    branch_create: Entity<ComposerInput>,
    branch_create_engaged: bool,
    create_task: Option<Task<()>>,
    _search_events: Subscription,
    _create_events: Subscription,
    _state_observe: Subscription,
    _catalog_observe: Subscription,
}

impl Pickers {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| ComposerInput::new("Search…", cx));
        // The branch picker's create-row input: Enter submits, Escape is the
        // frame's to handle (abandon the row, keep the popover open).
        let branch_create = cx.new(|cx| ComposerInput::new("New branch name…", cx));
        let create_events = cx.subscribe(&branch_create, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.create_branch_submit(cx);
            }
        });
        let search_events = cx.subscribe(&search, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Edited => {
                // Typing in a filter resets the highlight to the top of the
                // fresh results. `set_text` emits Edited on programmatic
                // clears too, and this subscription runs AFTER `toggle`
                // returns — an unmuted reset clobbers the just-anchored
                // selected row back to 0, leaving the top row wearing a
                // second highlight next to the selection (user report;
                // `toggle` arms the mute right before its clear).
                if !std::mem::take(&mut this.search_reset_muted) {
                    if this.open_kind() == Some(PickerKind::Branch) {
                        this.active = 0;
                    }
                    if this.open_kind() == Some(PickerKind::ProviderModel) {
                        this.active = 0;
                        this.model_scroll_base().set_offset(gpui::Point::default());
                    }
                }
                cx.notify();
            }
            ComposerInputEvent::Submitted => this.on_search_submit(cx),
            // Pasted images/files don't apply to a search box.
            ComposerInputEvent::PastedImages(_)
            | ComposerInputEvent::PastedPaths(_)
            | ComposerInputEvent::CursorMoved
            | ComposerInputEvent::ViewportChanged
            | ComposerInputEvent::MentionNavigate(_)
            | ComposerInputEvent::MentionAccept
            | ComposerInputEvent::MentionDismiss => {}
        });
        // Chat selection / config changes must re-render the chips (child views
        // only re-render on their own notify). A selection change also drops
        // the draft picks — they belonged to the previous chat/new-chat canvas.
        let state_observe = cx.observe(&state, |this: &mut Self, state, cx| {
            let selected = state.read(cx).selected_chat.clone();
            if selected != this.draft_owner {
                this.draft_owner = selected;
                this.config.provider = None;
                this.config.model = None;
                this.config.reasoning = None;
                this.config.model_options.clear();
            }
            // A space switch invalidates the branch draft + cache — the folder
            // changed under them.
            let space = state.read(cx).selected_space.clone();
            if space != this.space_owner {
                this.space_owner = space;
                this.config.branch = None;
                this.config.checkout = CheckoutKind::default();
                this.refs = Loadable::Idle;
                this.refs_target = None;
            }
            cx.notify();
        });
        // A Settings → Providers change updated the configured set: force-refresh
        // the cached catalog so the rail/chips follow without a restart
        // (stale rows stay visible while the reload runs).
        let catalog_observe = cx.observe_global::<ProviderCatalogChanged>(|this: &mut Self, cx| {
            this.ensure_providers(true, cx);
            cx.notify();
        });
        // Dev/testing knob: `HOLT_OPEN_PICKER=model|traits|repo|branch` boots
        // with that popover open — synthetic input can't reach the app on
        // headless compositors, so captures need a data-side path.
        let boot_open = match std::env::var("HOLT_OPEN_PICKER").ok().as_deref() {
            Some("model") => Some(PickerKind::ProviderModel),
            Some("traits") => Some(PickerKind::ProviderModel),
            Some("branch") => Some(PickerKind::Branch),
            Some("checkout") => Some(PickerKind::Checkout),
            Some("project") => Some(PickerKind::Space),
            _ => None,
        };
        let mut open = popover::Popup::default();
        if let Some(kind) = boot_open {
            open.open(kind);
        }
        // Sticky last-used picks: loaded synchronously so the very first frame
        // shows the remembered provider/model/reasoning, never a placeholder.
        let data_dir = state.read(cx).data_dir.clone();
        let defaults = data_dir
            .as_deref()
            .map(ComposerDefaults::load)
            .unwrap_or_default();
        // Restore the last project pick (the canvas's "defaults to last
        // selected" rule). Vanished rows heal in `apply_spaces`. A remembered
        // "Don't work in a project" opt-out is deliberately NOT restored: the
        // menu row is gone, so a stale saved opt-out would strand the canvas
        // in a state the picker can no longer express.
        {
            let project = defaults.project.clone();
            state.update(cx, |s, _| {
                if s.selected_space.is_none() {
                    s.selected_space = project;
                }
            });
        }
        let draft_owner = state.read(cx).selected_chat.clone();
        let space_owner = state.read(cx).selected_space.clone();
        Self {
            state,
            space_owner,
            config: DraftConfig::default(),
            defaults,
            data_dir,
            draft_owner,
            open,
            model_rail: ModelRail::default(),
            providers: Loadable::Idle,
            models: HashMap::new(),
            refs: Loadable::Idle,
            refs_target: None,
            active: 0,
            model_scroll: gpui::UniformListScrollHandle::new(),
            model_rows_cache: std::cell::RefCell::new(None),
            catalog_rev: 0,
            model_bar: popover::MenuScrollbarState::default(),
            search,
            search_reset_muted: false,
            focus: cx.focus_handle(),
            boot_focus_pending: boot_open.is_some(),
            load_task: None,
            refs_task: None,
            switching: None,
            switch_task: None,
            switch_dialog: None,
            mutate_task: None,
            branch_create,
            branch_create_engaged: false,
            create_task: None,
            _search_events: search_events,
            _create_events: create_events,
            _state_observe: state_observe,
            _catalog_observe: catalog_observe,
        }
    }

    /// Persist the sticky defaults (best-effort; picks are rare and tiny).
    fn save_defaults(&self) {
        if let Some(dir) = self.data_dir.as_deref()
            && let Err(err) = self.defaults.save(dir)
        {
            tracing::warn!(error = %err, "composer-defaults save failed");
        }
    }

    pub fn draft(&self) -> &DraftConfig {
        &self.config
    }

    /// Provider is locked once the chat exists (feature-inventory §1.7).
    fn provider_locked(&self, _cx: &App) -> bool {
        false
    }

    fn engine(&self, cx: &App) -> Option<EngineHandle> {
        self.state.read(cx).engine().cloned()
    }

    /// Effective provider: picked, or the chat's config, or the first listed.
    fn effective_provider(&self, cx: &App) -> Option<ProviderId> {
        if let Some(provider) = self.config.provider.clone() {
            return Some(provider);
        }
        if let Some(config) = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            return Some(config.provider.clone());
        }
        // New-chat canvas: the remembered last-used provider (sticky defaults),
        // when the loaded catalog still offers it (its key may have been
        // removed in Settings → Providers since).
        if let Some(provider) = self.defaults.provider.as_ref() {
            let provider_id = ProviderId(provider.clone());
            let offered = match self.providers.ready() {
                Some(list) => offered_providers(list).iter().any(|d| d.id == provider_id),
                None => true, // catalog not loaded yet — trust the memory
            };
            if offered {
                return Some(provider_id);
            }
        }
        // Fall back to the first configured provider.
        self.providers
            .ready()
            .and_then(|list| offered_providers(list).first().map(|d| d.id.clone()))
    }

    /// Effective model id: the draft pick, the selected chat's config, or (on
    /// the new-chat canvas) the remembered last-used model for the provider.
    fn effective_model_id<'a>(&'a self, cx: &'a App) -> Option<&'a str> {
        if let Some(id) = self.config.model.as_deref() {
            return Some(id);
        }
        if let Some(chat) = self.state.read(cx).selected_chat_row() {
            return chat.config.as_ref().map(|c| c.model.as_str());
        }
        let provider = self.effective_provider(cx)?;
        self.defaults
            .model_for(provider.as_str())
            .map(|m| m.id.as_str())
    }

    /// Effective reasoning — always concrete once the model is known: the
    /// draft pick / chat config / remembered default, clamped to the selected
    /// model's ladder, falling back to the model's default level.
    fn effective_reasoning(&self, cx: &App) -> Option<ReasoningLevel> {
        let explicit = self.config.reasoning.or_else(|| {
            match self.state.read(cx).selected_chat_row() {
                Some(chat) => chat.config.as_ref().and_then(|c| c.reasoning),
                // New chat: the remembered last-used level.
                None => self.defaults.reasoning,
            }
        });
        if self.selected_model(cx).is_none() {
            // Catalog not loaded yet: show the explicit value as-is (nothing
            // to clamp against); it resolves to a concrete level on load.
            return explicit;
        }
        clamp_reasoning(explicit, &self.trait_ladder(cx))
    }

    /// The selected model — concrete from the moment the list loads: the
    /// effective id when the list still offers it, else the provider default
    /// (first row). Never `None` with a non-empty catalog.
    fn selected_model<'a>(&'a self, cx: &'a App) -> Option<&'a Model> {
        let provider = self.effective_provider(cx)?;
        let models = self.models.get(&provider)?.ready()?;
        match self.effective_model_id(cx) {
            Some(id) => models
                .iter()
                .find(|m| m.id == id)
                .or_else(|| default_model(models)),
            None => default_model(models),
        }
    }

    /// The explicit (non-default) option picks: the chat's persisted
    /// selections for existing chats, the draft's for the new-chat canvas.
    fn explicit_options(&self, cx: &App) -> serde_json::Map<String, serde_json::Value> {
        match self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            Some(config) => config.model_options.clone(),
            None => self.config.model_options.clone(),
        }
    }

    /// The fully-resolved config the composer threads into the Run request and
    /// `Mutate createChat`: concrete model + reasoning whenever the catalog is
    /// loaded (no "engine picks a default" passthrough).
    /// The resolved provider's steering mode, from the loaded descriptor list.
    /// `None` while the catalog is loading (callers should assume the common
    /// StepBoundary case and show nothing).
    pub fn resolved_steering_mode(&self, _cx: &App) -> Option<holt_proto::SteeringMode> {
        Some(holt_proto::SteeringMode::StepBoundary)
    }

    /// The catalog is loaded and has no configured provider.
    pub fn no_providers_available(&self) -> bool {
        self.providers
            .ready()
            .is_some_and(|list| offered_providers(list).is_empty())
    }

    pub fn can_send(&self, cx: &App) -> bool {
        let Some(providers) = self.providers.ready() else {
            return true;
        };
        let Some(selected) = self.effective_provider(cx) else {
            return false;
        };
        offered_providers(providers)
            .iter()
            .any(|provider| provider.id == selected)
            && self.resolved(cx).model.is_some()
    }

    pub fn resolved(&self, cx: &App) -> ResolvedRunConfig {
        ResolvedRunConfig {
            provider: self.effective_provider(cx),
            model: self
                .selected_model(cx)
                .map(|m| m.id.clone())
                // Catalog not loaded (offline): still send the id we know.
                .or_else(|| self.effective_model_id(cx).map(str::to_string)),
            reasoning: self.effective_reasoning(cx),
            model_options: self.explicit_options(cx),
        }
    }

    // ---- open/close ----

    /// The picker that's open AND interactive — `None` while one animates out.
    fn open_kind(&self) -> Option<PickerKind> {
        self.open.as_open().copied()
    }

    /// Whether any picker popover is open (shell-side: session-nav shortcuts
    /// go quiet underneath an open popover instead of yanking the session out
    /// from under it).
    pub fn is_open(&self) -> bool {
        self.open.as_open().is_some()
    }

    /// The picker to render: open or mid-exit.
    fn mounted_kind(&self) -> Option<PickerKind> {
        self.open.get().copied()
    }

    /// Begin the exit animation (shared by every close path).
    fn animate_close(&mut self, cx: &mut Context<Self>) {
        self.model_bar = popover::MenuScrollbarState::default();
        self.branch_create_engaged = false;
        if self.open.begin_close() {
            popover::reap_popup(cx, |pickers: &mut Self| &mut pickers.open);
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.animate_close(cx);
        cx.notify();
    }

    /// Capture knob (`HOLT_OPEN_DIALOG=model`): open the combined
    /// provider/model menu programmatically.
    /// A jump-slot press while the model menu is open. The shell's session
    /// bindings (Mod+1…9) win the dispatch race — gpui runs a matched
    /// binding before any key handler — so the shell forwards the slot here
    /// instead of going quiet and eating the very chips the rows advertise
    /// (macOS field report: "cmd shortcuts do nothing in the model
    /// selector"). Returns whether the menu was open and the slot consumed.
    pub fn jump_model_slot(&mut self, slot: usize, cx: &mut Context<Self>) -> bool {
        if self.open_kind() != Some(PickerKind::ProviderModel) {
            return false;
        }
        self.activate_model_index(slot, cx);
        cx.notify();
        true
    }

    pub fn open_model_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_kind() != Some(PickerKind::ProviderModel) {
            self.toggle(PickerKind::ProviderModel, window, cx);
        }
    }

    fn toggle(&mut self, kind: PickerKind, window: &mut Window, cx: &mut Context<Self>) {
        // A press that found this picker open closes it — the card's
        // `on_mouse_down_out` already began the close on that same press,
        // so by click time the popup reads as closed and a plain toggle
        // would reopen it. A press while a DIFFERENT picker is open doesn't
        // count (see note_trigger_press_matching): that click switches.
        let pressed_open = self.open.take_press_was_open();
        if self.open_kind() == Some(kind) || pressed_open {
            self.animate_close(cx);
            cx.notify();
            return;
        }
        self.open.open(kind);
        // Clearing stale text emits Edited AFTER this function returns —
        // mute that one event so its reset can't clobber the highlight
        // anchored below (the no-op clear is also skipped for the same
        // reason).
        self.search_reset_muted = !self.search.read(cx).text().is_empty();
        self.search.update(cx, |input, cx| {
            input.set_placeholder("Search…", cx);
            if !input.text().is_empty() {
                input.set_text("", cx);
            }
        });
        // Prime the model picker's rail BEFORE anchoring the highlight (the
        // visible rows depend on it): the provider catalog view when models exist —
        // t3 ModelPickerContent's initial selection — else the effective
        // provider. Locked chats stay on their own provider.
        if kind == PickerKind::ProviderModel {
            self.model_rail = ModelRail::Provider;
        }
        // The keyboard-nav highlight starts ON the selected row — row 0
        // otherwise reads as a second active row (user report).
        self.active = match kind {
            PickerKind::Checkout => match self.config.checkout {
                CheckoutKind::Local => 0,
                CheckoutKind::NewWorktree => 1,
            },
            PickerKind::Branch => self.selected_ref_index(cx),
            PickerKind::ProviderModel => self.selected_model_index(cx),
            PickerKind::Space => self.selected_space_index(cx),
        };
        if kind == PickerKind::ProviderModel {
            self.model_scroll_base().set_offset(gpui::Point::default());
            self.model_scroll
                .scroll_to_item(self.active, gpui::ScrollStrategy::Nearest);
        }
        // Searchable pickers focus the filter input (it sits inside the frame,
        // so the frame's key handler still sees arrows/Enter); the rest focus
        // the frame itself for pure keyboard nav.
        match kind {
            PickerKind::Branch => {
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search refs…", cx);
                });
                window.focus(&handle, cx);
            }
            PickerKind::Space => {
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search projects…", cx);
                });
                window.focus(&handle, cx);
            }
            PickerKind::ProviderModel => {
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search models…", cx);
                });
                window.focus(&handle, cx);
            }
            _ => window.focus(&self.focus, cx),
        }
        match kind {
            // Force: the checkout state moves under us (a send mints a
            // worktree+branch, terminals switch refs) — every open
            // revalidates, keeping stale rows visible until fresh ones land.
            PickerKind::Branch | PickerKind::Checkout => self.ensure_refs(true, cx),
            PickerKind::ProviderModel => {
                // Force: configured providers can change in Settings —
                // every open revalidates, keeping current rows visible until
                // the fresh catalog lands.
                self.ensure_providers(true, cx);
                // Revalidate models on every open instead of pinning a stale
                // catalog until the application restarts.
                self.prefetch_models(true, cx);
            }
            // Projects are already synced state — nothing to load.
            PickerKind::Space => {}
        }
        cx.notify();
    }

    fn on_search_submit(&mut self, cx: &mut Context<Self>) {
        if self.open_kind() == Some(PickerKind::Branch)
            && let Some(row) = self.filtered_ref_rows(cx).into_iter().nth(self.active)
        {
            self.pick_ref(row, cx);
        }
        if self.open_kind() == Some(PickerKind::Space)
            && let Some(space) = self.filtered_space_rows(cx).into_iter().nth(self.active)
        {
            self.pick_space(space.id, cx);
        }
        // The model search box submits the highlighted row (Enter reaches
        // here via the input's Submitted event while it holds focus).
        if self.open_kind() == Some(PickerKind::ProviderModel) {
            self.activate_model_row(cx);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // The frame stays mounted (and possibly focused) through the exit
        // animation — keys must not drive a dying popover.
        if !self.open.is_open() {
            return;
        }
        // ⌘1…⌘9 jump-picks the Nth visible model row (t3 modelPickerKeys;
        // the chips on the rows advertise these).
        if self.open_kind() == Some(PickerKind::ProviderModel)
            && event.keystroke.modifiers.platform
            && let Ok(n) = event.keystroke.key.parse::<usize>()
            && (1..=9).contains(&n)
        {
            self.activate_model_index(n - 1, cx);
            cx.notify();
            return;
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        let search_focused = self.search.read(cx).focus_handle(cx).is_focused(window);
        // An engaged create row owns Enter/Escape: Enter submits through the
        // input's Submitted event, Escape abandons the row without closing
        // the popover.
        let create_focused = self.branch_create_engaged
            && self
                .branch_create
                .read(cx)
                .focus_handle(cx)
                .is_focused(window);
        match key {
            MenuKey::Escape => {
                if create_focused {
                    self.branch_create_engaged = false;
                    window.focus(&self.focus, cx);
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                self.animate_close(cx);
                cx.notify();
            }
            MenuKey::Up | MenuKey::Down => {
                let delta = if key == MenuKey::Up { -1 } else { 1 };
                let count = match self.open_kind() {
                    Some(PickerKind::Branch) => self.filtered_ref_rows(cx).len().min(MAX_REF_ROWS),
                    Some(PickerKind::Checkout) => 2,
                    // Keyboard nav walks the MODEL list only; the traits
                    // chips below (reasoning ladder, model options) are
                    // mouse-only.
                    Some(PickerKind::ProviderModel) => self.model_rows_len(cx),
                    Some(PickerKind::Space) => self.filtered_space_rows(cx).len(),
                    None => 0,
                };
                let current = (self.active != NO_ACTIVE_ROW).then_some(self.active);
                self.active = popover::menu_step(current, count, delta).unwrap_or(0);
                // Keep the highlighted MODEL row in view (the rows are the
                // scroll container's direct children, so indices map 1:1);
                // the traits chips below live in the pinned tray and never
                // need scrolling into view.
                if self.open_kind() == Some(PickerKind::ProviderModel)
                    && self.active < self.model_rows_len(cx)
                {
                    self.model_scroll
                        .scroll_to_item(self.active, gpui::ScrollStrategy::Nearest);
                }
                cx.notify();
            }
            MenuKey::Enter if !search_focused && !create_focused => {
                if self.open_kind() == Some(PickerKind::ProviderModel) {
                    self.activate_model_row(cx);
                } else if self.open_kind() == Some(PickerKind::Checkout) {
                    let kind = if self.active == 0 {
                        CheckoutKind::Local
                    } else {
                        CheckoutKind::NewWorktree
                    };
                    self.pick_checkout(kind, cx);
                } else {
                    self.on_search_submit(cx);
                }
            }
            _ => {}
        }
    }

    // ---- render ----

    /// The switch-failure modal (ADR-0007): inform-only — explanation, the
    /// blocking file list when the refusal carried one, and a single
    /// dismiss. No force, stash, or discard action exists anywhere.
    fn render_switch_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(content) = self.switch_dialog.clone() else {
            return div().into_any_element();
        };
        let mut card = popover::dialog_card(&theme)
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" {
                    this.switch_dialog = None;
                    cx.notify();
                }
            }))
            .child(popover::dialog_title(&theme, &content.title))
            .child(
                div()
                    .mt(px(6.0))
                    .child(popover::dialog_body(&theme, content.message.clone())),
            );
        if !content.files.is_empty() {
            card = card.child(
                div()
                    .id("switch-refusal-files")
                    .mt(px(10.0))
                    .max_h(px(180.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(crate::theme::hairline(0.08))
                    .bg(crate::theme::ink(0.04))
                    .children(content.files.iter().map(|file| {
                        div()
                            .px(px(10.0))
                            .py(px(4.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .font_family(theme.font_mono.clone())
                            .text_color(theme.text_muted.opacity(0.9))
                            .child(SharedString::from(file.clone()))
                    })),
            );
        }
        card = card.child(
            div()
                .mt(px(16.0))
                .flex()
                .flex_row()
                .justify_end()
                .gap(px(8.0))
                .child(
                    popover::btn_primary(&theme, "Got it")
                        .id("switch-dialog-dismiss")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.switch_dialog = None;
                            cx.notify();
                        })),
                ),
        );
        popover::modal(
            "switch-refusal-dialog",
            window.viewport_size(),
            card.into_any_element(),
        )
    }

    /// The new-session target row — the project selector chip rendered ABOVE
    /// the composer pill, left-aligned like the checkout toolbar (the
    /// composer footer carries only checkout + ref, and sessions show their
    /// target in the titlebar instead).
    pub fn render_target_selectors(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let closing = self.open.closing_since();
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
            Some(PickerKind::Space) => {
                let content = self.render_space_popover(cx);
                Some((PickerKind::Space, self.popover_frame(280.0, content, cx)))
            }
            _ => None,
        };
        let project_label: SharedString = {
            let state = self.state.read(cx);
            state
                .selected_space_row()
                .map(|s| s.display_name().to_string())
                .unwrap_or_else(|| "No project".to_string())
                .into()
        };
        let project_chip = self.footer_chip(
            PickerKind::Space,
            "picker-project",
            crate::icons::FOLDER,
            project_label,
            &theme,
            cx,
        );
        // Same left-edge geometry as the checkout toolbar under the pill
        // (`render_footer`'s row): full-width, 10px inset, chips hugging the
        // left. The row sits just above the composer pill, so the menus open
        // UPWARD.
        div()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .px(px(10.0))
            .child(attach_overlay(
                project_chip,
                &mut overlay,
                PickerKind::Space,
                "project-popover",
                closing,
            ))
            .into_any_element()
    }

    /// The composer footer row: checkout-kind + ref, LEFT-aligned, only when
    /// the picked (or session's) project has git. The project picker lives in
    /// the row above the pill ([`Self::render_target_selectors`]); sessions
    /// name their target in the titlebar.
    /// The composer footer row: checkout-kind + branch chip, LEFT-aligned,
    /// only when the picked (or session's) project has git. In a session the
    /// branch chip is live (ADR-0007): it renders the working directory's
    /// current branch and its picker switches the chat's own folder. The
    /// project picker lives in the row above the pill
    /// ([`Self::render_target_selectors`]); sessions name their target in
    /// the titlebar.
    pub fn render_footer(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        // A selected chat whose workspace row hasn't synced yet (the moment
        // right after send mints it) still renders the DRAFT footer — the
        // values are identical, so the toolbar never blinks through a
        // half-empty transitional state.
        let (space, session, change_request) = {
            let state = self.state.read(cx);
            let space = state.selected_space_row().cloned();
            let session = state
                .selected_chat
                .as_ref()
                .and_then(|_| state.selected_chat_row().cloned());
            let change_request = session
                .as_ref()
                .and_then(|chat| state.change_request_for_chat(chat).cloned());
            (space, session, change_request)
        };
        let row = || {
            // Symmetric: the container's 8px gap sits above the toolbar;
            // bleeding 8 of the container's 16px bottom padding (mb -8)
            // leaves 8 below — equal air on both sides of the row.
            // `w_full` is load-bearing: without it the canvas layout sizes
            // the row to CONTENT, and the left cluster's flex_1 (basis 0)
            // collapsed to zero width — both clusters painted from the same
            // origin, chips overlapping (user report).
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .px(px(10.0))
                .mb(px(-8.0))
        };

        if let Some(chat) = &session {
            // A live session: the branch chip is the same interactive pick
            // the draft renders (ADR-0007) — it shows the working
            // directory's live current branch, and a pick safe-switches the
            // chat's own folder immediately (mid-Turn included). The
            // checkout-kind label stays display-only here; its "New
            // worktree" option is a draft-only affordance.
            let space = space.as_ref().filter(|s| s.git_detected)?;
            let is_worktree = chat.cwd.as_deref().is_some_and(|cwd| cwd != space.path);
            let icon_path = if is_worktree {
                crate::icons::FOLDER_WITH_FILES
            } else {
                crate::icons::FOLDER
            };
            // One rule for icon and label (see logic::session_checkout_label).
            let kind_label = logic::session_checkout_label(&space.path, chat.cwd.as_deref());
            // Refs feed the live label — eager + idempotent, keyed to the
            // chat's own working directory.
            self.ensure_refs(false, cx);
            let branch_label = logic::session_branch_label(
                self.live_current_branch().as_deref(),
                chat.branch.as_deref(),
            );
            let closing = self.open.closing_since();
            let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
                Some(PickerKind::Branch) => {
                    let content = self.render_branch_popover(cx);
                    Some((PickerKind::Branch, self.popover_frame(320.0, content, cx)))
                }
                _ => None,
            };
            let ref_chip = self.footer_chip(
                PickerKind::Branch,
                "picker-branch-session",
                crate::icons::GIT_BRANCH,
                SharedString::from(branch_label),
                &theme,
                cx,
            );
            // Mirrors the draft chips: checkout hugs the left edge, ref the
            // right.
            let left = div()
                .flex()
                .flex_row()
                .items_center()
                .min_w_0()
                .child(Self::footer_label(
                    icon_path,
                    SharedString::from(kind_label),
                    &theme,
                ));
            let right = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .min_w_0()
                .when_some(change_request, |el, summary| {
                    el.child(crate::change_requests::pull_request_badge(
                        "composer-pull-request".into(),
                        summary,
                        crate::change_requests::ChangeRequestBadgeSurface::Composer,
                        &theme,
                    ))
                })
                .child(attach_overlay_end(
                    ref_chip,
                    &mut overlay,
                    PickerKind::Branch,
                    "branch-popover-session",
                    closing,
                ));
            return Some(row().child(left).child(right).into_any_element());
        }

        // New-session draft: checkout + ref only, LEFT-aligned (the project
        // picker lives in the row above the pill now).
        let git = space.as_ref().is_some_and(|s| s.git_detected);
        if !git {
            return None;
        }
        // Refs feed the draft labels — eager + idempotent.
        self.ensure_refs(false, cx);
        let closing = self.open.closing_since();
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
            Some(PickerKind::Branch) => {
                let content = self.render_branch_popover(cx);
                Some((PickerKind::Branch, self.popover_frame(320.0, content, cx)))
            }
            Some(PickerKind::Checkout) => {
                let content = self.render_checkout_popover(cx);
                Some((PickerKind::Checkout, self.popover_frame(224.0, content, cx)))
            }
            // The Space popover mounts on the target row above the pill
            // (`render_target_selectors`), not here.
            _ => None,
        };

        let ref_label = self.ref_label();
        let ref_chip = self.footer_chip(
            PickerKind::Branch,
            "picker-branch",
            crate::icons::GIT_BRANCH,
            ref_label,
            &theme,
            cx,
        );
        let kind_icon = match self.config.checkout {
            CheckoutKind::Local => crate::icons::FOLDER,
            CheckoutKind::NewWorktree => crate::icons::FOLDER_WITH_FILES,
        };
        let kind_chip = self.footer_chip(
            PickerKind::Checkout,
            "picker-checkout",
            kind_icon,
            SharedString::from(self.checkout_label()),
            &theme,
            cx,
        );
        // Checkout on the left edge, ref on the right — the row's
        // justify_between splits them (user request).
        let left = div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .child(attach_overlay(
                kind_chip,
                &mut overlay,
                PickerKind::Checkout,
                "checkout-popover",
                closing,
            ));
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .child(attach_overlay_end(
                ref_chip,
                &mut overlay,
                PickerKind::Branch,
                "branch-popover",
                closing,
            ));
        Some(row().child(left).child(right).into_any_element())
    }
}

impl Render for Pickers {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        // A HOLT_OPEN_PICKER popover never went through `toggle`, so claim
        // its keyboard focus here (re-claim until it sticks — the shell's
        // first-paint fallback focuses the composer after our first render).
        if self.boot_focus_pending {
            match self.open_kind() {
                Some(PickerKind::Branch) => {
                    self.search.update(cx, |input, cx| {
                        input.set_placeholder("Search refs…", cx);
                    });
                    let handle = self.search.read(cx).focus_handle(cx);
                    if handle.is_focused(window) {
                        self.boot_focus_pending = false;
                    } else {
                        window.focus(&handle, cx);
                    }
                }
                Some(_) => {
                    if self.focus.is_focused(window) {
                        self.boot_focus_pending = false;
                    } else {
                        window.focus(&self.focus, cx);
                    }
                }
                None => self.boot_focus_pending = false,
            }
        }

        // Eager-load the provider catalog + every offered provider's models so
        // the chip reads "Fable 5" (a concrete pick) before any popover
        // opens, and rail switches inside the picker are instant.
        self.ensure_providers(false, cx);
        self.prefetch_models(false, cx);
        // A popover opened data-side (HOLT_OPEN_PICKER) never went through
        // `toggle`, so kick its loads here (all ensure_* are idempotent).
        if matches!(
            self.open_kind(),
            Some(PickerKind::Branch) | Some(PickerKind::Checkout)
        ) && matches!(self.refs, Loadable::Idle)
        {
            self.ensure_refs(false, cx);
        }
        // Chip shows the model's display name alone (holt `modelText`); the
        // provider reads from the brand mark beside it. Never "Default model":
        // before the catalog lands the remembered label (or the configured id)
        // names the pick; the loaded list then resolves it to a concrete row.
        // No-provider state: nothing runnable resolved (and the catalog is
        // loaded, so that's a conclusion, not a loading gap) — the chip says
        // so instead of wearing a provider mark that cannot run.
        let no_providers = self.no_providers_available() && self.effective_provider(cx).is_none();
        let model_label: SharedString = if no_providers {
            SharedString::from("Configure provider")
        } else {
            let loaded = self.selected_model(cx).map(|m| m.label.clone());
            let label = loaded.or_else(|| {
                let remembered = self
                    .effective_provider(cx)
                    .and_then(|provider| self.defaults.model_for(provider.as_str()));
                match self.effective_model_id(cx) {
                    Some(id) => Some(
                        remembered
                            .filter(|m| m.id == id)
                            .map(|m| m.label.clone())
                            .or_else(|| self.defaults.label_for(id).map(str::to_string))
                            .unwrap_or_else(|| id.to_string()),
                    ),
                    None => remembered.map(|m| m.label.clone()),
                }
            });
            label.map(SharedString::from).unwrap_or_default()
        };
        let catalog_loading = matches!(self.providers, Loadable::Idle | Loadable::Loading);
        let models_loading = self.effective_provider(cx).is_some_and(|provider| {
            !matches!(
                self.models.get(&provider),
                Some(Loadable::Ready(_)) | Some(Loadable::Error(_))
            )
        });
        // Provider unknown while the catalog resolves: the pixel-glyph loader
        // instead of guessing a brand mark.
        let chip_icon_loading =
            self.effective_provider(cx).is_none() && !no_providers && catalog_loading;
        // Provider known but nothing names the model yet (fresh install, no
        // remembered pick): a ghost label instead of a bare icon.
        let chip_label_loading =
            !no_providers && model_label.is_empty() && (catalog_loading || models_loading);
        let provider_icon: (&'static str, Option<gpui::Hsla>) = match self.effective_provider(cx) {
            Some(provider) => provider_brand_icon(&provider).unwrap_or((crate::icons::BOT, None)),
            None if no_providers => (crate::icons::TERMINAL, Some(theme.text_muted)),
            None => (crate::icons::BOT, Some(theme.text_muted)),
        };
        let explicit_options = self.explicit_options(cx);
        let traits_set = traits_summary(
            self.selected_model(cx),
            self.effective_reasoning(cx),
            &explicit_options,
        );
        let traits_active = traits_customized(
            self.selected_model(cx),
            self.effective_reasoning(cx),
            &self.trait_ladder(cx),
            &explicit_options,
        );
        // Render the open popover's body first (mutable borrow), then the
        // chips. Branch/Checkout render in the composer FOOTER row (see
        // `render_footer`), not here.
        let closing = self.open.closing_since();
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
            // Footer- and target-row pickers — their popovers mount there.
            Some(PickerKind::Branch) | Some(PickerKind::Checkout) | Some(PickerKind::Space) => None,
            Some(PickerKind::ProviderModel) => {
                let content = self.render_provider_model_popover(cx);
                Some((
                    PickerKind::ProviderModel,
                    // Compact single-provider pane (t3 ModelPickerContent
                    // shrunk to its tabbed layout).
                    self.popover_frame_flush(304.0, content, cx),
                ))
            }
            None => None,
        };

        // Left cluster: empty — the project picker lives in the row above
        // the pill (`render_target_selectors`).
        // Right cluster: agent+model and traits — the composer appends
        // attach + send after this element (holt composer-actions.tsx
        // arrangement).
        let left = div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .gap(px(4.0));
        // ONE chip for the whole run identity (user request): brand icon +
        // model name, then the joined traits summary ("Medium", "High · 1M ·
        // Fast", "Agent · Balance") as the chip's muted second tone — the
        // run's configuration reads without opening anything, and the suffix
        // brightens only when something departs from its default. No suffix
        // when the model has neither a ladder nor options (e.g. Hermes).
        let chip_suffix = traits_set.map(|summary| {
            (
                SharedString::from(summary),
                traits_active.then(|| theme.text.opacity(0.85)),
            )
        });
        let model_chip = self.trigger_chip(
            PickerKind::ProviderModel,
            model_label,
            true,
            Some(provider_icon),
            chip_icon_loading,
            chip_label_loading,
            chip_suffix,
            no_providers,
            &theme,
            cx,
        );
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .gap(px(4.0))
            // End-anchored: the menu's right edge sits flush with the chip's
            // right edge (user request), same as the footer's ref popover.
            .child(attach_overlay_end(
                model_chip,
                &mut overlay,
                PickerKind::ProviderModel,
                "model-popover",
                closing,
            ));
        // The switch-failure modal rides this entity wherever the composer
        // mounts it (deferred + priority: it floats above all session
        // chrome); an empty div when the dialog is down.
        let switch_dialog = if self.switch_dialog.is_some() {
            self.render_switch_dialog(window, cx)
        } else {
            div().into_any_element()
        };
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(Theme::SPACE_SM))
            // GPUI dispatches this captured stream while the thumb is dragged,
            // including when the pointer has left the model popover.
            .on_drag_move(cx.listener(Self::on_model_scrollbar_drag_move))
            .child(left)
            .child(right)
            .child(switch_dialog)
    }
}
