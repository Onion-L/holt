//! The combined provider/model/traits picker: provider, model, reasoning and
//! option selection (persisted per-chat or into the draft), the flattened
//! model-row cache keyboard nav and jump slots walk, and the tabbed popover
//! with its pinned traits tray.

use gpui::{AnyElement, App, Context, SharedString, Window, div, prelude::*, px};

use holt_proto::{ChatConfig, Model, Provider, ProviderId, ReasoningLevel};
use holt_rpc::methods;

use crate::popover::{self, Loadable};
use crate::theme::Theme;

use super::Pickers;
use super::logic::provider_brand_icon_for;
use super::{
    PROVIDER_TOOLTIP_DELAY, PickerKind, clamp_reasoning, default_reasoning, offered_providers,
    reasoning_label,
};

struct ProviderNameTooltip {
    name: SharedString,
}

impl Render for ProviderNameTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .shadow_md()
            .text_size(crate::typography::ui_rems(11.0))
            .text_color(theme.text)
            .child(self.name.clone())
    }
}

/// Which pane the provider/model picker's icon rail is showing (t3code
/// ModelPickerContent `selectedInstanceId | "provider catalog"`). `Provider` means
/// "the effective provider's list" — the rail has no browse-without-commit
/// state; clicking a brand icon picks that provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum ModelRail {
    #[default]
    Provider,
}

/// Cache key for the flattened model-row list: any input that changes the
/// list's CONTENT (not its highlight/selection, which render per-row).
#[derive(Clone, PartialEq, Eq)]
pub(super) struct ModelRowsKey {
    query: String,
    rail: ModelRail,
    effective: Option<ProviderId>,
    locked: bool,
    catalog_rev: u64,
}

/// One row of the model list: the model plus the provider it belongs to —
/// search results and the provider catalog view mix providers, and every row's
/// subline names its provider (t3code ModelListRow `showProvider`).
#[derive(Debug, Clone)]
pub(super) struct ModelRowData {
    provider: ProviderId,
    provider_name: SharedString,
    model: Model,
}

impl Pickers {
    fn pick_provider(&mut self, provider: ProviderId, cx: &mut Context<Self>) {
        if self.config.provider.as_ref() != Some(&provider) {
            // The remembered model for this provider takes over via the
            // defaults fallback; a foreign pick must not linger.
            self.config.model = None;
            self.config.reasoning = None;
            self.config.model_options.clear();
        }
        self.config.provider = Some(provider.clone());
        self.defaults.provider = Some(provider.to_string());
        self.save_defaults();
        self.model_scroll_base().set_offset(gpui::Point::default());
        self.ensure_models(provider, false, cx);
        // Re-anchor the keyboard highlight onto the new provider's selected row.
        self.active = self.selected_model_index(cx);
        cx.notify();
    }

    fn pick_model(&mut self, model_id: String, cx: &mut Context<Self>) {
        // The card stays open on a pick (user request): model and traits
        // share one popover now, and adjusting the tray right after choosing
        // a model is the expected flow. Esc, click-out, or the chip close it.
        if self.state.read(cx).selected_chat.is_some() {
            // Existing chat: persist to the chat row (Mutate setChatConfig) —
            // survives restarts and syncs; next runs in this chat use it.
            let selected = model_id.clone();
            self.update_chat_config(cx, move |config| config.model = selected);
        } else {
            // New chat: draft pick + sticky last-used memory for this provider.
            self.config.model = Some(model_id.clone());
        }
        if let Some(provider) = self.effective_provider(cx) {
            let label = self
                .models
                .get(&provider)
                .and_then(|l| l.ready())
                .and_then(|models| models.iter().find(|m| m.id == model_id))
                .map(|m| m.label.clone())
                .unwrap_or_else(|| model_id.clone());
            self.defaults
                .remember_model(provider.to_string(), model_id, label);
            self.save_defaults();
        }
        cx.notify();
    }

    fn pick_reasoning(&mut self, level: ReasoningLevel, cx: &mut Context<Self>) {
        // Always a concrete selection (no toggle-back-to-default).
        if self.state.read(cx).selected_chat.is_some() {
            self.update_chat_config(cx, move |config| config.reasoning = Some(level));
        } else {
            self.config.reasoning = Some(level);
        }
        self.defaults.reasoning = Some(level);
        self.save_defaults();
        cx.notify();
    }

