//! The manual model-record form (ADR-model-setup-applies-stored-proposals's manual half): field
//! construction, validation, the API-dialect dropdown, and the dialog.

use super::*;

/// The model-record form's single-line fields, in render order:
/// (key, label, placeholder). Laid out two per row. No `baseUrl` field —
/// a record added under a provider rides that provider's endpoint; the
/// engine fills the default, and Advanced JSON is the override hatch.
/// `api` is not a text field: the dialect set is closed (the engine's
/// registered list), so it rides a dropdown. Labels are catalog lookups,
/// so this is a function, not a const; placeholders stay literal — they
/// are example values, not copy.
pub(super) fn record_fields() -> [(&'static str, Cow<'static, str>, &'static str); 8] {
    [
        (
            "id",
            t!("settings_providers_usage.providers.record_form.field_model_id"),
            "acme-1",
        ),
        (
            "name",
            t!("settings_providers_usage.providers.record_form.field_name"),
            "Acme 1",
        ),
        (
            "contextWindow",
            t!("settings_providers_usage.providers.record_form.field_context_window"),
            "200000",
        ),
        (
            "maxTokens",
            t!("settings_providers_usage.providers.record_form.field_max_tokens"),
            "8192",
        ),
        (
            "inputCost",
            t!("settings_providers_usage.providers.record_form.field_input_cost"),
            "0.0",
        ),
        (
            "outputCost",
            t!("settings_providers_usage.providers.record_form.field_output_cost"),
            "0.0",
        ),
        (
            "cacheReadCost",
            t!("settings_providers_usage.providers.record_form.field_cache_read_cost"),
            "0.0",
        ),
        (
            "cacheWriteCost",
            t!("settings_providers_usage.providers.record_form.field_cache_write_cost"),
            "0.0",
        ),
    ]
}

/// A numeric form field's display label, for whole-sentence error
/// messages ("%{field} must be a number"): the same key the form's label
/// renders, so the error names the field as the user read it.
fn record_field_label(key: &str) -> Cow<'static, str> {
    match key {
        "contextWindow" => {
            t!("settings_providers_usage.providers.record_form.field_context_window")
        }
        "maxTokens" => t!("settings_providers_usage.providers.record_form.field_max_tokens"),
        "inputCost" => t!("settings_providers_usage.providers.record_form.field_input_cost"),
        "outputCost" => t!("settings_providers_usage.providers.record_form.field_output_cost"),
        "cacheReadCost" => {
            t!("settings_providers_usage.providers.record_form.field_cache_read_cost")
        }
        "cacheWriteCost" => {
            t!("settings_providers_usage.providers.record_form.field_cache_write_cost")
        }
        other => Cow::Owned(other.to_string()),
    }
}

/// One labelled checkbox row — the manual record form's Reasoning / Image
/// input flags. The whole row is the click target; `scope` keeps a
/// caller's element ids apart from another caller's.
fn flag_row(
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
    scope: &'static str,
    key: &'static str,
    label: Cow<'static, str>,
    on: bool,
    toggle: impl Fn(&mut ProvidersPage, &mut Context<ProvidersPage>) + 'static,
) -> AnyElement {
    div()
        .id(SharedString::from(format!("{scope}-flag-{key}")))
        .debug_selector(move || format!("{scope}-flag-{key}"))
        .cursor_pointer()
        .flex()
        .items_center()
        .gap(px(8.0))
        .on_click(cx.listener(move |page, _, _, cx| toggle(page, cx)))
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
                .child(label),
        )
        .into_any_element()
}

/// The record fields the form owns. The advanced JSON carries everything
/// else a record may hold, and can never override these — save for a cost's
/// `tiers`, which the form has no field for and puts back under its rates.
const FORM_OWNED_KEYS: [&str; 9] = [
    "id",
    "name",
    "api",
    "provider",
    "reasoning",
    "input",
    "cost",
    "contextWindow",
    "maxTokens",
];

/// A stored cost as the form's text — the catalog's `2.5`, or the field's
/// placeholder when the record carries no number.
fn cost_text(value: &serde_json::Value, placeholder: &str) -> String {
    value
        .as_f64()
        .map(|value| format!("{value}"))
        .unwrap_or_else(|| placeholder.to_string())
}

/// The mounted model-record form. One panel is expanded at a time, so one
/// instance serves the whole page; it targets that panel's active variant.
pub(super) struct RecordForm {
    provider: String,
    inputs: HashMap<String, Entity<ComposerInput>>,
    /// The picked API dialect (the dropdown's selection; the set comes
    /// from the engine's `ListApiDialects`).
    api: String,
    reasoning: bool,
    image: bool,
    /// Opened on a row that already exists: the title says so.
    editing: bool,
    error: Option<String>,
}

/// The record form's API-dialect dropdown: a closed set (the engine's
/// registered dialects), so a picker instead of a typo-prone text field.
/// The trigger mirrors [`bordered_input`]'s shape to sit flush in the
/// form's grid; the menu opens downward at [`popover::ABOVE_MODAL_PRIORITY`]
/// because the dialog itself is a modal.
pub(super) fn record_api_dropdown(
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
                    .debug_selector(move || selector.clone())
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

/// The Add-model-record dialog (`popover::modal` from the page), opened
/// from the Models header's "+ Add model" action. Basic fields up front;
/// `thinkingLevelMap`/`compat`/`headers` ride the advanced JSON textarea —
/// a full structured editor is not worth the surface.
pub(super) fn record_form_dialog(
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
    let pair = |left: (&str, Cow<'static, str>), right: Option<(&str, Cow<'static, str>)>| {
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
    let mut card = popover::dialog_card(theme)
        // A mask click dismisses — unless the API dropdown is open, whose
        // rows float outside this card's bounds and must survive the same
        // press that closes the menu.
        .on_mouse_down_out(cx.listener(|page, _, _, cx| {
            if page.record_api_menu.get().is_none() {
                page.record_form = None;
                cx.notify();
            }
        }))
        .w(px(560.0))
        .gap(px(14.0))
        .child(
            div()
                .flex()
                .items_center()
                .child(popover::dialog_title(
                    theme,
                    &if form.editing {
                        t!("settings_providers_usage.providers.record_form.edit_title")
                    } else {
                        t!("settings_providers_usage.providers.record_form.title")
                    },
                ))
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
        .child(pair(
            (
                "id",
                t!("settings_providers_usage.providers.record_form.field_model_id"),
            ),
            Some((
                "name",
                t!("settings_providers_usage.providers.record_form.field_name"),
            )),
        ))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(widgets::field_label(
                    theme,
                    t!("settings_providers_usage.providers.record_form.api_dialect_label"),
                ))
                .child(api_dropdown),
        )
        .child(pair(
            (
                "contextWindow",
                t!("settings_providers_usage.providers.record_form.field_context_window"),
            ),
            Some((
                "maxTokens",
                t!("settings_providers_usage.providers.record_form.field_max_tokens"),
            )),
        ))
        .child(pair(
            (
                "inputCost",
                t!("settings_providers_usage.providers.record_form.field_input_cost"),
            ),
            Some((
                "outputCost",
                t!("settings_providers_usage.providers.record_form.field_output_cost"),
            )),
        ))
        .child(pair(
            (
                "cacheReadCost",
                t!("settings_providers_usage.providers.record_form.field_cache_read_cost"),
            ),
            Some((
                "cacheWriteCost",
                t!("settings_providers_usage.providers.record_form.field_cache_write_cost"),
            )),
        ))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(20.0))
                .child(flag_row(
                    theme,
                    cx,
                    "record",
                    "reasoning",
                    t!("settings_providers_usage.providers.record_form.reasoning"),
                    form.reasoning,
                    |page, cx| page.toggle_record_flag("reasoning", cx),
                ))
                .child(flag_row(
                    theme,
                    cx,
                    "record",
                    "image",
                    t!("settings_providers_usage.providers.record_form.image_input"),
                    form.image,
                    |page, cx| page.toggle_record_flag("image", cx),
                )),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(widgets::field_label(
                    theme,
                    t!("settings_providers_usage.providers.record_form.advanced_json"),
                ))
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
                        .child(t!(
                            "settings_providers_usage.providers.record_form.save_record"
                        )),
                )
                .child(
                    widgets::ghost_action(theme)
                        .id("cancel-record")
                        .hover(move |style| widgets::ghost_hover(theme, style))
                        .on_click(cx.listener(move |page: &mut ProvidersPage, _, _, cx| {
                            page.record_form = None;
                            cx.notify();
                        }))
                        .child(t!("settings_providers_usage.providers.cancel")),
                ),
        );
    card.into_any_element()
}

/// The fetch dialog's width and its columns: the checkbox, the model, and
/// one column per number a row carries. They add up to the card's inner
/// width, with a gap between each and the rows' own padding.
const FETCH_WIDTH: f32 = 960.0;
const FETCH_GAP: f32 = 12.0;
const FETCH_COLUMNS: [f32; 7] = [22.0, 196.0, 82.0, 82.0, 280.0, 90.0, 68.0];

/// The fetch dialog: a vendor listing as a table
/// (ADR-fetched-models-adopt-the-catalogs-record). One row per id — its
/// name over its id, context window, max output, thinking levels, and
/// price, and where its numbers come from. An id the catalog carries shows
/// the catalog's own; a new one opens with a sibling's or the plain
/// defaults and is edited in place, its level chips as switches; one the
/// provider already offers is shown dimmed and never added again. A row is
/// corrected afterwards the way any record is: the record form, opened on
/// the row, replaces it.
pub(super) fn fetch_models_dialog(
    page: &mut ProvidersPage,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let Some(dialog) = page.fetch_dialog.as_ref() else {
        return div().into_any_element();
    };
    let catalog = dialog
        .rows
        .iter()
        .filter(|row| row.source.is_some() && !row.added)
        .count();
    let fresh = dialog
        .rows
        .iter()
        .filter(|row| row.source.is_none() && !row.added)
        .count();
    let added = dialog.rows.iter().filter(|row| row.added).count();
    let mut summary = Vec::new();
    for (count, text) in [
        (
            catalog,
            t!(
                "settings_providers_usage.providers.record_form.fetch_count_catalog",
                count = catalog
            ),
        ),
        (
            fresh,
            t!(
                "settings_providers_usage.providers.record_form.fetch_count_new",
                count = fresh
            ),
        ),
        (
            added,
            t!(
                "settings_providers_usage.providers.record_form.fetch_count_added",
                count = added
            ),
        ),
    ] {
        if count > 0 {
            summary.push(text.into_owned());
        }
    }
    let rows: Vec<AnyElement> = dialog
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| fetch_table_row(theme, dialog, index, row, cx))
        .collect();
    let selected = dialog
        .rows
        .iter()
        .filter(|row| dialog.checked.contains(&row.id))
        .collect::<Vec<_>>();
    let mut foot = Vec::new();
    let adopted = selected.iter().filter(|row| row.source.is_some()).count();
    let starting = selected.len() - adopted;
    if adopted > 0 {
        foot.push(
            t!(
                "settings_providers_usage.providers.record_form.fetch_foot_catalog",
                count = adopted
            )
            .into_owned(),
        );
    }
    if starting > 0 {
        let from = if selected
            .iter()
            .any(|row| row.source.is_none() && row.starts_from.is_some())
        {
            t!("settings_providers_usage.providers.record_form.fetch_from_sibling")
        } else {
            t!("settings_providers_usage.providers.record_form.fetch_from_defaults")
        };
        foot.push(
            t!(
                "settings_providers_usage.providers.record_form.fetch_foot_new",
                count = starting,
                from = from.as_ref()
            )
            .into_owned(),
        );
    }
    if foot.is_empty() {
        foot.push(
            t!("settings_providers_usage.providers.record_form.fetch_foot_none").into_owned(),
        );
    }
    let count = selected.len();
    let hover_theme = theme.clone();
    let mut card = popover::dialog_card(theme)
        // A mask click dismisses, unless a save is under way.
        .on_mouse_down_out(cx.listener(|page: &mut ProvidersPage, _, _, cx| {
            if !page
                .fetch_dialog
                .as_ref()
                .is_some_and(|dialog| dialog.saving)
            {
                page.fetch_dialog = None;
                cx.notify();
            }
        }))
        .w(px(FETCH_WIDTH))
        .gap(px(16.0))
        .child(
            div()
                .flex()
                .items_start()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(popover::dialog_title(
                            theme,
                            &t!(
                                "settings_providers_usage.providers.record_form.fetch_title",
                                provider = dialog.provider.as_str()
                            ),
                        ))
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(summary.join(" · "))),
                        ),
                )
                .child(div().flex_1())
                .child(
                    widgets::ghost_action(theme)
                        .id("fetch-dialog-close")
                        .hover(move |style| widgets::ghost_hover(&hover_theme, style))
                        .on_click(cx.listener(|page: &mut ProvidersPage, _, _, cx| {
                            page.fetch_dialog = None;
                            cx.notify();
                        }))
                        .child(
                            crate::icons::icon(crate::icons::CLOSE)
                                .size(px(13.0))
                                .text_color(theme.text_muted),
                        ),
                ),
        )
        .child(
            div()
                .id("fetch-table")
                .debug_selector(|| "fetch-table".into())
                .max_h(px(420.0))
                .overflow_y_scroll()
                .occlude()
                .flex()
                .flex_col()
                .child(fetch_table_header(theme))
                .children(rows),
        );
    if dialog.bulk.is_some() && fresh >= 2 {
        card = card.child(fetch_bulk_bar(theme, dialog, cx));
    }
    card = card
        .children(dialog.error.clone().map(|message| {
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
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(foot.join(" · "))),
                )
                .child(
                    action_button(theme)
                        .id("save-fetch")
                        .debug_selector(|| "save-fetch".into())
                        .hover(|style| style.bg(crate::theme::ink(0.04)))
                        .on_click(cx.listener(|page: &mut ProvidersPage, _, _, cx| {
                            page.save_fetch(cx);
                        }))
                        .child(if count == 1 {
                            t!(
                                "settings_providers_usage.providers.record_form.add_one",
                                count = count
                            )
                        } else {
                            t!(
                                "settings_providers_usage.providers.record_form.add_other",
                                count = count
                            )
                        }),
                )
                .child(
                    widgets::ghost_action(theme)
                        .id("cancel-fetch")
                        .hover(move |style| widgets::ghost_hover(theme, style))
                        .on_click(cx.listener(move |page: &mut ProvidersPage, _, _, cx| {
                            page.fetch_dialog = None;
                            cx.notify();
                        }))
                        .child(t!("settings_providers_usage.providers.cancel")),
                ),
        );
    card.into_any_element()
}