    fn pick_option(
        &mut self,
        option_id: String,
        choice_id: String,
        default: bool,
        cx: &mut Context<Self>,
    ) {
        if self.state.read(cx).selected_chat.is_some() {
            self.update_chat_config(cx, move |config| {
                if default {
                    config.model_options.remove(&option_id);
                } else {
                    config
                        .model_options
                        .insert(option_id, serde_json::Value::String(choice_id));
                }
            });
        } else if default {
            self.config.model_options.remove(&option_id);
        } else {
            self.config
                .model_options
                .insert(option_id, serde_json::Value::String(choice_id));
        }
        cx.notify();
    }

    /// Apply `change` to the selected chat's effective config and persist it:
    /// optimistic row stamp (chips update on click) + `Mutate setChatConfig`
    /// (an LWW workspace write — it survives restarts). The written row
    /// always carries the CONCRETE resolved model/reasoning, with the
    /// reasoning re-clamped to the (possibly just-changed) model's ladder.
    fn update_chat_config(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut ChatConfig)) {
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let resolved = self.resolved(cx);
        let Some(mut config) = resolved.chat_config() else {
            return; // provider unknown (catalog + chat row both missing) — nothing safe to write
        };
        // Preserve fields the pickers don't own.
        if let Some(existing) = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            config.permission_mode = existing.permission_mode;
        }
        change(&mut config);
        // Reasoning must stay concrete for whatever model the row now names —
        // same ladder resolution as [`Self::trait_ladder`] (model levels, else
        // the provider's advertised ladder).
        if let Some(models) = self.models.get(&config.provider).and_then(|l| l.ready()) {
            let ladder = models
                .iter()
                .find(|m| m.id == config.model)
                .map(|m| m.reasoning_levels.clone())
                .unwrap_or_default();
            if !ladder.is_empty() {
                config.reasoning = clamp_reasoning(config.reasoning, &ladder);
            }
        }
        self.state.update(cx, |state, cx| {
            state.apply_chat_config(&chat_id, config.clone());
            cx.notify();
        });
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.mutate_task = Some(cx.spawn(async move |_, _| {
            let params = serde_json::json!({
                "op": "setChatConfig",
                "chatId": chat_id,
                "config": config,
            });
            if let Err(err) = engine.client().call(methods::MUTATE, params).await {
                tracing::warn!(error = %err, "setChatConfig mutate failed");
            }
        }));
    }

    // ---- keyboard ----

    /// The traits popover's reasoning ladder (model levels, falling back to
    /// the provider's advertised ladder) — shared by render and keyboard nav.
    pub(super) fn trait_ladder(&self, cx: &App) -> Vec<ReasoningLevel> {
        let Some(model) = self.selected_model(cx) else {
            return Vec::new();
        };
        if !model.reasoning_levels.is_empty() {
            return model.reasoning_levels.clone();
        }
        Vec::new()
    }

    /// The configured provider descriptors shown in the picker rail. The
    /// committed provider remains visible after its key is removed so the
    /// existing chat does not silently switch providers.
    fn rail_descriptors(&self, cx: &App) -> Vec<Provider> {
        let Some(list) = self.providers.ready() else {
            return Vec::new();
        };
        let mut descriptors = offered_providers(list);
        if let Some(effective) = self.effective_provider(cx)
            && !descriptors.iter().any(|d| d.id == effective)
            && let Some(descriptor) = list
                .iter()
                .flat_map(|row| row.concrete_providers())
                .find(|d| d.id == effective)
        {
            descriptors.insert(0, descriptor.clone());
        }
        descriptors
    }

    /// The model rows the picker currently shows, flat and in render order —
    /// keyboard nav, ⌘N jumps, Enter and the render walk THE SAME list.
    ///
    /// A live search spans every ready provider (t3: the sidebar hides and
    /// the query ignores it); otherwise the rail selection decides —
    /// provider catalog across providers, or the effective provider's list with its
    /// selected rows floated to the top (t3 `groupProvider catalog`). A locked chat
    /// restricts every view to its own provider.
    /// Cached [`Self::visible_model_rows`]: selection/highlight changes and
    /// re-renders share one flattened list until an input actually changes.
    fn model_rows(&self, cx: &App) -> std::sync::Arc<Vec<ModelRowData>> {
        let key = ModelRowsKey {
            query: self.search.read(cx).text().trim().to_string(),
            rail: self.model_rail,
            effective: self.effective_provider(cx),
            locked: self.provider_locked(cx),
            catalog_rev: self.catalog_rev,
        };
        if let Some((cached_key, rows)) = self.model_rows_cache.borrow().as_ref()
            && *cached_key == key
        {
            return rows.clone();
        }
        let rows = std::sync::Arc::new(self.visible_model_rows(cx));
        *self.model_rows_cache.borrow_mut() = Some((key, rows.clone()));
        rows
    }

    fn visible_model_rows(&self, cx: &App) -> Vec<ModelRowData> {
        let effective = self.effective_provider(cx);
        let mut descriptors = self.rail_descriptors(cx);
        if self.provider_locked(cx) {
            descriptors.retain(|d| Some(d.id.clone()) == effective);
        }
        let query = self.search.read(cx).text().trim().to_string();
        scoped_model_rows(
            &query,
            self.model_rail,
            effective,
            &descriptors,
            |provider| {
                self.models
                    .get(&provider)
                    .and_then(|l| l.ready())
                    .map(|models| models.as_slice())
            },
            |_, _| false,
        )
    }

    /// The row the keyboard-nav highlight starts on: the resolved selected
    /// model's index in the VISIBLE rows (the provider catalog/search views may not
    /// contain it — then 0), 0 while the list is loading.
    pub(super) fn selected_model_index(&self, cx: &App) -> usize {
        let selected = self.selected_model(cx).map(|m| m.id.clone());
        let effective = self.effective_provider(cx);
        self.model_rows(cx)
            .iter()
            .position(|row| {
                Some(row.provider.clone()) == effective
                    && selected.as_deref() == Some(row.model.id.as_str())
            })
            .unwrap_or(0)
    }

    /// The picker's visible row count (keyboard nav bounds).
    pub(super) fn model_rows_len(&self, cx: &App) -> usize {
        self.model_rows(cx).len()
    }

    /// Enter on the provider/model popover: pick the highlighted model.
    pub(super) fn activate_model_row(&mut self, cx: &mut Context<Self>) {
        self.activate_model_index(self.active, cx);
    }

    /// Pick the visible row at `ix` — a foreign-provider row (provider catalog /
    /// search) switches the provider first, exactly like clicking its rail
    /// icon and then the model.
    pub(super) fn activate_model_index(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.model_rows(cx).get(ix).cloned() else {
            return;
        };
        if self.effective_provider(cx) != Some(row.provider.clone()) {
            if self.provider_locked(cx) {
                return;
            }
            self.pick_provider(row.provider, cx);
        }
        self.pick_model(row.model.id, cx);
    }

    /// The combined provider + model switcher (holt provider-model-picker.tsx):
    /// a vertical provider rail of square brand-icon tabs on the left, the
    /// viewed provider's models on the right. On an existing chat the other
    /// tabs stay visible but disabled — the lock reads as a rule.
    /// The provider/model picker (t3code ModelPickerContent): an icons-only
    /// provider rail on the left (provider catalog model on top), a search box over
    /// the model list on the right. Rows are two lines — model name over the
    /// provider icon + name (t3 `showProvider`, replacing the description) —
    /// with a ⌘N jump chip and a model toggle trailing. Searching hides the
    /// rail and spans every provider.
    pub(super) fn render_provider_model_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // Compact tabbed layout (user request, modeled on the referenced
        // picker): the model LIST gets a fixed band of roughly seven compact
        // rows; the pinned traits tray below sizes to its sections.
        const LIST_HEIGHT: f32 = 216.0;

        let theme = Theme::of(cx).clone();

        // Catalog-level loading/error take over the whole card — the tabs ARE
        // the catalog, so there is nothing stable to draw above the skeleton.
        match &self.providers {
            Loadable::Loading | Loadable::Idle => {
                return div()
                    .h(px(LIST_HEIGHT))
                    .p(px(8.0))
                    .child(popover::skeleton_menu_rows(
                        "provider-skeleton",
                        &theme,
                        5,
                        cx.entity_id(),
                        cx,
                    ))
                    .into_any_element();
            }
            Loadable::Error(message) => {
                let message = message.clone();
                return div()
                    .h(px(LIST_HEIGHT))
                    .p(px(8.0))
                    .child(self.retry_row(
                        "provider-retry",
                        &message,
                        PickerKind::ProviderModel,
                        &theme,
                        cx,
                    ))
                    .into_any_element();
            }
            Loadable::Ready(_) => {}
        }

        let locked = self.provider_locked(cx);
        let effective = self.effective_provider(cx);
        let model_scroll = self.model_scroll.clone();
        let query = self.search.read(cx).text().trim().to_string();
        let searching = !query.is_empty();
        let descriptors = self.rail_descriptors(cx);
        // No-agents empty state: the catalog loaded but offers nothing
        // runnable (no provider has a key configured) and there's no
        // committed chat provider to force-include — guidance instead of an
        // empty tab row.
        if descriptors.is_empty() {
            return div()
                .p(px(16.0))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.0))
                .child(
                    crate::icons::icon(crate::icons::TERMINAL)
                        .size(px(20.0))
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text)
                        .child(SharedString::from("Configure provider")),
                )
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .text_center()
                        .child(SharedString::from(
                            "Add an API key in Settings → Providers.",
                        )),
                )
                .into_any_element();
        }
        let rows = self.model_rows(cx);

        // ── tabs: one brand mark per configured provider —
        //    ACROSS THE TOP (user request; was a left rail). The
        //    viewed tab wears a 2px accent bar sitting on the row's bottom
        //    hairline. Tabs never hide: a live search only filters the
        //    viewed tab's list, so switching tabs re-scopes the same query.
        let mut tabs = div()
            .flex_none()
            .h(px(40.0))
            .px(px(6.0))
            .border_b_1()
            .border_color(crate::theme::hairline(0.08))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0));
        for (ix, descriptor) in descriptors.iter().enumerate() {
            let provider = descriptor.id.clone();
            let is_viewed = effective.as_ref() == Some(&provider);
            let is_disabled = locked && !is_viewed;
            let abbreviation = descriptor.abbreviation.clone();
            let provider_name: SharedString = descriptor.name.clone().into();
            let brand_mark: AnyElement = match provider_brand_icon(&provider) {
                Some((path, tint)) => crate::icons::icon(path)
                    .size(px(18.0))
                    .text_color(tint.unwrap_or(if is_viewed {
                        theme.text
                    } else {
                        theme.text_muted
                    }))
                    .into_any_element(),
                None => div()
                    .text_size(crate::typography::ui_rems(10.0))
                    .text_color(if is_viewed {
                        theme.text
                    } else {
                        theme.text_muted
                    })
                    .child(SharedString::from(abbreviation))
                    .into_any_element(),
            };
            let picked_provider = provider.clone();
            tabs = tabs.child(
                div()
                    .id(("provider-tab", ix))
                    .relative()
                    .w(px(32.0))
                    .h(px(32.0))
                    .rounded(px(8.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(is_disabled, |el| el.opacity(0.35))
                    .when(!is_disabled, |el| el.cursor_pointer())
                    .when(!is_disabled && !is_viewed, |el| {
                        el.hover(|s| s.bg(crate::theme::ink(0.06)))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.model_rail = ModelRail::Provider;
                        this.pick_provider(picked_provider.clone(), cx);
                        cx.notify();
                    }))
                    .tooltip(move |_, cx| {
                        cx.new(|_| ProviderNameTooltip {
                            name: provider_name.clone(),
                        })
                        .into()
                    })
                    .tooltip_show_delay(PROVIDER_TOOLTIP_DELAY)
                    .child(brand_mark)
                    .when(is_viewed, |el| el.child(tab_indicator(theme.accent))),
            );
        }

        // ── search row: icon + borderless input over a full-bleed hairline.
        //    The placeholder names the scope — the query never leaves the
        //    viewed tab (user request; the old global search hid the rail).
        let search_row = div()
            .flex_none()
            .h(px(40.0))
            .px(px(10.0))
            .border_b_1()
            .border_color(crate::theme::hairline(0.08))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(
                crate::icons::icon(crate::icons::MAGNIFER)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(crate::typography::ui_rems(13.0))
                    .child(self.search.clone()),
            );

        // ── model rows: a VIRTUALIZED uniform list — only the visible slice
        //    renders, so a 7k-model catalog scrolls as smoothly as seven
        //    (field report: the un-virtualized stack was the picker's lag).
        //    Keyboard nav scrolls via the UniformListScrollHandle.
        let effective_models = effective.and_then(|h| self.models.get(&h));
        let model_list: Option<AnyElement> = if !rows.is_empty() {
            let entity = cx.entity();
            let row_data = rows.clone();
            Some(
                gpui::uniform_list(
                    "model-menu-scroll",
                    rows.len(),
                    move |range, _window, app| {
                        entity.update(app, |this, cx| {
                            range
                                .filter_map(|ix| {
                                    row_data
                                        .get(ix)
                                        .map(|row| this.render_model_row(ix, row, cx))
                                })
                                .collect::<Vec<AnyElement>>()
                        })
                    },
                )
                .size_full()
                .px(px(6.0))
                .track_scroll(&model_scroll)
                .into_any_element(),
            )
        } else {
            None
        };
        let list_children: Vec<AnyElement> = if !rows.is_empty() {
            Vec::new()
        } else if searching {
            vec![empty_list_note(&theme, "No models found")]
        } else {
            match effective_models {
                Some(Loadable::Error(message)) => {
                    let message = message.clone();
                    vec![self.retry_row(
                        "model-retry",
                        &message,
                        PickerKind::ProviderModel,
                        &theme,
                        cx,
                    )]
                }
                _ => vec![popover::skeleton_menu_rows(
                    "model-skeleton",
                    &theme,
                    5,
                    cx.entity_id(),
                    cx,
                )],
            }
        };

        let model_scrollbar = self.render_model_scrollbar(&theme, cx);
        let list_host = div()
            .id("model-list-scroll-host")
            .relative()
            .flex_none()
            .h(px(LIST_HEIGHT))
            .py(px(6.0))
            // A whisper of wash keeps the scrolling band readable between
            // the pinned chrome above and the traits tray below.
            .bg(crate::theme::ink(0.02))
            .on_hover(cx.listener(Self::on_model_list_hover))
            .child(match model_list {
                Some(list) => list,
                // Empty/loading/error notes: a plain static stack.
                None => div()
                    .id("model-menu-scroll")
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .px(px(6.0))
                    .children(list_children)
                    .into_any_element(),
            })
            // Absolute child: the hit rail and thumb float above the
            // scroll content without consuming any list width.
            .children(model_scrollbar);

        // ── traits tray: the reasoning ladder + model options PINNED under
        //    the list (the separate Traits popover folded in here — user
        //    request). Hidden entirely when the selected model has neither.
        let has_tray = !self.trait_ladder(cx).is_empty()
            || self
                .selected_model(cx)
                .is_some_and(|m| !m.options.is_empty());
        let tray: Option<AnyElement> = has_tray.then(|| {
            let sections = self.render_traits_sections(cx);
            div()
                .id("model-traits-tray")
                .flex_none()
                .border_t_1()
                .border_color(crate::theme::hairline(0.08))
                // Long option stacks scroll inside the tray rather than
                // growing the card past the viewport.
                .max_h(px(236.0))
                .overflow_y_scroll()
                .px(px(6.0))
                .pb(px(6.0))
                .child(sections)
                .into_any_element()
        });

        div()
            .flex()
            .flex_col()
            .child(tabs)
            .child(search_row)
            .child(list_host)
            .children(tray)
            .into_any_element()
    }

    /// One model row for the virtualized list. `ix` is the row's GLOBAL index
    /// (⌘N chips, hover-cursor, and activation all key on it). The 2px
    /// inter-row gap is baked into each item's bottom padding so every item
    /// is the same height (uniform_list measures the first).
    fn render_model_row(
        &mut self,
        ix: usize,
        row: &ModelRowData,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let effective = self.effective_provider(cx);
        let is_selected = Some(row.provider.clone()) == effective
            && self.selected_model(cx).map(|m| m.id.as_str()) == Some(row.model.id.as_str());
        let is_active = ix == self.active;
        let (icon_path, tint) =
            provider_brand_icon(&row.provider).unwrap_or((crate::icons::BOT, None));
        let label: SharedString = row.model.label.clone().into();
        let provider_name = row.provider_name.clone();
        // Provider attribution (field report: several connected opencode
        // providers advertise identically-named models — "GLM-5.2" exists
        // under 64 providers — and rows were indistinguishable). The driver
        // ships the provider display name in `description`; other providers'
        // taglines read fine in the same slot. Skip when it just repeats the
        // provider name.
        let attribution: Option<SharedString> = row
            .model
            .description
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case(provider_name.as_ref()))
            .map(|d| SharedString::from(d.to_owned()));
        let compact = self.model_rail == ModelRail::Provider;
        let mut el = div()
            .id(("model-row", ix))
            .px(px(8.0))
            .py(px(if compact { 5.0 } else { 6.0 }))
            .rounded(px(6.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .cursor_pointer();
        // ONE moving highlight (t3/Base-UI combobox): hovering moves the
        // keyboard cursor instead of painting its own wash, so hover + arrow
        // cursor can never wear two washes at once. Selection is the
        // distinct stronger treatment (wash + ring).
        if is_selected {
            el = el
                .bg(crate::theme::card_selected_bg())
                .shadow(crate::theme::card_selected_shadows());
        } else if is_active {
            el = el.bg(crate::theme::ink(0.05));
        }
        el = el.on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            if *hovered && this.active != ix {
                this.active = ix;
                cx.notify();
            }
        }));
        // Compact single-line rows on a provider tab (user request): every
        // row there shares the tab's provider, so the identity subline is
        // dead weight — attribution rides inline instead (opencode ships
        // identically-named models under 64 providers; it must stay
        // visible). The provider catalog tab mixes providers and keeps the
        // two-line layout with the brand subline.
        let body: AnyElement = if compact {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_none()
                        .max_w_full()
                        .truncate()
                        .text_size(crate::typography::ui_rems(12.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(label),
                )
                .when_some(attribution, |el, attribution| {
                    el.child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_muted.opacity(0.7))
                            .child(attribution),
                    )
                })
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_size(crate::typography::ui_rems(12.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(label),
                )
                .child(
                    // Provider identity subline (t3 `showProvider`), plus
                    // the model's own attribution when it carries one.
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            crate::icons::icon(icon_path)
                                .size(px(11.0))
                                .flex_none()
                                .text_color(tint.unwrap_or(theme.text_muted.opacity(0.7))),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(crate::typography::ui_rems(11.0))
                                .text_color(theme.text_muted.opacity(0.7))
                                .child(provider_name),
                        )
                        .when_some(attribution, |el, attribution| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(theme.text_muted.opacity(0.45))
                                    .child(SharedString::from("·")),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(theme.text_muted.opacity(0.7))
                                    .child(attribution),
                            )
                        }),
                )
                .into_any_element()
        };
        el = el
            .on_click(cx.listener(move |this, _, _, cx| {
                this.activate_model_index(ix, cx);
            }))
            .child(body);
        if ix < 9 {
            el = el.child(popover::kbd_hint(&theme, &format!("⌘{}", ix + 1)));
        }
        div().pb(px(2.0)).child(el).into_any_element()
    }

    /// The traits dropdown body (t3code TraitsPicker): the reasoning ladder
    /// plus every advertised model option as headed sections of menu ROWS —
    /// label, a "Default" badge on the section's default choice, and the
    /// trailing check on the selected row. Sections split by hairline
    /// separators. Selecting keeps the menu open for multi-adjust.
    fn render_traits_sections(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(model) = self.selected_model(cx).cloned() else {
            return popover::skeleton_menu_rows("traits-skeleton", &theme, 3, cx.entity_id(), cx);
        };
        let levels = self.trait_ladder(cx);
        // Display the effective level (draft pick or the chat's config), so
        // the ladder check mirrors the chip summary.
        let current = self.effective_reasoning(cx);

        let mut sections: Vec<AnyElement> = Vec::new();
        if !levels.is_empty() {
            let default_level = default_reasoning(&levels);
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    // 2px row gap — the menu-column rhythm everywhere else
                    // (model list, project list); without it adjacent
                    // hover/selected washes fuse into one blob (user report).
                    .gap(px(2.0))
                    .child(popover::menu_heading(&theme, "Reasoning"))
                    .children(levels.into_iter().enumerate().map(|(ix, level)| {
                        let is_active = current == Some(level);
                        let is_default = default_level == Some(level);
                        let mut row =
                            popover::menu_row(&theme, is_active, format!("trait-reasoning-{ix}"))
                                .py(px(5.0))
                                .rounded(px(6.0))
                                .text_size(crate::typography::ui_rems(12.5))
                                .id(("reasoning-row", ix))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_reasoning(level, cx);
                                }))
                                .child(SharedString::from(reasoning_label(level)));
                        row = row.child(div().flex_1());
                        if is_default {
                            row = row.child(default_badge(&theme));
                        }
                        row
                    }))
                    .into_any_element(),
            );
        }

        let selections = self.explicit_options(cx);
        for (opt_ix, option) in model.options.iter().enumerate() {
            if !sections.is_empty() {
                sections.push(popover::menu_separator().into_any_element());
            }
            let selected_choice = selections
                .get(&option.id)
                .and_then(|v| v.as_str())
                .unwrap_or(&option.default_choice)
                .to_string();
            let option_id = option.id.clone();
            let default_choice = option.default_choice.clone();
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0)) // same rhythm as the Reasoning section above
                    .child(popover::menu_heading(&theme, &option.label))
                    .children(
                        option
                            .choices
                            .iter()
                            .enumerate()
                            .map(|(choice_ix, choice)| {
                                let is_active = selected_choice == choice.id;
                                let choice_id = choice.id.clone();
                                let option_id = option_id.clone();
                                let is_default = choice.id == default_choice;
                                let mut row = popover::menu_row(
                                    &theme,
                                    is_active,
                                    format!("trait-choice-{opt_ix}-{choice_ix}"),
                                )
                                .py(px(5.0))
                                .rounded(px(6.0))
                                .text_size(crate::typography::ui_rems(12.5))
                                .id(("trait-choice", opt_ix * 32 + choice_ix))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_option(
                                        option_id.clone(),
                                        choice_id.clone(),
                                        is_default,
                                        cx,
                                    );
                                }))
                                .child(SharedString::from(choice.label.clone()));
                                row = row.child(div().flex_1());
                                if is_default {
                                    row = row.child(default_badge(&theme));
                                }
                                row
                            }),
                    )
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_col()
            .pb(px(2.0))
            .children(sections)
            .into_any_element()
    }
}