fn fetch_column(width: f32) -> gpui::Div {
    div().w(px(width)).flex_none().min_w_0()
}

fn fetch_heading(theme: &Theme, text: Cow<'static, str>) -> gpui::Div {
    div()
        .text_size(crate::typography::ui_rems(10.5))
        .text_color(theme.text_faint)
        .child(SharedString::from(text))
}

/// The column titles, laid out on the rows' own grid so each lines up over
/// its numbers.
fn fetch_table_header(theme: &Theme) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(FETCH_GAP))
        .px(px(12.0))
        .pb(px(8.0))
        .border_b_1()
        .border_color(theme.border)
        .child(fetch_column(FETCH_COLUMNS[0]))
        .child(fetch_column(FETCH_COLUMNS[1]).child(fetch_heading(
            theme,
            t!("settings_providers_usage.providers.record_form.fetch_col_model"),
        )))
        .child(fetch_column(FETCH_COLUMNS[2]).child(fetch_heading(
            theme,
            t!("settings_providers_usage.providers.record_form.fetch_col_context"),
        )))
        .child(fetch_column(FETCH_COLUMNS[3]).child(fetch_heading(
            theme,
            t!("settings_providers_usage.providers.record_form.fetch_col_max_output"),
        )))
        .child(fetch_column(FETCH_COLUMNS[4]).child(fetch_heading(
            theme,
            t!("settings_providers_usage.providers.record_form.fetch_col_levels"),
        )))
        .child(fetch_column(FETCH_COLUMNS[5]).child(fetch_heading(
            theme,
            t!("settings_providers_usage.providers.record_form.fetch_col_price"),
        )))
        .child(fetch_column(FETCH_COLUMNS[6]).child(fetch_heading(
            theme,
            t!("settings_providers_usage.providers.record_form.fetch_col_source"),
        )))
        .into_any_element()
}

/// A small borderless-looking input for a table cell: the numbers in a new
/// row, edited where they are shown.
fn cell_input(theme: &Theme, input: Entity<ComposerInput>) -> gpui::Div {
    div()
        .h(px(26.0))
        .min_w_0()
        .px(px(6.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.input_glass_bg())
        .child(input)
}

/// One thinking level as a chip. A row that can be edited shows all six,
/// on or off; a read-only row shows only the levels it has. An estimate is
/// dashed and amber, so a guess never reads as the catalog's word.
#[allow(clippy::too_many_arguments)]
fn level_chip(
    theme: &Theme,
    label: &str,
    on: bool,
    estimated: bool,
    key: Option<(usize, usize)>,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let mut chip = div()
        .flex_none()
        .h(px(17.0))
        .px(px(4.0))
        .flex()
        .items_center()
        .rounded(px(5.0))
        .border_1()
        .text_size(crate::typography::ui_rems(10.0))
        .child(SharedString::from(label.to_string()));
    if on && estimated {
        chip = chip
            .border_dashed()
            .border_color(theme.warning.opacity(0.6))
            .bg(theme.warning.opacity(0.08))
            .text_color(theme.warning);
    } else if on {
        chip = chip
            .border_color(theme.accent.opacity(0.55))
            .bg(theme.accent_wash)
            .text_color(theme.accent);
    } else {
        chip = chip
            .border_color(theme.border_strong)
            .text_color(theme.text_muted);
    }
    match key {
        None => chip.into_any_element(),
        Some((row, level)) => {
            let name: &'static str = THINKING_LEVELS[level];
            let hover = theme.text;
            chip.id((
                "fetch-level",
                row.wrapping_mul(THINKING_LEVELS.len()).wrapping_add(level),
            ))
            .debug_selector(move || {
                if row == usize::MAX {
                    format!("fetch-bulk-level-{name}")
                } else {
                    format!("fetch-level-{row}-{name}")
                }
            })
            .cursor_pointer()
            .hover(move |style| {
                if on {
                    style
                } else {
                    style.border_color(hover).text_color(hover)
                }
            })
            .on_click(cx.listener(move |page: &mut ProvidersPage, _, _, cx| {
                page.toggle_fetch_level(row, level, cx);
            }))
            .into_any_element()
        }
    }
}

/// Where a row's numbers come from, as a tag at the row's end.
fn fetch_tag(theme: &Theme, text: Cow<'static, str>, kind: FetchTag) -> AnyElement {
    let mut tag = div()
        .flex_none()
        .h(px(17.0))
        .px(px(5.0))
        .flex()
        .items_center()
        .rounded(px(5.0))
        .border_1()
        .text_size(crate::typography::ui_rems(10.0))
        .child(SharedString::from(text));
    tag = match kind {
        FetchTag::Catalog => tag
            .border_color(theme.accent.opacity(0.45))
            .text_color(theme.accent),
        FetchTag::Estimate => tag
            .border_dashed()
            .border_color(theme.warning.opacity(0.6))
            .text_color(theme.warning),
        FetchTag::Plain => tag
            .border_color(theme.border_strong)
            .text_color(theme.text_muted),
    };
    tag.into_any_element()
}

enum FetchTag {
    Catalog,
    Estimate,
    Plain,
}

/// The model half of a `provider/model`: what a row's note names its
/// sibling by.
fn model_half(origin: &str) -> &str {
    origin.rsplit('/').next().unwrap_or(origin)
}

/// `1050000` as `1,050,000`, for a number a person reads.
fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// A price as the catalog states it: `2.5`, not `2.50` nor `2.5000001`.
fn price_text(value: f64) -> String {
    format!("{value}")
}

fn fetch_number(theme: &Theme, text: String) -> AnyElement {
    div()
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(theme.text)
        .child(SharedString::from(text))
        .into_any_element()
}

fn fetch_dash(theme: &Theme) -> AnyElement {
    div()
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(theme.text_faint)
        .child("—")
        .into_any_element()
}

fn fetch_checkbox_state(added: bool, selected: bool) -> widgets::CheckboxState {
    if added || selected {
        widgets::CheckboxState::Checked
    } else {
        widgets::CheckboxState::Unchecked
    }
}

#[cfg(test)]
thread_local! {
    static FETCH_ROW_RENDERS: std::cell::RefCell<Vec<usize>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

/// One listing row. The id line doubles as the new row's image-input
/// switch, which is why an adopted row's id gets the line to itself.
fn fetch_table_row(
    theme: &Theme,
    dialog: &FetchDialog,
    index: usize,
    row: &FetchRow,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    #[cfg(test)]
    FETCH_ROW_RENDERS.with(|renders| {
        let mut renders = renders.borrow_mut();
        if renders.len() <= index {
            renders.resize(index + 1, 0);
        }
        renders[index] += 1;
    });
    let checked = dialog.checked.contains(&row.id);
    let numbers = row.numbers.unwrap_or(FetchNumbers {
        context_window: 0,
        max_tokens: 0,
        input_cost: 0.0,
        output_cost: 0.0,
    });
    let check = if row.added {
        div()
            .flex_none()
            .ml(px(3.0))
            .child(widgets::checkbox(
                theme,
                fetch_checkbox_state(row.added, checked),
            ))
            .opacity(0.35)
            .into_any_element()
    } else {
        div()
            .id(("fetch-check", index))
            .debug_selector(move || format!("fetch-check-{index}"))
            .cursor_pointer()
            .flex()
            .items_center()
            .justify_center()
            .w(px(22.0))
            .h(px(22.0))
            .on_click(cx.listener(move |page: &mut ProvidersPage, _, _, cx| {
                page.toggle_fetch_row(index, cx);
            }))
            .child(widgets::checkbox(
                theme,
                fetch_checkbox_state(row.added, checked),
            ))
            .into_any_element()
    };
    let shown_name = if row.added {
        div()
            .text_size(crate::typography::ui_rems(13.0))
            .text_color(theme.text)
            .truncate()
            .child(SharedString::from(row.name.read(cx).text().to_string()))
            .into_any_element()
    } else {
        cell_input(theme, row.name.clone()).into_any_element()
    };
    let mut id_line = div().flex().items_center().gap(px(6.0)).min_w_0().child(
        div()
            .min_w_0()
            .truncate()
            .font_family(theme.font_mono.clone())
            .text_size(crate::typography::ui_rems(11.0))
            .text_color(theme.text_muted)
            .child(SharedString::from(row.id.clone())),
    );
    if row.inputs.is_some() {
        id_line = id_line.child(
            div()
                .id(("fetch-image", index))
                .debug_selector(move || format!("fetch-image-{index}"))
                .cursor_pointer()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(4.0))
                .on_click(cx.listener(move |page: &mut ProvidersPage, _, _, cx| {
                    page.toggle_fetch_image(index, cx);
                }))
                .child(widgets::checkbox(
                    theme,
                    if row.image {
                        widgets::CheckboxState::Checked
                    } else {
                        widgets::CheckboxState::Unchecked
                    },
                ))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(theme.text_muted)
                        .child(t!(
                            "settings_providers_usage.providers.record_form.fetch_image_short"
                        )),
                ),
        );
    }
    let (context, max_output, price) = match &row.inputs {
        Some(inputs) => (
            cell_input(theme, inputs.context_window.clone()).into_any_element(),
            cell_input(theme, inputs.max_tokens.clone()).into_any_element(),
            div()
                .flex()
                .items_center()
                .gap(px(3.0))
                .child(cell_input(theme, inputs.input_cost.clone()).w(px(34.0)))
                .child(div().text_color(theme.text_faint).child("·"))
                .child(cell_input(theme, inputs.output_cost.clone()).w(px(42.0)))
                .into_any_element(),
        ),
        None if row.added && numbers.context_window == 0 => {
            (fetch_dash(theme), fetch_dash(theme), fetch_dash(theme))
        }
        None if row.added => (
            fetch_number(theme, thousands(numbers.context_window)),
            fetch_dash(theme),
            fetch_dash(theme),
        ),
        None => (
            fetch_number(theme, thousands(numbers.context_window)),
            fetch_number(theme, thousands(numbers.max_tokens)),
            fetch_number(
                theme,
                format!(
                    "${} · ${}",
                    price_text(numbers.input_cost),
                    price_text(numbers.output_cost)
                ),
            ),
        ),
    };
    let chips: Vec<AnyElement> = if row.opening_levels.is_some() {
        THINKING_LEVELS
            .iter()
            .enumerate()
            .map(|(level, name)| {
                level_chip(
                    theme,
                    name,
                    row.levels.iter().any(|chosen| chosen == name),
                    row.estimated,
                    Some((index, level)),
                    cx,
                )
            })
            .collect()
    } else {
        row.levels
            .iter()
            .filter_map(|chosen| {
                THINKING_LEVELS
                    .iter()
                    .find(|name| **name == chosen.as_str())
            })
            .map(|name| level_chip(theme, name, true, row.estimated, None, cx))
            .collect()
    };
    let note: Option<(gpui::Hsla, Cow<'static, str>)> =
        if let Some(from) = row.estimated_from.as_ref().filter(|_| row.estimated) {
            Some((
                theme.warning,
                t!(
                    "settings_providers_usage.providers.record_form.fetch_estimated_from",
                    source = model_half(from)
                ),
            ))
        } else if row.estimated {
            Some((
                theme.warning,
                t!("settings_providers_usage.providers.record_form.fetch_estimated_default"),
            ))
        } else {
            row.starts_from.as_ref().map(|from| {
                (
                    theme.text_muted,
                    t!(
                        "settings_providers_usage.providers.record_form.fetch_starts_from",
                        source = model_half(from)
                    ),
                )
            })
        };
    let tag = if row.added {
        fetch_tag(
            theme,
            t!("settings_providers_usage.providers.record_form.fetch_tag_added"),
            FetchTag::Plain,
        )
    } else if row.source.is_some() && row.estimated {
        fetch_tag(
            theme,
            t!("settings_providers_usage.providers.record_form.fetch_tag_estimated"),
            FetchTag::Estimate,
        )
    } else if row.source.is_some() {
        fetch_tag(
            theme,
            t!("settings_providers_usage.providers.record_form.fetch_tag_catalog"),
            FetchTag::Catalog,
        )
    } else {
        fetch_tag(
            theme,
            t!("settings_providers_usage.providers.record_form.fetch_tag_new"),
            FetchTag::Plain,
        )
    };
    let hover = theme.element_hover;
    div()
        .id(("fetch-row", index))
        .debug_selector(move || format!("fetch-row-{index}"))
        .flex()
        .items_center()
        .gap(px(FETCH_GAP))
        .px(px(12.0))
        .py(px(10.0))
        .rounded(px(8.0))
        .border_b_1()
        .border_color(theme.border.opacity(0.5))
        .when(!row.added, |row_el| {
            row_el.hover(move |style| style.bg(hover))
        })
        .when(row.added, |row_el| row_el.opacity(0.45))
        .child(fetch_column(FETCH_COLUMNS[0]).child(check))
        .child(
            fetch_column(FETCH_COLUMNS[1])
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(shown_name)
                .child(id_line),
        )
        .child(fetch_column(FETCH_COLUMNS[2]).child(context))
        .child(fetch_column(FETCH_COLUMNS[3]).child(max_output))
        .child(
            fetch_column(FETCH_COLUMNS[4])
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(if chips.is_empty() {
                    div()
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_faint)
                        .child(t!(
                            "settings_providers_usage.providers.record_form.fetch_no_thinking"
                        ))
                        .into_any_element()
                } else {
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(4.0))
                        .children(chips)
                        .into_any_element()
                })
                .children(note.map(|(color, text)| {
                    div()
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(color)
                        .child(SharedString::from(text))
                })),
        )
        .child(fetch_column(FETCH_COLUMNS[5]).child(price))
        .child(fetch_column(FETCH_COLUMNS[6]).child(tag))
        .into_any_element()
}

/// One set of numbers for every new row, for a listing that brought
/// several: the rows start uniform, and "Apply" writes this set into each.
fn fetch_bulk_bar(
    theme: &Theme,
    dialog: &FetchDialog,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let Some(bulk) = dialog.bulk.as_ref() else {
        return div().into_any_element();
    };
    let fresh = dialog
        .rows
        .iter()
        .filter(|row| row.source.is_none() && !row.added)
        .count();
    let bulk_key = usize::MAX;
    div()
        .flex()
        .items_center()
        .gap(px(10.0))
        .flex_wrap()
        .px(px(10.0))
        .py(px(8.0))
        .rounded(px(8.0))
        .border_1()
        .border_dashed()
        .border_color(theme.border_strong)
        .child(
            div()
                .text_size(crate::typography::ui_rems(11.5))
                .text_color(theme.text_muted)
                .child(t!(
                    "settings_providers_usage.providers.record_form.fetch_bulk_title"
                )),
        )
        .child(cell_input(theme, bulk.context_window.clone()).w(px(92.0)))
        .child(cell_input(theme, bulk.max_tokens.clone()).w(px(92.0)))
        .child(div().flex().items_center().gap(px(3.0)).children(
            THINKING_LEVELS.iter().enumerate().map(|(level, name)| {
                level_chip(
                    theme,
                    name,
                    bulk.levels.iter().any(|chosen| chosen == name),
                    false,
                    Some((bulk_key, level)),
                    cx,
                )
            }),
        ))
        .child(
            div()
                .id("fetch-bulk-image")
                .debug_selector(|| "fetch-bulk-image".into())
                .cursor_pointer()
                .flex()
                .items_center()
                .gap(px(4.0))
                .on_click(cx.listener(|page: &mut ProvidersPage, _, _, cx| {
                    page.toggle_fetch_bulk_image(cx);
                }))
                .child(widgets::checkbox(
                    theme,
                    if bulk.image {
                        widgets::CheckboxState::Checked
                    } else {
                        widgets::CheckboxState::Unchecked
                    },
                ))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(theme.text_muted)
                        .child(t!(
                            "settings_providers_usage.providers.record_form.fetch_image_short"
                        )),
                ),
        )
        .child(div().flex_1())
        .child(
            action_button(theme)
                .id("fetch-apply-all")
                .debug_selector(|| "fetch-apply-all".into())
                .hover(|style| style.bg(crate::theme::ink(0.04)))
                .on_click(cx.listener(|page: &mut ProvidersPage, _, _, cx| {
                    page.apply_fetch_bulk(cx);
                }))
                .child(t!(
                    "settings_providers_usage.providers.record_form.fetch_apply_all",
                    count = fresh
                )),
        )
        .into_any_element()
}

/// Offer or withdraw one level, keeping the ladder's order.
fn flip_level(levels: &mut Vec<String>, level: &str) {
    if let Some(at) = levels.iter().position(|chosen| chosen == level) {
        levels.remove(at);
    } else {
        levels.push(level.to_string());
        levels.sort_by_key(|chosen| {
            THINKING_LEVELS
                .iter()
                .position(|name| name == chosen)
                .unwrap_or(THINKING_LEVELS.len())
        });
    }
}

/// Builds the model record the engine expects from the manual record
/// form's texts: numbers parse, costs default to zero, and the advanced
/// JSON (when present) must be an object whose keys ride along — except
/// the form's own fields, which the advanced object can never override.
/// `baseUrl` is not a form field: empty means omitted, and the engine
/// fills the provider's default endpoint (Advanced JSON may still set it).
pub(super) fn build_record_json(
    provider: &str,
    texts: &HashMap<String, String>,
    reasoning: bool,
    image: bool,
) -> Result<serde_json::Value, String> {
    let text = |key: &str| texts.get(key).cloned().unwrap_or_default();
    let id = text("id");
    if id.is_empty() {
        return Err(
            t!("settings_providers_usage.providers.record_form.error_id_required").into_owned(),
        );
    }
    let name = {
        let name = text("name");
        if name.is_empty() { id.clone() } else { name }
    };
    let base_url = text("baseUrl");
    if !base_url.is_empty() && !base_url.starts_with("http://") && !base_url.starts_with("https://")
    {
        return Err(
            t!("settings_providers_usage.providers.add_dialog.error_base_url_scheme").into_owned(),
        );
    }
    let api = text("api");
    if api.is_empty() {
        return Err(
            t!("settings_providers_usage.providers.record_form.error_api_required").into_owned(),
        );
    }
    let number = |key: &str| -> Result<u64, String> {
        let raw = text(key);
        raw.parse::<u64>().map_err(|_| {
            t!(
                "settings_providers_usage.providers.record_form.error_whole_number",
                field = record_field_label(key)
            )
            .into_owned()
        })
    };
    let context_window = number("contextWindow")?;
    if context_window == 0 {
        return Err(
            t!("settings_providers_usage.providers.record_form.error_window_zero").into_owned(),
        );
    }
    let max_tokens = number("maxTokens")?;
    let cost = |key: &str| -> Result<f64, String> {
        let raw = text(key);
        if raw.is_empty() {
            return Ok(0.0);
        }
        raw.parse::<f64>().map_err(|_| {
            t!(
                "settings_providers_usage.providers.record_form.error_number",
                field = record_field_label(key)
            )
            .into_owned()
        })
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
        let parsed: serde_json::Value = serde_json::from_str(&advanced).map_err(|error| {
            t!(
                "settings_providers_usage.providers.record_form.error_advanced_parse",
                error = error
            )
            .into_owned()
        })?;
        let Some(object) = parsed.as_object() else {
            return Err(
                t!("settings_providers_usage.providers.record_form.error_advanced_object")
                    .into_owned(),
            );
        };
        if let Some(target) = record.as_object_mut() {
            for (key, value) in object {
                // The form's fields are authoritative — the advanced object
                // carries only what the form does not own (baseUrl included:
                // omitting it is the default-endpoint case). A cost is the
                // form's but for its tiers, which it has no field for.
                if FORM_OWNED_KEYS.contains(&key.as_str()) {
                    if key == "cost"
                        && let Some(tiers) = value.get("tiers").filter(|tiers| !tiers.is_null())
                        && let Some(cost) = target.get_mut("cost")
                    {
                        cost["tiers"] = tiers.clone();
                    }
                    continue;
                }
                target.insert(key.clone(), value.clone());
            }
        }
    }
    Ok(record)
}