/// The "Default" marker beside a section's default choice: a ghost badge —
/// bare muted text, no border or fill (user request; t3code draws an outline
/// pill here).
fn default_badge(theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .text_size(crate::typography::ui_rems(10.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text_muted.opacity(0.6))
        .child(SharedString::from("Default"))
}

/// Brand mark + optional tint for a provider.
/// The 2px underline marking the viewed top tab: sits on the tab row's
/// bottom hairline (the tab is 32px tall inside a 40px row, so -4px lands
/// exactly on the border), rounded like a capsule.
fn tab_indicator(tint: gpui::Hsla) -> gpui::Div {
    div()
        .absolute()
        .bottom(px(-4.0))
        .left(px(6.0))
        .right(px(6.0))
        .h(px(2.0))
        .rounded(px(1.0))
        .bg(tint)
}

/// Flatten the picker's visible rows for one tab. The QUERY NEVER LEAVES THE
/// VIEWED TAB (user request; the old global search spanned every provider and
/// hid the rail): on a provider tab it ranks that provider's models only, on
/// the provider catalog tab it ranks the selected set. Without a query, a provider
/// tab lists its catalog models-first and the provider catalog tab lists every model.
fn scoped_model_rows<'a>(
    query: &str,
    _rail: ModelRail,
    effective: Option<ProviderId>,
    descriptors: &[Provider],
    models_for: impl Fn(ProviderId) -> Option<&'a [Model]>,
    _is_favorite: impl Fn(ProviderId, &str) -> bool,
) -> Vec<ModelRowData> {
    let row = |descriptor: &Provider, model: &Model| ModelRowData {
        provider: descriptor.id.clone(),
        provider_name: SharedString::from(descriptor.name.clone()),
        model: model.clone(),
    };
    let in_scope = |descriptor: &Provider, _model: &Model| Some(descriptor.id.clone()) == effective;
    if !query.is_empty() {
        // Rank: label prefix < label substring < description hit; models,
        // then input order, break ties (t3 modelPickerSearch's field ladder
        // + favorite boost, collapsed to our ranks). The description stays
        // in the haystack — opencode's provider attribution ("anthropic")
        // must find its models even inside one tab.
        let mut ranked: Vec<(usize, usize, usize, ModelRowData)> = Vec::new();
        let mut input_ix = 0usize;
        for descriptor in descriptors {
            let Some(models) = models_for(descriptor.id.clone()) else {
                continue;
            };
            for model in models {
                if !in_scope(descriptor, model) {
                    continue;
                }
                let by_label = popover::match_rank(query, &model.label);
                let by_description = popover::match_rank(
                    query,
                    &format!(
                        "{} {}",
                        model.description.as_deref().unwrap_or(""),
                        model.label
                    ),
                )
                .map(|rank| rank + 2);
                if let Some(rank) = by_label.into_iter().chain(by_description).min() {
                    ranked.push((rank, 0, input_ix, row(descriptor, model)));
                }
                input_ix += 1;
            }
        }
        ranked.sort_by_key(|(rank, unselected, ix, _)| (*rank, *unselected, *ix));
        return ranked.into_iter().map(|(_, _, _, row)| row).collect();
    }
    let Some(descriptor) = descriptors.iter().find(|d| Some(d.id.clone()) == effective) else {
        return Vec::new();
    };
    models_for(descriptor.id.clone())
        .unwrap_or_default()
        .iter()
        .map(|model| row(descriptor, model))
        .collect()
}