impl ProvidersPage {
    /// The Models header's "Fetch models": a fresh ProbeProvider call; the
    /// dialog opens only on `ok` — the status line carries the failure
    /// verdict, as it does a preview the engine could not answer. The
    /// engine's preview comes back on the same call and says which ids the
    /// provider already offers; the panel's model list is asked for as well
    /// — it also names an id the engine's exact test misses, a listing that
    /// gives it provider-qualified — and again when that list lands
    /// ([`Self::fetch_models_loaded`]): a listing opened before the list
    /// did used to offer the provider's own models back, and Add filed blank
    /// records over the catalog's.
    pub(super) fn open_fetch(&mut self, provider: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.load_models(&provider, false, cx);
        self.probes.insert(provider.clone(), ProbeState::Running);
        self.task = Some(cx.spawn(async move |this, cx| {
            let probed = engine
                .client()
                .call(
                    methods::PROBE_PROVIDER,
                    serde_json::json!({ "providerId": provider }),
                )
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<ProbeOutcome>(value)
                        .map_err(|error| error.to_string())
                });
            // What the engine would file for the listing rides the same
            // task: the dialog opens with a complete, matching set of rows
            // or not at all. Filing a row on the plain defaults because its
            // facts were missing would overwrite the catalog's facts — the
            // thing this dialog exists to stop.
            let facts: Result<Vec<FetchedFacts>, String> = match &probed {
                Ok(outcome) if outcome.ok && outcome.dialect.is_some() => engine
                    .client()
                    .call(
                        methods::PREVIEW_FETCHED_MODELS,
                        serde_json::json!({
                            "providerId": provider,
                            "modelIds": outcome.model_ids,
                        }),
                    )
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|value| {
                        serde_json::from_value(value).map_err(|error| error.to_string())
                    }),
                // A probe that did not open a dialog has nothing to preview.
                _ => Ok(Vec::new()),
            };
            this.update(cx, |page, cx| {
                match (probed, facts) {
                    (Ok(outcome), Ok(facts)) => {
                        let ok = outcome.ok;
                        let has_transport = outcome.dialect.is_some();
                        let ids = outcome.model_ids.clone();
                        page.probes
                            .insert(provider.clone(), ProbeState::Done(outcome));
                        if ok && !has_transport {
                            // Nothing to file the rows under: the engine
                            // resolved no dialect for the provider.
                            page.fail(
                                t!("settings_providers_usage.providers.record_form.error_no_transport")
                                    .into_owned(),
                                cx,
                            );
                        } else if ok
                            && let Some(problem) =
                                page.open_fetch_dialog(provider, ids, facts, cx)
                        {
                            page.fail(problem, cx);
                        }
                    }
                    // The listing came back and its rows did not: nothing
                    // opens, and there is nothing for Add to file.
                    (Ok(outcome), Err(error)) => {
                        page.probes.insert(provider.clone(), ProbeState::Done(outcome));
                        page.fail(error, cx);
                    }
                    (Err(error), _) => {
                        page.probes.remove(&provider);
                        page.fail(error, cx);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Build the dialog from a probed listing: one row per id, in listing
    /// order. The preview must answer exactly those ids, once each and in
    /// the same order, before any row is built; otherwise the whole dialog
    /// is refused rather than filling missing facts with plain defaults. An
    /// id the provider already offers stays in view, dimmed and never
    /// selected — the engine's `offered` says so, where the panel's list
    /// hides the row — and when that is all the listing holds (including an
    /// empty listing), the dialog does not open. A problem string means the
    /// dialog did not open.
    fn open_fetch_dialog(
        &mut self,
        provider: String,
        ids: Vec<String>,
        facts: Vec<FetchedFacts>,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let mut seen = HashSet::with_capacity(facts.len());
        if facts.len() != ids.len()
            || !facts
                .iter()
                .zip(&ids)
                .all(|(facts, id)| facts.id == *id && seen.insert(facts.id.as_str()))
        {
            return Some(
                t!("settings_providers_usage.providers.record_form.error_preview_mismatch")
                    .into_owned(),
            );
        }
        let listed = self
            .models
            .get(&provider)
            .and_then(|models| models.ready())
            .cloned()
            .unwrap_or_default();
        let input = |text: &str, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut input = ComposerInput::new(text, cx);
                input.set_text(text.to_string(), cx);
                input
            })
        };
        let mut rows = Vec::with_capacity(ids.len());
        for (id, facts) in ids.iter().zip(&facts) {
            // The engine's word first — it refuses the save on the same
            // test — and the panel's list for its provider-qualified ids:
            // that list hides rows the provider still offers.
            let added = facts.offered
                || listed
                    .iter()
                    .any(|model| model.id == *id || model.id == format!("{provider}/{id}"));
            let new = added || facts.source.is_none();
            let (name, numbers, levels, estimated, estimated_from, starts_from, source) = (
                facts.name.clone(),
                Some(FetchNumbers {
                    context_window: facts.context_window,
                    max_tokens: facts.max_tokens,
                    input_cost: facts.cost.input,
                    output_cost: facts.cost.output,
                }),
                facts.reasoning_levels.clone(),
                facts.levels_estimated,
                facts.estimated_from.clone(),
                facts.starts_from.clone(),
                facts.source.clone(),
            );
            let can_turn_off = facts.can_turn_off;
            let image = facts.input.iter().any(|input| input == "image");
            let opening = numbers.unwrap_or(FetchNumbers {
                context_window: 0,
                max_tokens: 0,
                input_cost: 0.0,
                output_cost: 0.0,
            });
            let inputs = (new && !added).then(|| FetchInputs {
                context_window: input(&opening.context_window.to_string(), cx),
                max_tokens: input(&opening.max_tokens.to_string(), cx),
                input_cost: input(&price_text(opening.input_cost), cx),
                output_cost: input(&price_text(opening.output_cost), cx),
            });
            rows.push(FetchRow {
                id: id.clone(),
                source,
                starts_from,
                added,
                name: cx.new(|cx| {
                    let mut input = ComposerInput::new(name.as_str(), cx);
                    input.set_text(name.clone(), cx);
                    input
                }),
                numbers,
                inputs,
                levels: levels.clone(),
                // An adopted row whose levels are an estimate can be
                // corrected here, and nowhere else in this dialog.
                // A new row's levels are its switches; an adopted row's only
                // when they are an estimate.
                opening_levels: (!added && (new || estimated)).then_some(levels),
                estimated,
                estimated_from,
                can_turn_off,
                image,
            });
        }
        if rows.iter().all(|row| row.added) {
            return Some(
                t!("settings_providers_usage.providers.record_form.error_nothing_new").into_owned(),
            );
        }
        let mut fresh = rows.iter().filter(|row| row.source.is_none() && !row.added);
        let first = fresh.next();
        // One set of numbers for several new rows at once; with a single
        // one there is nothing to apply it to.
        let bulk = first
            .filter(|_| fresh.next().is_some())
            .map(|row| FetchBulk {
                context_window: input(
                    &row.numbers.map_or_else(
                        || "0".to_string(),
                        |numbers| numbers.context_window.to_string(),
                    ),
                    cx,
                ),
                max_tokens: input(
                    &row.numbers
                        .map_or_else(|| "0".to_string(), |numbers| numbers.max_tokens.to_string()),
                    cx,
                ),
                levels: row.levels.clone(),
                image: row.image,
            });
        let checked = rows
            .iter()
            .filter(|row| !row.added)
            .map(|row| row.id.clone())
            .collect();
        self.fetch_dialog = Some(FetchDialog {
            provider,
            rows,
            checked,
            bulk,
            saving: false,
            error: None,
        });
        None
    }

    /// The panel's model list landed: from now on the dialog marks the ids
    /// the provider already offers and drops them from the selection, so a
    /// listing that opened before the list did cannot add them a second
    /// time.
    pub(super) fn fetch_models_loaded(&mut self, provider: &str, cx: &mut Context<Self>) {
        if !self
            .fetch_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.provider == provider)
        {
            return;
        }
        let Some(listed) = self
            .models
            .get(provider)
            .and_then(|models| models.ready())
            .map(|models| {
                models
                    .iter()
                    .map(|model| model.id.clone())
                    .collect::<Vec<_>>()
            })
        else {
            return;
        };
        let Some(dialog) = self.fetch_dialog.as_mut() else {
            return;
        };
        for row in &mut dialog.rows {
            if listed
                .iter()
                .any(|id| id == &row.id || id == &format!("{provider}/{}", row.id))
            {
                row.added = true;
                dialog.checked.remove(&row.id);
            }
        }
        cx.notify();
    }

    pub(super) fn toggle_fetch_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(dialog) = self.fetch_dialog.as_mut() else {
            return;
        };
        if dialog.saving {
            return;
        }
        let Some(row) = dialog.rows.get(index).filter(|row| !row.added) else {
            return;
        };
        if !dialog.checked.remove(&row.id) {
            dialog.checked.insert(row.id.clone());
        }
        cx.notify();
    }

    /// Flip one level of a row whose levels can be edited; `usize::MAX`
    /// addresses the set shared by every new row.
    pub(super) fn toggle_fetch_level(&mut self, row: usize, level: usize, cx: &mut Context<Self>) {
        let Some(dialog) = self.fetch_dialog.as_mut() else {
            return;
        };
        if dialog.saving {
            return;
        }
        let Some(level) = THINKING_LEVELS.get(level) else {
            return;
        };
        let levels = if row == usize::MAX {
            dialog.bulk.as_mut().map(|bulk| &mut bulk.levels)
        } else {
            dialog
                .rows
                .get_mut(row)
                .filter(|row| row.opening_levels.is_some())
                .map(|row| &mut row.levels)
        };
        if let Some(levels) = levels {
            flip_level(levels, level);
        }
        cx.notify();
    }

    pub(super) fn toggle_fetch_image(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(dialog) = self.fetch_dialog.as_mut() else {
            return;
        };
        if dialog.saving {
            return;
        }
        if let Some(row) = dialog
            .rows
            .get_mut(index)
            .filter(|row| row.inputs.is_some())
        {
            row.image = !row.image;
        }
        cx.notify();
    }

    pub(super) fn toggle_fetch_bulk_image(&mut self, cx: &mut Context<Self>) {
        if let Some(bulk) = self
            .fetch_dialog
            .as_mut()
            .filter(|dialog| !dialog.saving)
            .and_then(|dialog| dialog.bulk.as_mut())
        {
            bulk.image = !bulk.image;
        }
        cx.notify();
    }

    /// Writes the shared set into every new row's inputs, levels, and image
    /// flag — a row edited before keeps nothing of its own numbers.
    pub(super) fn apply_fetch_bulk(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.fetch_dialog.as_ref().filter(|dialog| !dialog.saving) else {
            return;
        };
        let Some(bulk) = dialog.bulk.as_ref() else {
            return;
        };
        let context_window = bulk.context_window.read(cx).text().to_string();
        let max_tokens = bulk.max_tokens.read(cx).text().to_string();
        let (levels, image) = (bulk.levels.clone(), bulk.image);
        let inputs = dialog
            .rows
            .iter()
            .filter(|row| row.inputs.is_some())
            .filter_map(|row| row.inputs.clone())
            .collect::<Vec<_>>();
        for inputs in &inputs {
            inputs
                .context_window
                .update(cx, |input, cx| input.set_text(context_window.clone(), cx));
            inputs
                .max_tokens
                .update(cx, |input, cx| input.set_text(max_tokens.clone(), cx));
        }
        let Some(dialog) = self.fetch_dialog.as_mut() else {
            return;
        };
        for row in dialog.rows.iter_mut().filter(|row| row.inputs.is_some()) {
            row.levels = levels.clone();
            row.image = image;
        }
        cx.notify();
    }