/// Centered muted note filling an empty model list ("No models found").
fn empty_list_note(theme: &Theme, copy: &str) -> AnyElement {
    div()
        .px(px(8.0))
        .py(px(24.0))
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(theme.text_muted.opacity(0.6))
        .text_center()
        .child(SharedString::from(copy.to_string()))
        .into_any_element()
}

pub(crate) fn provider_brand_icon(
    provider: &ProviderId,
) -> Option<(&'static str, Option<gpui::Hsla>)> {
    provider_brand_icon_for(provider, crate::theme::current_appearance()).map(|path| (path, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- test scaffolding ----

    fn model(id: &str, label: &str) -> Model {
        Model {
            id: id.to_string(),
            provider: ProviderId("test".into()),
            label: label.to_string(),
            description: None,
            reasoning_levels: Vec::new(),
            default_reasoning: None,
            options: Vec::new(),
            custom: false,
        }
    }

    fn model_with_description(id: &str, label: &str, description: &str) -> Model {
        Model {
            description: Some(description.to_string()),
            ..model(id, label)
        }
    }

    /// A concrete (flattened, variant-less) provider descriptor.
    fn descriptor(id: &str, configured: bool) -> Provider {
        Provider {
            id: ProviderId(id.into()),
            name: id.to_string(),
            abbreviation: id[..2.min(id.len())].to_ascii_uppercase(),
            configured,
            variants: Vec::new(),
        }
    }

    // ---- model-row scoping + ranking ----

    #[test]
    fn scoped_rows_list_only_the_effective_provider_without_a_query() {
        let descriptors = vec![descriptor("openai", true), descriptor("anthropic", true)];
        let openai_models = vec![model("openai/one", "One"), model("openai/two", "Two")];
        let anthropic_models = vec![model("anthropic/opus", "Opus")];
        let rows = scoped_model_rows(
            "",
            ModelRail::Provider,
            Some(ProviderId("openai".into())),
            &descriptors,
            |provider| {
                if provider.as_str() == "openai" {
                    Some(&openai_models)
                } else {
                    Some(&anthropic_models)
                }
            },
            |_, _| false,
        );
        let ids: Vec<&str> = rows.iter().map(|r| r.model.id.as_str()).collect();
        assert_eq!(ids, ["openai/one", "openai/two"]);
        // Rows carry the descriptor identity the subline renders.
        assert_eq!(rows[0].provider.as_str(), "openai");
        assert_eq!(rows[0].provider_name.as_ref(), "openai");
    }

    #[test]
    fn scoped_rows_empty_without_a_matching_descriptor() {
        let descriptors = vec![descriptor("openai", true)];
        let models = vec![model("openai/one", "One")];
        let rows = scoped_model_rows(
            "",
            ModelRail::Provider,
            Some(ProviderId("anthropic".into())),
            &descriptors,
            |_| Some(&models),
            |_, _| false,
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn search_ranks_prefix_over_substring_over_description() {
        let descriptors = vec![descriptor("test", true)];
        let models = vec![
            model_with_description("t/krop", "Krop", "a big model"),
            model_with_description("t/opus", "Opus", ""),
            model_with_description("t/koko", "Koko", "opus-grade quality"),
            model_with_description("t/else", "Else", "unrelated"),
        ];
        let rows = scoped_model_rows(
            "op",
            ModelRail::Provider,
            Some(ProviderId("test".into())),
            &descriptors,
            |_| Some(&models),
            |_, _| false,
        );
        let ids: Vec<&str> = rows.iter().map(|r| r.model.id.as_str()).collect();
        // Label prefix ("Opus") < label substring ("Krop") < description hit
        // ("Koko"); the non-matching row is excluded.
        assert_eq!(ids, ["t/opus", "t/krop", "t/koko"]);
    }

    #[test]
    fn search_keeps_input_order_among_equal_ranks() {
        let descriptors = vec![descriptor("test", true)];
        let models = vec![
            model("t/second", "Opus 2"),
            model("t/first", "Opus 1"),
            model("t/third", "GPT Opus"),
        ];
        let rows = scoped_model_rows(
            "opus",
            ModelRail::Provider,
            Some(ProviderId("test".into())),
            &descriptors,
            |_| Some(&models),
            |_, _| false,
        );
        let ids: Vec<&str> = rows.iter().map(|r| r.model.id.as_str()).collect();
        assert_eq!(ids, ["t/second", "t/first", "t/third"]);
    }

    #[test]
    fn search_stays_scoped_to_the_effective_provider() {
        let descriptors = vec![descriptor("openai", true), descriptor("anthropic", true)];
        let openai_models = vec![model("openai/opus-mini", "Opus Mini")];
        let anthropic_models = vec![model("anthropic/opus", "Opus")];
        let rows = scoped_model_rows(
            "opus",
            ModelRail::Provider,
            Some(ProviderId("openai".into())),
            &descriptors,
            |provider| {
                if provider.as_str() == "openai" {
                    Some(&openai_models)
                } else {
                    Some(&anthropic_models)
                }
            },
            |_, _| false,
        );
        let ids: Vec<&str> = rows.iter().map(|r| r.model.id.as_str()).collect();
        assert_eq!(ids, ["openai/opus-mini"]);
    }

    #[test]
    fn search_skips_providers_whose_models_have_not_loaded() {
        let descriptors = vec![descriptor("openai", true), descriptor("anthropic", true)];
        let anthropic_models = vec![model("anthropic/opus", "Opus")];
        let rows = scoped_model_rows(
            "opus",
            ModelRail::Provider,
            Some(ProviderId("anthropic".into())),
            &descriptors,
            |provider| (provider.as_str() == "anthropic").then_some(&anthropic_models),
            |_, _| false,
        );
        let ids: Vec<&str> = rows.iter().map(|r| r.model.id.as_str()).collect();
        assert_eq!(ids, ["anthropic/opus"]);
    }
}