    /// One `SaveFetchedModels` for every checked row, in listing order, in
    /// one commit. An adopted row sends its name, and the levels only where
    /// it corrected an estimate; a new row sends everything it shows. The
    /// first problem a row has stops the batch and shows in the dialog with
    /// the selection intact.
    pub(super) fn save_fetch(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(dialog) = self.fetch_dialog.as_ref() else {
            return;
        };
        if dialog.saving {
            return;
        }
        let mut models = Vec::new();
        let mut problem = None;
        for row in dialog
            .rows
            .iter()
            .filter(|row| dialog.checked.contains(&row.id))
        {
            let name = row.name.read(cx).text().trim().to_string();
            let mut model = serde_json::json!({ "id": row.id, "name": name });
            match row.source.as_ref() {
                Some(_) => {
                    if let Some(opening) = &row.opening_levels
                        && opening != &row.levels
                    {
                        model["thinkingLevels"] = serde_json::json!(row.levels);
                        model["canTurnOff"] = serde_json::json!(row.can_turn_off);
                    }
                }
                None => {
                    let Some(inputs) = &row.inputs else {
                        continue;
                    };
                    let number = |input: &Entity<ComposerInput>, key: &'static str| {
                        input.read(cx).text().trim().parse::<u64>().map_err(|_| {
                            t!(
                                "settings_providers_usage.providers.record_form.error_whole_number",
                                field = record_field_label(key)
                            )
                            .into_owned()
                        })
                    };
                    let cost =
                        |input: &Entity<ComposerInput>, key: &'static str| {
                            let raw = input.read(cx).text().trim().to_string();
                            if raw.is_empty() {
                                Ok(0.0)
                            } else {
                                raw.parse::<f64>().map_err(|_| {
                                t!("settings_providers_usage.providers.record_form.error_number",
                                    field = record_field_label(key))
                                .into_owned()
                            })
                            }
                        };
                    let parsed = match (
                        number(&inputs.context_window, "contextWindow"),
                        number(&inputs.max_tokens, "maxTokens"),
                        cost(&inputs.input_cost, "inputCost"),
                        cost(&inputs.output_cost, "outputCost"),
                    ) {
                        (Ok(context), Ok(max), Ok(input), Ok(output)) => {
                            Ok((context, max, input, output))
                        }
                        (Err(problem), _, _, _)
                        | (_, Err(problem), _, _)
                        | (_, _, Err(problem), _)
                        | (_, _, _, Err(problem)) => Err(problem),
                    };
                    match parsed {
                        Ok((0, ..)) => problem = Some(
                            t!("settings_providers_usage.providers.record_form.error_window_zero")
                                .into_owned(),
                        ),
                        Ok((context, max, input, output)) => {
                            model["contextWindow"] = serde_json::json!(context);
                            model["maxTokens"] = serde_json::json!(max);
                            model["cost"] = serde_json::json!({
                                "input": input, "output": output, "cacheRead": 0.0, "cacheWrite": 0.0,
                            });
                            model["thinkingLevels"] = serde_json::json!(row.levels);
                            model["canTurnOff"] = serde_json::json!(row.can_turn_off);
                            model["image"] = serde_json::json!(row.image);
                            if let Some(from) = &row.starts_from {
                                model["startsFrom"] = serde_json::json!(from);
                            }
                        }
                        Err(found) => problem = Some(found),
                    }
                }
            }
            if problem.is_some() {
                break;
            }
            models.push(model);
        }
        if let Some(problem) = problem {
            if let Some(dialog) = self.fetch_dialog.as_mut() {
                dialog.error = Some(problem);
            }
            cx.notify();
            return;
        }
        if models.is_empty() {
            self.fetch_dialog = None;
            cx.notify();
            return;
        }
        let provider = dialog.provider.clone();
        if let Some(dialog) = self.fetch_dialog.as_mut() {
            dialog.saving = true;
            dialog.error = None;
        }
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SAVE_FETCHED_MODELS,
                    serde_json::json!({ "providerId": provider, "models": models }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.fetch_dialog = None;
                        crate::pickers::bump_provider_catalog(cx);
                        page.load_models(&provider, true, cx);
                        page.load_hidden(&provider, true, cx);
                    }
                    Err(error) => {
                        if let Some(dialog) = page.fetch_dialog.as_mut() {
                            dialog.saving = false;
                            dialog.error = Some(error.to_string());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The record form over a row that already exists — its fields, its
    /// dialect, and everything else it carries in the advanced JSON — so a
    /// correction saved under the same id replaces the row without dropping
    /// what the form cannot show. `None` opens the form empty. Opening
    /// retires every record fetch still in flight
    /// ([`Self::edit_model`]): none of them may replace this form.
    pub(super) fn open_record_form(
        &mut self,
        provider: String,
        existing: Option<serde_json::Value>,
        cx: &mut Context<Self>,
    ) {
        self.record_form_epoch += 1;
        let editing = existing.is_some();
        let existing = existing.unwrap_or_else(|| serde_json::json!({}));
        let text = |key: &str, fallback: &str| -> String {
            match &existing[key] {
                serde_json::Value::Null => fallback.to_string(),
                serde_json::Value::String(value) => value.clone(),
                value => value.to_string(),
            }
        };
        let mut inputs = HashMap::new();
        for (key, _, placeholder) in record_fields() {
            let prefill = match key {
                "id" | "name" => text(key, ""),
                "contextWindow" | "maxTokens" => text(key, placeholder),
                "inputCost" => cost_text(&existing["cost"]["input"], placeholder),
                "outputCost" => cost_text(&existing["cost"]["output"], placeholder),
                "cacheReadCost" => cost_text(&existing["cost"]["cacheRead"], placeholder),
                "cacheWriteCost" => cost_text(&existing["cost"]["cacheWrite"], placeholder),
                _ => unreachable!("record_fields names only these keys"),
            };
            inputs.insert(
                key.to_string(),
                cx.new(|cx| {
                    let mut input = ComposerInput::new(placeholder, cx);
                    if !prefill.is_empty() {
                        input.set_text(prefill, cx);
                    }
                    input
                }),
            );
        }
        // Everything the form does not own rides along verbatim: the
        // ladder, `compat`, the endpoint, an adoption's markers.
        let mut advanced: serde_json::Map<String, serde_json::Value> = existing
            .as_object()
            .map(|record| {
                record
                    .iter()
                    .filter(|(key, _)| !FORM_OWNED_KEYS.contains(&key.as_str()))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        // A cost's rates are the form's; its tiers it has no field for, so
        // they ride here and go back under the rates on save.
        if let Some(tiers) = existing["cost"]
            .get("tiers")
            .filter(|tiers| !tiers.is_null())
        {
            advanced.insert("cost".to_string(), serde_json::json!({ "tiers": tiers }));
        }
        inputs.insert(
            "advanced".to_string(),
            cx.new(|cx| {
                let mut input = ComposerInput::new(
                    "{ \"thinkingLevelMap\": …, \"compat\": … } — optional JSON",
                    cx,
                );
                if !advanced.is_empty() {
                    let text = serde_json::to_string_pretty(&advanced).unwrap_or_default();
                    input.set_text(text, cx);
                }
                input
            }),
        );
        // A stale menu from a previous form must not carry over.
        self.record_api_menu = Popup::default();
        self.load_api_dialects(cx);
        self.record_form = Some(RecordForm {
            editing,
            provider,
            api: text("api", "openai-completions"),
            reasoning: existing["reasoning"].as_bool().unwrap_or(false),
            image: existing["input"]
                .as_array()
                .is_some_and(|input| input.contains(&serde_json::json!("image"))),
            inputs,
            error: None,
        });
        cx.notify();
    }

    /// The row's record as the engine serves it, for the form to open on.
    /// The reply lands only if no form was opened since the request left:
    /// an Add clicked meanwhile holds a draft of its own.
    pub(super) fn edit_model(&mut self, provider: String, model: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let epoch = self.record_form_epoch;
        self.task = Some(cx.spawn(async move |this, cx| {
            let record = engine
                .client()
                .call(
                    methods::GET_MODEL_RECORD,
                    serde_json::json!({ "providerId": provider, "modelId": model }),
                )
                .await;
            this.update(cx, |page, cx| {
                if page.record_form_epoch != epoch {
                    return;
                }
                match record {
                    Ok(record) => page.open_record_form(provider, Some(record), cx),
                    Err(error) => page.fail(error.to_string(), cx),
                }
            })
            .ok();
        }));
    }

    /// The dialect dropdown's options: the engine's registered api
    /// dialects (holt-core's registry), loaded once per page. Detached — a
    /// cancelled load would wedge the list in Loading forever.
    pub(super) fn load_api_dialects(&mut self, cx: &mut Context<Self>) {
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

    pub(super) fn toggle_record_api_menu(&mut self, cx: &mut Context<Self>) {
        if self.record_api_menu.take_press_was_open() || self.record_api_menu.is_open() {
            self.close_record_api_menu(cx);
        } else {
            self.record_api_menu.open(());
        }
        cx.notify();
    }

    pub(super) fn close_record_api_menu(&mut self, cx: &mut Context<Self>) {
        if self.record_api_menu.begin_close() {
            popover::reap_popup(cx, |page: &mut ProvidersPage| &mut page.record_api_menu);
            cx.notify();
        }
    }

    pub(super) fn pick_record_api(&mut self, dialect: String, cx: &mut Context<Self>) {
        if let Some(form) = self.record_form.as_mut() {
            form.api = dialect;
        }
        self.close_record_api_menu(cx);
        cx.notify();
    }

    pub(super) fn toggle_record_flag(&mut self, flag: &str, cx: &mut Context<Self>) {
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

    pub(super) fn save_record(&mut self, cx: &mut Context<Self>) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::providers::test_support::*;

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
        // The assertions pin the English error copy: hold the locale lock
        // and pin the locale so a concurrent switching test can't flip it.
        let _guard = crate::i18n::tests::locale_lock();
        crate::i18n::tests::reset_locale();
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

    /// A cost's tiers have no field: they ride in the advanced JSON, and a
    /// save puts them back under the form's rates — which the same object
    /// still cannot override.
    #[test]
    fn a_cost_s_tiers_ride_in_the_advanced_json_under_the_form_s_rates() {
        let texts = record_texts(&[
            ("id", "acme-1"),
            ("api", "openai-completions"),
            ("contextWindow", "1000"),
            ("maxTokens", "100"),
            ("inputCost", "2.5"),
            (
                "advanced",
                r#"{"cost": {"input": 99, "tiers": [{"input": 10, "inputTokensAbove": 272000}]}}"#,
            ),
        ]);
        let record = build_record_json("acme", &texts, false, false).unwrap();
        assert_eq!(
            record["cost"]["input"].as_f64(),
            Some(2.5),
            "the rate is the form's"
        );
        assert_eq!(
            record["cost"]["tiers"],
            serde_json::json!([{ "input": 10, "inputTokensAbove": 272000 }])
        );
    }

    /// The record form's API dialect is a dropdown over the engine's
    /// registered dialects, not a free-text field: the pick lands in the
    /// saved record.
    #[gpui::test]
    fn the_record_form_picks_the_api_dialect_from_a_dropdown(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
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

    /// The manual form's Reasoning / Image input rows are click targets,
    /// and what they hold is what the saved record carries.
    #[gpui::test]
    fn the_record_forms_flag_rows_reach_the_saved_record(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("toggle-record-form");
        harness.click("record-flag-reasoning");
        harness.click("record-flag-image");

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
        assert_eq!(records[0]["record"]["reasoning"], true);
        assert_eq!(
            records[0]["record"]["input"],
            serde_json::json!(["text", "image"])
        );
    }

    /// Every hover transition repaints; a stationary pointer does not.
    #[gpui::test]
    fn hover_transitions_repaint_without_redundant_frames(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        let ids: Vec<String> = (0..13).map(|n| format!("glm-9-{n}")).collect();
        *harness.engine.probe_ids.lock().unwrap() = ids.clone();
        for id in &ids {
            let mut row = preview_row(id);
            row["levelsEstimated"] = serde_json::json!(true);
            row["estimatedFrom"] = serde_json::json!("zai-coding-cn/glm-5.2");
            harness.engine.preview_rows.lock().unwrap().push(row);
        }
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("fetch-vendor");
        harness.pump();

        let target = harness.visual.debug_bounds("fetch-row-2").unwrap();
        let adjacent = harness.visual.debug_bounds("fetch-row-3").unwrap();
        let next = harness.visual.debug_bounds("fetch-row-4").unwrap();
        assert_eq!(
            target.size.height,
            gpui::px(68.0),
            "editable rows keep their natural height"
        );
        assert!(target.bottom() <= adjacent.top());
        assert!(adjacent.bottom() <= next.top());
        harness.visual.simulate_mouse_move(
            gpui::point(gpui::px(0.0), gpui::px(0.0)),
            None,
            gpui::Modifiers::default(),
        );
        let renders = || FETCH_ROW_RENDERS.with(|renders| renders.borrow().clone());
        let assert_repaint = || {
            let counts = renders();
            assert_eq!(counts.len(), ids.len());
            assert!(
                counts.iter().all(|count| *count > 0),
                "every row repaints with the page: {counts:?}"
            );
        };

        FETCH_ROW_RENDERS.with(|renders| renders.borrow_mut().clear());
        harness
            .visual
            .simulate_mouse_move(target.center(), None, gpui::Modifiers::default());
        assert_repaint();

        let after_enter = renders();
        harness
            .visual
            .simulate_mouse_move(target.center(), None, gpui::Modifiers::default());
        assert_eq!(renders(), after_enter, "staying put does no more work");

        FETCH_ROW_RENDERS.with(|renders| renders.borrow_mut().clear());
        harness
            .visual
            .simulate_mouse_move(next.center(), None, gpui::Modifiers::default());
        assert_repaint();

        FETCH_ROW_RENDERS.with(|renders| renders.borrow_mut().clear());
        harness.visual.simulate_mouse_move(
            gpui::point(gpui::px(0.0), gpui::px(0.0)),
            None,
            gpui::Modifiers::default(),
        );
        assert_repaint();

        FETCH_ROW_RENDERS.with(|renders| renders.borrow_mut().clear());
        harness.click("fetch-check-2");
        assert_repaint();
    }

    /// A canned preview row, as `PreviewFetchedModels` answers for an id the
    /// catalog carries: its facts, and where they come from.
    fn preview_row(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "name": "GPT-5.4",
            "source": "openai/gpt-5.4",
            "startsFrom": null,
            "levelsEstimated": false,
            "estimatedFrom": null,
            "canTurnOff": true,
            "contextWindow": 1050000,
            "maxTokens": 128000,
            "reasoningLevels": ["low", "medium", "high", "xhigh"],
            "input": ["text", "image"],
            "cost": { "input": 2.5, "output": 15.0, "cacheRead": 0.25, "cacheWrite": 0.0 }
        })
    }

    fn capture_errors(
        harness: &mut ProvidersHarness<'_>,
    ) -> (std::sync::Arc<std::sync::Mutex<Vec<String>>>, Entity<()>) {
        let errors = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = errors.clone();
        let page = harness.page.clone();
        let listener = harness.visual.update(|_window, cx| {
            cx.new(|cx| {
                cx.subscribe(&page, move |_, _, event, _| {
                    let ProvidersPageEvent::Error(message) = event;
                    sink.lock().unwrap().push(message.to_string());
                })
                .detach();
            })
        });
        (errors, listener)
    }

    fn assert_preview_reply_fails_closed(cx: &mut gpui::TestAppContext, reply: serde_json::Value) {
        let _locale = crate::i18n::tests::locale_lock();
        let mut harness = providers_harness(cx);
        *harness.engine.preview_reply.lock().unwrap() = Some(reply);
        let (errors, _listener) = capture_errors(&mut harness);
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("fetch-vendor");

        let opened = harness
            .page
            .update(&mut *harness.visual, |page, _| page.fetch_dialog.is_some());
        let save_rendered = harness.visual.debug_bounds("save-fetch").is_some();
        if save_rendered {
            harness.click("save-fetch");
        }
        assert!(!opened, "an inconsistent preview opens no dialog");
        assert!(!save_rendered, "an inconsistent preview offers no Save");
        assert!(
            harness.engine.fetched_saves.lock().unwrap().is_empty(),
            "an inconsistent preview sends no SaveFetchedModels"
        );
        assert_eq!(
            *errors.lock().unwrap(),
            vec!["the model preview did not match the probe's model IDs; nothing was added"]
        );
    }

    fn text_of(harness: &mut ProvidersHarness, input: &Entity<ComposerInput>) -> String {
        harness.page.update(&mut *harness.visual, |_, cx| {
            input.read(cx).text().to_string()
        })
    }

    fn set_text(harness: &mut ProvidersHarness, input: &Entity<ComposerInput>, text: &str) {
        harness.page.update(&mut *harness.visual, |_, cx| {
            input.update(cx, |input, cx| input.set_text(text, cx))
        });
    }

    /// The dialog opens on a fresh probe: one row per listed id, in listing
    /// order. `acme-1` is already offered, so it stays in view, dimmed and
    /// unselected; `acme-9` is new, with the engine's facts in its row.
    #[gpui::test]
    fn fetch_opens_a_table_with_one_row_per_listed_id(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("fetch-vendor");

        let (ids, checked, added, fresh) = harness.page.update(&mut *harness.visual, |page, _| {
            let dialog = page.fetch_dialog.as_ref().expect("the fetch dialog opened");
            (
                dialog
                    .rows
                    .iter()
                    .map(|row| row.id.clone())
                    .collect::<Vec<_>>(),
                dialog.checked.clone(),
                dialog.rows.iter().map(|row| row.added).collect::<Vec<_>>(),
                dialog
                    .rows
                    .iter()
                    .map(|row| row.inputs.is_some())
                    .collect::<Vec<_>>(),
            )
        });
        assert_eq!(ids, vec!["acme-9".to_string(), "acme-1".to_string()]);
        assert_eq!(added, vec![false, true]);
        assert_eq!(fresh, vec![true, false]);
        assert_eq!(checked, HashSet::from(["acme-9".to_string()]));
        for selector in [
            "fetch-table",
            "fetch-check-0",
            "fetch-level-0-low",
            "fetch-image-0",
            "save-fetch",
        ] {
            assert!(
                harness.visual.debug_bounds(selector).is_some(),
                "{selector} renders"
            );
        }
        // Nothing to apply one set to, and nothing to select on an added row.
        assert!(harness.visual.debug_bounds("fetch-apply-all").is_none());
        assert!(harness.visual.debug_bounds("fetch-check-1").is_none());
    }

    /// A new row sends everything its row shows — name, numbers, levels,
    /// image input — under the provider's own dialect, which the engine
    /// resolves; nothing is typed by the dialog itself.
    #[gpui::test]
    fn a_fetched_new_row_is_saved_with_what_its_row_shows(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("fetch-vendor");

        let (name, context, max) = harness.page.update(&mut *harness.visual, |page, _| {
            let row = &page.fetch_dialog.as_ref().unwrap().rows[0];
            let inputs = row.inputs.as_ref().unwrap();
            (
                row.name.clone(),
                inputs.context_window.clone(),
                inputs.max_tokens.clone(),
            )
        });
        assert_eq!(
            text_of(&mut harness, &name),
            "Acme-9",
            "the name is derived from the id"
        );
        set_text(&mut harness, &name, "DeepSeek Flash 9");
        set_text(&mut harness, &context, "321000");
        set_text(&mut harness, &max, "16384");
        // No level comes with the plain defaults; two are switched on.
        harness.click("fetch-level-0-low");
        harness.click("fetch-level-0-max");
        harness.click("fetch-image-0");
        harness.click("save-fetch");
        harness.pump();

        let saves = harness.engine.fetched_saves.lock().unwrap().clone();
        assert_eq!(saves.len(), 1, "one SaveFetchedModels for the batch");
        assert_eq!(saves[0]["providerId"], "acme");
        assert_eq!(
            saves[0]["models"],
            serde_json::json!([{
                "id": "acme-9",
                "name": "DeepSeek Flash 9",
                "contextWindow": 321_000,
                "maxTokens": 16_384,
                "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0 },
                "thinkingLevels": ["low", "max"],
                "canTurnOff": true,
                "image": true,
            }])
        );
        assert!(
            harness.engine.records.lock().unwrap().is_empty(),
            "a fetched row never goes round SaveModelRecord"
        );
        assert!(
            harness
                .page
                .update(&mut *harness.visual, |page, _| page.fetch_dialog.is_none())
        );
    }

    /// An id the catalog carries elsewhere
    /// (ADR-fetched-models-adopt-the-catalogs-record): its row shows the
    /// catalog's numbers read-only — no chips to switch, no numbers to type
    /// — and Add sends its name alone.
    #[gpui::test]
    fn a_catalog_row_is_adopted_with_its_facts_and_needs_no_typing(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        harness
            .engine
            .preview_rows
            .lock()
            .unwrap()
            .push(preview_row("acme-9"));
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("fetch-vendor");

        let (name, source, inputs, levels) =
            harness.page.update(&mut *harness.visual, |page, _| {
                let row = &page.fetch_dialog.as_ref().unwrap().rows[0];
                (
                    row.name.clone(),
                    row.source.clone(),
                    row.inputs.is_some(),
                    row.levels.clone(),
                )
            });
        assert_eq!(source.as_deref(), Some("openai/gpt-5.4"));
        assert!(!inputs, "an adopted row has nothing to type");
        assert_eq!(text_of(&mut harness, &name), "GPT-5.4");
        assert_eq!(levels, ["low", "medium", "high", "xhigh"]);
        assert!(
            harness.visual.debug_bounds("fetch-level-0-low").is_none(),
            "the catalog's own levels are not switches"
        );
        assert!(harness.visual.debug_bounds("fetch-image-0").is_none());

        harness.click("save-fetch");
        harness.pump();

        let saves = harness.engine.fetched_saves.lock().unwrap().clone();
        assert_eq!(
            saves[0]["models"],
            serde_json::json!([{ "id": "acme-9", "name": "GPT-5.4" }])
        );
    }

    /// A ladder the catalog only estimated is shown as switches, and is
    /// sent only once it was changed: unedited, the row stays the
    /// catalog's to regenerate.
    #[gpui::test]
    fn an_estimated_ladder_is_switchable_and_sent_only_once_changed(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        let mut row = preview_row("acme-9");
        row["source"] = serde_json::json!("zai-coding-cn/glm-5.1");
        row["name"] = serde_json::json!("GLM-5.1");
        row["levelsEstimated"] = serde_json::json!(true);
        row["estimatedFrom"] = serde_json::json!("zai-coding-cn/glm-5.2");
        row["canTurnOff"] = serde_json::json!(false);
        row["reasoningLevels"] = serde_json::json!(["high", "max"]);
        harness.engine.preview_rows.lock().unwrap().push(row);
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("fetch-vendor");

        assert!(
            harness.visual.debug_bounds("fetch-level-0-low").is_some(),
            "an estimate is there to be corrected"
        );

        harness.click("fetch-level-0-low");
        harness.click("save-fetch");
        harness.pump();

        let saves = harness.engine.fetched_saves.lock().unwrap().clone();
        assert_eq!(
            saves[0]["models"],
            serde_json::json!([{
                "id": "acme-9",
                "name": "GLM-5.1",
                "thinkingLevels": ["low", "high", "max"],
                "canTurnOff": false,
            }]),
            "the corrected ladder is the user's, with the name; no numbers"
        );
    }

    #[gpui::test]
    fn an_unchanged_estimate_is_left_to_the_catalog(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        let mut row = preview_row("acme-9");
        row["levelsEstimated"] = serde_json::json!(true);
        harness.engine.preview_rows.lock().unwrap().push(row);
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("fetch-vendor");
        harness.click("save-fetch");
        harness.pump();

        let saves = harness.engine.fetched_saves.lock().unwrap().clone();
        assert_eq!(
            saves[0]["models"],
            serde_json::json!([{ "id": "acme-9", "name": "GPT-5.4" }])
        );
    }

    /// An id the provider already offers is checked but out of the batch:
    /// a toggle does nothing, and Add leaves it alone.
    #[gpui::test]
    fn an_id_the_provider_already_offers_is_never_added_again(cx: &mut gpui::TestAppContext) {
        assert_eq!(
            fetch_checkbox_state(true, false),
            widgets::CheckboxState::Checked
        );
        assert_eq!(
            fetch_checkbox_state(false, false),
            widgets::CheckboxState::Unchecked
        );
        let mut harness = providers_harness(cx);
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("fetch-vendor");

        harness.page.update(&mut *harness.visual, |page, cx| {
            page.toggle_fetch_row(1, cx);
            let dialog = page.fetch_dialog.as_ref().unwrap();
            assert!(!dialog.checked.contains("acme-1"));
        });
        harness.click("save-fetch");
        harness.pump();

        let saves = harness.engine.fetched_saves.lock().unwrap().clone();
        assert_eq!(saves[0]["models"].as_array().unwrap().len(), 1);
        assert_eq!(saves[0]["models"][0]["id"], "acme-9");
    }

    /// The panel's list hides rows the provider still offers — a hidden
    /// model, a dated snapshot — and `SaveFetchedModels` refuses them all
    /// the same. The engine's `offered` is that refusal: such an id is
    /// dimmed and out of the batch though the list never names it.
    #[gpui::test]
    fn an_id_only_the_engine_knows_is_offered_stays_out_of_the_batch(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut harness = providers_harness(cx);
        let mut hidden = preview_row("acme-hidden");
        hidden["offered"] = serde_json::json!(true);
        harness.engine.preview_rows.lock().unwrap().push(hidden);
        *harness.engine.probe_ids.lock().unwrap() = vec!["acme-hidden".into(), "acme-9".into()];
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("fetch-vendor");

        let (ids, added, checked) = harness.page.update(&mut *harness.visual, |page, _| {
            let dialog = page.fetch_dialog.as_ref().expect("the fetch dialog opened");
            (
                dialog
                    .rows
                    .iter()
                    .map(|row| row.id.clone())
                    .collect::<Vec<_>>(),
                dialog.rows.iter().map(|row| row.added).collect::<Vec<_>>(),
                dialog.checked.clone(),
            )
        });
        assert_eq!(ids, vec!["acme-hidden".to_string(), "acme-9".to_string()]);
        assert_eq!(added, vec![true, false]);
        assert_eq!(checked, HashSet::from(["acme-9".to_string()]));

        harness.click("save-fetch");
        harness.pump();

        let saves = harness.engine.fetched_saves.lock().unwrap().clone();
        assert_eq!(saves[0]["models"].as_array().unwrap().len(), 1);
        assert_eq!(saves[0]["models"][0]["id"], "acme-9");
    }

    /// A listing of nothing but such ids opens no dialog, as one of rows the
    /// panel does list.
    #[gpui::test]
    fn a_listing_wholly_offered_in_the_engine_s_eyes_opens_nothing(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        let mut hidden = preview_row("acme-hidden");
        hidden["offered"] = serde_json::json!(true);
        harness.engine.preview_rows.lock().unwrap().push(hidden);
        *harness.engine.probe_ids.lock().unwrap() = vec!["acme-hidden".into()];
        harness.click("provider-row-0");
        harness.click("fetch-vendor");
        harness.pump();

        assert!(
            harness
                .page
                .update(&mut *harness.visual, |page, _| page.fetch_dialog.is_none())
        );
    }

    /// A preview the engine cannot answer fails the fetch. A row with no
    /// facts used to open on the plain defaults, and Add filed those over
    /// the catalog's — for an id the catalog carries, at zero price and no
    /// longer following the catalog.
    #[gpui::test]
    fn a_failed_preview_opens_no_dialog_and_saves_nothing(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        *harness.engine.preview_error.lock().unwrap() =
            Some("provider acme has no transport to file models under".to_string());
        // The shell's error modal is the page's only report of a failure.
        let errors = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = errors.clone();
        let page = harness.page.clone();
        let _listener = harness.visual.update(|_window, cx| {
            cx.new(|cx| {
                cx.subscribe(&page, move |_, _, event, _| {
                    let ProvidersPageEvent::Error(message) = event;
                    sink.lock().unwrap().push(message.to_string());
                })
                .detach();
            })
        });
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("fetch-vendor");

        let open = harness
            .page
            .update(&mut *harness.visual, |page, _| page.fetch_dialog.is_some());
        assert!(
            !open,
            "the dialog does not open on rows the engine did not answer for"
        );
        assert_eq!(
            *errors.lock().unwrap(),
            vec!["provider acme has no transport to file models under".to_string()],
            "the failure reaches the error modal"
        );
        assert!(harness.engine.fetched_saves.lock().unwrap().is_empty());
    }

    #[gpui::test]
    fn an_empty_preview_reply_fails_closed(cx: &mut gpui::TestAppContext) {
        assert_preview_reply_fails_closed(cx, serde_json::json!([]));
    }

    #[gpui::test]
    fn a_partial_preview_reply_fails_closed(cx: &mut gpui::TestAppContext) {
        assert_preview_reply_fails_closed(
            cx,
            serde_json::Value::Array(vec![preview_row("acme-9")]),
        );
    }

    #[gpui::test]
    fn a_wrong_id_preview_reply_fails_closed(cx: &mut gpui::TestAppContext) {
        assert_preview_reply_fails_closed(
            cx,
            serde_json::Value::Array(vec![preview_row("wrong-id"), preview_row("acme-1")]),
        );
    }

    #[gpui::test]
    fn a_duplicate_preview_reply_fails_closed(cx: &mut gpui::TestAppContext) {
        assert_preview_reply_fails_closed(
            cx,
            serde_json::Value::Array(vec![preview_row("acme-9"), preview_row("acme-9")]),
        );
    }

    #[gpui::test]
    fn an_empty_probe_keeps_the_nothing_new_result(cx: &mut gpui::TestAppContext) {
        let _locale = crate::i18n::tests::locale_lock();
        let mut harness = providers_harness(cx);
        *harness.engine.probe_ids.lock().unwrap() = Vec::new();
        *harness.engine.preview_reply.lock().unwrap() = Some(serde_json::json!([]));
        let (errors, _listener) = capture_errors(&mut harness);
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("fetch-vendor");

        assert!(
            harness
                .page
                .update(&mut *harness.visual, |page, _| page.fetch_dialog.is_none())
        );
        assert!(harness.engine.fetched_saves.lock().unwrap().is_empty());
        assert_eq!(
            *errors.lock().unwrap(),
            vec!["the vendor's listing has nothing this provider doesn't already offer"]
        );
    }

    /// When the listing holds nothing the provider does not already offer,
    /// there is nothing to add and the dialog does not open.
    #[gpui::test]
    fn a_listing_wholly_offered_opens_nothing(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        *harness.engine.probe_ids.lock().unwrap() = vec!["acme-1".into()];
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("fetch-vendor");
        harness.pump();

        assert!(
            harness
                .page
                .update(&mut *harness.visual, |page, _| page.fetch_dialog.is_none())
        );
    }

    /// The reported bug: a dialog that opened before the panel's model list
    /// landed offered the provider's own models back, and Add filed blank
    /// records over the catalog's. Once the list lands, every id it carries
    /// is marked and dropped from the selection.
    #[gpui::test]
    fn the_model_list_landing_late_takes_its_ids_out_of_the_batch(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        *harness.engine.probe_ids.lock().unwrap() =
            vec!["acme-9".into(), "acme-custom".into(), "acme-1".into()];
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("fetch-vendor");

        // As the dialog would stand with the list still loading: both of
        // the provider's own ids selectable.
        harness.page.update(&mut *harness.visual, |page, cx| {
            let dialog = page.fetch_dialog.as_mut().unwrap();
            for row in &mut dialog.rows {
                row.added = false;
                dialog.checked.insert(row.id.clone());
            }
            page.fetch_models_loaded("acme", cx);
            let dialog = page.fetch_dialog.as_ref().unwrap();
            let added: Vec<bool> = dialog.rows.iter().map(|row| row.added).collect();
            assert_eq!(added, vec![false, true, true]);
            assert_eq!(dialog.checked, HashSet::from(["acme-9".to_string()]));
        });
    }

    #[gpui::test]
    fn a_bad_number_in_a_new_row_stops_the_batch_with_a_message(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("fetch-vendor");

        let (context, max) = harness.page.update(&mut *harness.visual, |page, _| {
            let inputs = page.fetch_dialog.as_ref().unwrap().rows[0]
                .inputs
                .as_ref()
                .unwrap();
            (inputs.context_window.clone(), inputs.max_tokens.clone())
        });
        for (input, text) in [(&context, "abc"), (&context, "0"), (&max, "-1")] {
            set_text(&mut harness, input, text);
            harness.click("save-fetch");
            harness.pump();
            let error = harness.page.update(&mut *harness.visual, |page, _| {
                page.fetch_dialog
                    .as_ref()
                    .and_then(|dialog| dialog.error.clone())
            });
            assert!(error.is_some(), "{text} is refused");
            assert!(
                harness.engine.fetched_saves.lock().unwrap().is_empty(),
                "nothing is sent while a row does not parse"
            );
        }
    }

    /// Several new rows start uniform and are corrected together: the shared
    /// set's numbers and levels land in each row at once.
    #[gpui::test]
    fn one_set_applies_to_every_new_row(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        *harness.engine.probe_ids.lock().unwrap() =
            vec!["acme-9".into(), "acme-7".into(), "acme-1".into()];
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("fetch-vendor");

        assert!(harness.visual.debug_bounds("fetch-apply-all").is_some());
        let bulk = harness.page.update(&mut *harness.visual, |page, _| {
            let bulk = page.fetch_dialog.as_ref().unwrap().bulk.as_ref().unwrap();
            (bulk.context_window.clone(), bulk.max_tokens.clone())
        });
        set_text(&mut harness, &bulk.0, "321000");
        harness.click("fetch-bulk-level-low");
        harness.click("fetch-apply-all");
        harness.click("save-fetch");
        harness.pump();

        let saves = harness.engine.fetched_saves.lock().unwrap().clone();
        let models = saves[0]["models"].as_array().unwrap();
        assert_eq!(models.len(), 2, "the offered id stays out");
        for model in models {
            assert_eq!(model["contextWindow"], 321_000);
            assert_eq!(model["maxTokens"], 8_192);
            assert_eq!(model["thinkingLevels"], serde_json::json!(["low"]));
        }
    }

    #[gpui::test]
    fn an_emptied_name_is_left_to_the_engine(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("fetch-vendor");

        let name = harness.page.update(&mut *harness.visual, |page, _| {
            page.fetch_dialog.as_ref().unwrap().rows[0].name.clone()
        });
        set_text(&mut harness, &name, "");
        harness.click("save-fetch");
        harness.pump();

        let saves = harness.engine.fetched_saves.lock().unwrap().clone();
        assert_eq!(saves[0]["models"][0]["name"], "");
    }

    /// Drive the tokio runtime while the window settles, until `ready`
    /// holds or the budget runs out: a real loopback round trip (the
    /// probe's HTTP call) does not finish on a fixed count of turns. An
    /// expired budget is never a hang — the assertion waiting on the
    /// state is what reports it.
    fn settle(
        runtime: &tokio::runtime::Runtime,
        visual: &mut gpui::VisualTestContext,
        mut ready: impl FnMut(&mut gpui::VisualTestContext) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            runtime.block_on(async {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            });
            visual.run_until_parked();
            if ready(visual) || std::time::Instant::now() >= deadline {
                return;
            }
        }
    }

    fn click(
        runtime: &tokio::runtime::Runtime,
        visual: &mut gpui::VisualTestContext,
        selector: &'static str,
    ) {
        let bounds = visual
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} renders"));
        visual.simulate_click(bounds.center(), Default::default());
        settle(runtime, visual, |_| true);
    }

    /// A gateway and its listing behind a real engine. The listing's
    /// loopback server comes back too: it lives as long as the test, or
    /// the probe finds nothing.
    fn gateway_over(
        runtime: &tokio::runtime::Runtime,
        listing: &'static [u8],
    ) -> (
        crate::engine_fixture::Fixture,
        Arc<holt_engine::LocalEngine>,
        crate::engine_fixture::LoopbackServer,
    ) {
        use crate::engine_fixture::{self, Fixture, ScriptedProvider};
        use holt_rpc::RpcService;
        let fixture = Fixture::new();
        let (vendor, _heads) = runtime.block_on(engine_fixture::serve_loopback_with_capture(
            "application/json",
            listing,
        ));
        let engine = Arc::new(fixture.engine(&ScriptedProvider::new(vec![])));
        runtime.block_on(async {
            engine
                .handle(
                    methods::SAVE_CUSTOM_PROVIDER,
                    serde_json::json!({
                        "id": "gw",
                        "name": "Company Gateway",
                        "baseUrl": vendor.base,
                        "defaultApi": "openai-completions",
                    }),
                )
                .await
                .unwrap();
        });
        (fixture, engine, vendor)
    }

    /// The page over that engine, the gateway's panel expanded, with
    /// `body` driving the window.
    fn over_gateway(
        runtime: &tokio::runtime::Runtime,
        cx: &mut gpui::TestAppContext,
        engine: Arc<holt_engine::LocalEngine>,
        body: impl FnOnce(&mut gpui::VisualTestContext, &Entity<ProvidersPage>),
    ) {
        let client = {
            let _enter = runtime.enter();
            holt_rpc::memory_client(engine)
        };
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::default());
            crate::settings::init(crate::settings::UiSettings::default(), dir.path(), cx);
        });
        let state = cx.new(|_| AppState::new());
        state.update(cx, |state, cx| state.attach_test_engine(client, cx));
        let (page, visual) =
            cx.add_window_view(|_window, cx| ProvidersPage::new(state.clone(), cx));
        // A window tall enough to hold the whole provider list, with the
        // gateway at its end: a click needs its target's hitbox inside
        // the viewport.
        visual.simulate_resize(gpui::size(px(1280.0), px(4000.0)));
        visual.update(|_window, cx| cx.set_reduce_motion(true));
        page.update(visual, |page, cx| page.focus_provider("gw".into(), cx));
        settle(runtime, visual, |visual| {
            page.update(&mut *visual, |page, _| {
                page.expanded.as_deref() == Some("gw")
            })
        });
        body(visual, &page);
    }

    async fn listed_over(
        engine: &holt_engine::LocalEngine,
        provider: &str,
    ) -> Vec<serde_json::Value> {
        use holt_rpc::RpcService;
        match engine
            .handle(
                methods::LIST_MODELS,
                serde_json::json!({ "providerId": provider }),
            )
            .await
            .unwrap()
        {
            holt_rpc::RpcReply::Value(rows) => rows.as_array().unwrap().clone(),
            _ => panic!("ListModels did not return a value"),
        }
    }

    /// Open the dialog on the gateway's listing and bring back what each
    /// row started as.
    fn open_over(
        runtime: &tokio::runtime::Runtime,
        visual: &mut gpui::VisualTestContext,
        page: &Entity<ProvidersPage>,
    ) -> (Vec<String>, Vec<bool>, String) {
        click(runtime, visual, "fetch-vendor");
        settle(runtime, visual, |visual| {
            page.update(&mut *visual, |page, _| page.fetch_dialog.is_some())
        });
        page.update(&mut *visual, |page, cx| {
            let dialog = page.fetch_dialog.as_ref().expect("the fetch dialog opened");
            (
                dialog.rows.iter().map(|row| row.id.clone()).collect(),
                dialog.rows.iter().map(|row| row.source.is_some()).collect(),
                dialog.rows[dialog.rows.len() - 1]
                    .name
                    .read(cx)
                    .text()
                    .to_string(),
            )
        })
    }

    /// The reported scenario end to end on a real engine: the gateway's
    /// listing names `gpt-5.4`, whose row comes with the catalog's numbers
    /// and needs nothing typed, and `acme-1`, which the catalog has never
    /// heard of. Add files both in one commit — the first with the 1,050K
    /// window and the ladder without `minimal`, where the old dialog's
    /// shared 200,000 / generic four used to land.
    #[gpui::test]
    fn a_fetched_listing_lists_with_each_model_s_own_facts_through_the_real_engine(
        cx: &mut gpui::TestAppContext,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_fixture, engine, _vendor) =
            gateway_over(&runtime, br#"{"data":[{"id":"gpt-5.4"},{"id":"acme-1"}]}"#);
        over_gateway(&runtime, cx, engine.clone(), |visual, page| {
            let (ids, adopted, last_name) = open_over(&runtime, visual, page);
            assert_eq!(ids, vec!["gpt-5.4".to_string(), "acme-1".to_string()]);
            assert_eq!(adopted, vec![true, false]);
            // The new row's name is written the model list's way, not left
            // as the lowercase id.
            assert_eq!(last_name, "Acme-1");
            assert!(
                visual.debug_bounds("fetch-level-1-low").is_some(),
                "the new row's levels are switches"
            );
            assert!(
                visual.debug_bounds("fetch-level-0-low").is_none(),
                "the catalog's levels are read-only"
            );
            click(&runtime, visual, "save-fetch");
            settle(&runtime, visual, |visual| {
                page.update(&mut *visual, |page, _| page.fetch_dialog.is_none())
            });
        });

        let rows = runtime.block_on(listed_over(&engine, "gw"));
        assert_eq!(rows.len(), 2, "both fetched ids list");
        let gpt = rows.iter().find(|row| row["id"] == "gw/gpt-5.4").unwrap();
        assert_eq!(gpt["label"], "GPT-5.4");
        assert_eq!(gpt["contextWindow"], 1_050_000);
        assert_eq!(
            gpt["reasoningLevels"],
            serde_json::json!(["low", "medium", "high", "xhigh"])
        );
        let acme = rows.iter().find(|row| row["id"] == "gw/acme-1").unwrap();
        assert_eq!(acme["label"], "Acme-1");
        assert_eq!(acme["contextWindow"], 200_000);
    }

    /// A model nobody knows yet, on a real engine: its levels are whatever
    /// the switches say, and the chat's picker offers exactly those.
    #[gpui::test]
    fn a_new_model_s_levels_are_the_switches_through_the_real_engine(
        cx: &mut gpui::TestAppContext,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_fixture, engine, _vendor) = gateway_over(&runtime, br#"{"data":[{"id":"glm-9"}]}"#);
        over_gateway(&runtime, cx, engine.clone(), |visual, page| {
            let (_, adopted, _) = open_over(&runtime, visual, page);
            assert_eq!(adopted, vec![false]);
            // A blank gateway reasons nothing by default: no level is on.
            click(&runtime, visual, "fetch-level-0-low");
            click(&runtime, visual, "fetch-level-0-max");
            click(&runtime, visual, "save-fetch");
            settle(&runtime, visual, |visual| {
                page.update(&mut *visual, |page, _| page.fetch_dialog.is_none())
            });
        });

        let rows = runtime.block_on(listed_over(&engine, "gw"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["label"], "GLM-9");
        assert_eq!(
            rows[0]["reasoningLevels"],
            serde_json::json!(["low", "max"])
        );
    }

    /// The key row's Test button: one ProbeProvider call, verdict rendered.
    #[gpui::test]
    fn the_test_button_reports_the_probe_verdict(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("test-provider");
        harness.pump();

        assert_eq!(harness.engine.probes.lock().unwrap().len(), 1);
        let done = harness.page.update(&mut *harness.visual, |page, _| {
            matches!(
                page.probes.get("acme"),
                Some(ProbeState::Done(outcome)) if outcome.ok
            )
        });
        assert!(done, "the probe reply landed in the page state");
        assert!(
            harness.visual.debug_bounds("probe-status").is_some(),
            "the verdict line renders"
        );
    }

    /// The form opened on an existing row carries everything the row does:
    /// its numbers, its flags, and — in the advanced JSON — the ladder the
    /// form has no field for, so a corrected price saved under the same id
    /// does not drop it. The cost's tiers ride there too: twenty-two catalog
    /// rows price a long context by them, and a save that rebuilt the cost
    /// from the four rate fields filed them away tier-less.
    #[gpui::test]
    fn the_record_form_opens_on_a_row_with_everything_it_carries(cx: &mut gpui::TestAppContext) {
        let harness = providers_harness(cx);
        let tiers = serde_json::json!([{
            "input": 10, "output": 30, "cacheRead": 1, "cacheWrite": 0,
            "inputTokensAbove": 272000
        }]);
        harness
            .engine
            .records_served
            .lock()
            .unwrap()
            .push(serde_json::json!({
                "id": "acme-custom",
                "name": "Acme custom",
                "api": "openai-completions",
                "provider": "acme",
                "baseUrl": "https://acme.example/v1",
                "reasoning": true,
                "thinkingLevelMap": {
                    "minimal": null, "low": "low", "medium": "medium",
                    "high": "high", "xhigh": "xhigh", "max": null
                },
                "input": ["text", "image"],
                "cost": {
                    "input": 2.5, "output": 15, "cacheRead": 0.25, "cacheWrite": 0,
                    "tiers": tiers
                },
                "contextWindow": 1050000,
                "maxTokens": 128000,
            }));
        harness.page.update(&mut *harness.visual, |page, cx| {
            page.edit_model("acme".into(), "acme/acme-custom".into(), cx)
        });
        harness.pump();

        let texts = harness.page.update(&mut *harness.visual, |page, cx| {
            let form = page
                .record_form
                .as_ref()
                .expect("the form opened on the row");
            assert!(form.editing);
            assert_eq!(form.api, "openai-completions");
            assert!(form.reasoning && form.image);
            form.inputs
                .iter()
                .map(|(key, input)| (key.clone(), input.read(cx).text().to_string()))
                .collect::<HashMap<_, _>>()
        });
        for (key, expected) in [
            ("id", "acme-custom"),
            ("name", "Acme custom"),
            ("contextWindow", "1050000"),
            ("maxTokens", "128000"),
            ("inputCost", "2.5"),
            ("outputCost", "15"),
            ("cacheReadCost", "0.25"),
            ("cacheWriteCost", "0"),
        ] {
            assert_eq!(texts[key], expected, "{key} prefills from the row");
        }
        let advanced: serde_json::Value = serde_json::from_str(&texts["advanced"]).unwrap();
        assert_eq!(advanced["thinkingLevelMap"]["xhigh"], "xhigh");
        assert_eq!(
            advanced["thinkingLevelMap"]["minimal"],
            serde_json::Value::Null
        );
        assert_eq!(advanced["baseUrl"], "https://acme.example/v1");
        assert_eq!(
            advanced["cost"],
            serde_json::json!({ "tiers": tiers }),
            "the rates stay out of the advanced JSON; the tiers ride in it"
        );

        harness.page.update(&mut *harness.visual, |page, cx| {
            page.record_form.as_ref().unwrap().inputs["inputCost"]
                .update(cx, |input, cx| input.set_text("1.25", cx));
            page.save_record(cx);
        });
        harness.pump();

        let records = harness.engine.records.lock().unwrap().clone();
        assert_eq!(records.len(), 1);
        let record = &records[0]["record"];
        assert_eq!(record["cost"]["input"].as_f64(), Some(1.25));
        assert_eq!(record["cost"]["output"].as_f64(), Some(15.0));
        assert_eq!(
            record["cost"]["tiers"], tiers,
            "the tiers go back under the rates"
        );
        assert_eq!(record["thinkingLevelMap"]["xhigh"], "xhigh");
        assert_eq!(record["input"], serde_json::json!(["text", "image"]));
        assert_eq!(record["contextWindow"], 1_050_000);
    }

    /// The form a row never filled: opened for a new record, it is empty.
    #[gpui::test]
    fn the_record_form_opens_empty_for_a_new_record(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("toggle-record-form");

        harness.page.update(&mut *harness.visual, |page, cx| {
            let form = page.record_form.as_ref().expect("the form opened");
            assert!(!form.editing);
            assert_eq!(form.inputs["id"].read(cx).text(), "");
            assert_eq!(form.inputs["advanced"].read(cx).text(), "");
        });
    }

    /// The pen's record takes a round trip; a form opened since it was
    /// asked for — the Add button's, with a draft in it — stays.
    #[gpui::test]
    fn a_record_reply_that_lands_after_a_newer_form_replaces_nothing(
        cx: &mut gpui::TestAppContext,
    ) {
        let harness = providers_harness(cx);
        harness
            .engine
            .records_served
            .lock()
            .unwrap()
            .push(serde_json::json!({
                "id": "acme-custom",
                "name": "Acme custom",
                "api": "openai-completions",
                "provider": "acme",
                "reasoning": false,
                "input": ["text"],
                "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
                "contextWindow": 321000,
                "maxTokens": 16384,
            }));
        harness.page.update(&mut *harness.visual, |page, cx| {
            // The pen: its request leaves on the next turn of the runtime...
            page.edit_model("acme".into(), "acme/acme-custom".into(), cx);
            // ...and before that turn, the Add button opens a blank form.
            page.open_record_form("acme".into(), None, cx);
        });
        harness.pump();

        harness.page.update(&mut *harness.visual, |page, cx| {
            let form = page.record_form.as_ref().expect("the Add form stays");
            assert!(
                !form.editing,
                "the reply did not replace it with the row's record"
            );
            assert_eq!(form.inputs["id"].read(cx).text(), "");
        });
    }
}
