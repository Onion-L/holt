//! The project (space) picker on the new-session canvas: scoped and filtered
//! project rows, selection persistence (the "last selected" default), and
//! the popover rendering.

use gpui::{AnyElement, App, Context, SharedString, div, prelude::*, px};

use holt_proto::Space;

use crate::popover;
use crate::theme::Theme;

use super::NO_ACTIVE_ROW;
use super::Pickers;

impl Pickers {
    // ---- the space picker (new-session canvas) ----

    /// The picker's project rows: every synced project, sorted — projects
    /// aren't scoped to anything narrower.
    fn scoped_space_rows(&self, cx: &App) -> Vec<Space> {
        let state = self.state.read(cx);
        state.spaces_sorted().into_iter().cloned().collect()
    }

    /// [`Self::scoped_space_rows`] matching the search query, ranked
    /// (`popover::filter_indices`).
    pub(super) fn filtered_space_rows(&self, cx: &App) -> Vec<Space> {
        let query = self.search.read(cx).text().to_string();
        let spaces = self.scoped_space_rows(cx);
        let names: Vec<String> = spaces
            .iter()
            .map(|s| s.display_name().to_string())
            .collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| spaces[ix].clone())
            .collect()
    }

    /// Row index of the currently selected space (un-searched open) — within
    /// the scoped order [`filtered_space_rows`] lists on an empty query.
    /// [`NO_ACTIVE_ROW`] when nothing is selected (the no-project canvas must
    /// not open with row 0 wearing a phantom highlight — user report).
    pub(super) fn selected_space_index(&self, cx: &App) -> usize {
        let selected = self
            .state
            .read(cx)
            .selected_space_row()
            .map(|s| s.id.clone());
        selected
            .as_deref()
            .and_then(|id| self.scoped_space_rows(cx).iter().position(|s| s.id == id))
            .unwrap_or(NO_ACTIVE_ROW)
    }

    /// Re-home the canvas onto another project. The state observer does the
    /// heavy lifting: the branch draft and ref cache invalidate on the
    /// project change.
    pub(super) fn pick_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.state
            .update(cx, |s, cx| s.select_space(Some(space_id), cx));
        self.remember_target(cx);
        self.close(cx);
    }

    /// Persist the project pick — the "last selected" default the next
    /// boot's canvas restores.
    fn remember_target(&mut self, cx: &App) {
        {
            let state = self.state.read(cx);
            self.defaults.project = state.selected_space.clone();
            self.defaults.no_project = state.no_project;
        }
        if let Some(dir) = &self.data_dir {
            if let Err(err) = self.defaults.save(dir) {
                tracing::warn!(error = %err, "composer-defaults save failed");
            }
        }
    }

    /// The project popover: search + one row per project (check on the
    /// current pick), then a "New project…" action row.
    pub(super) fn render_space_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let rows = self.filtered_space_rows(cx);
        let selected = self
            .state
            .read(cx)
            .selected_space_row()
            .map(|s| s.id.clone());
        let active = self.active;
        let body: AnyElement = if rows.is_empty() {
            // Distinguish "the filter ate everything" from "no projects yet".
            let empty: &str = if self.search.read(cx).text().is_empty() {
                "No projects."
            } else {
                "No projects match."
            };
            div()
                .p(px(Theme::SPACE_SM))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from(empty.to_string()))
                .into_any_element()
        } else {
            div()
                .id("space-list")
                .flex()
                .flex_col()
                .gap(px(2.0))
                .max_h(px(224.0))
                .overflow_y_scroll()
                .children(rows.into_iter().enumerate().map(|(ix, space)| {
                    let label: SharedString = space.display_name().to_string().into();
                    let is_selected = selected.as_deref() == Some(space.id.as_str());
                    let pick_id = space.id.clone();
                    popover::menu_row_nav(
                        &theme,
                        is_selected,
                        ix == active,
                        format!("space-row-{ix}"),
                    )
                    .id(("space-row", ix))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pick_space(pick_id.clone(), cx);
                    }))
                    .child(div().flex_1().min_w_0().truncate().child(label))
                }))
                .into_any_element()
        };
        // Action row under a hairline: mint a project.
        let new_project = popover::menu_row_nav(&theme, false, false, "project-new".to_string())
            .id("project-new")
            .on_click(cx.listener(|this, _, window, cx| {
                this.close(cx);
                window.dispatch_action(Box::new(crate::shell::AddSpacePalette), cx);
            }))
            .child(
                crate::icons::icon(crate::icons::PLUS)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from("New project…")),
            );
        div()
            .flex()
            .flex_col()
            // Same 2px rhythm as the list's own row gap — the action rows
            // sat flush while list rows breathed (user report).
            .gap(px(2.0))
            .child(self.search_box(&theme))
            .child(body)
            .child(
                // Full-bleed through the card's 4px inset — a divider
                // stopping short of the edges read as a mistake.
                div()
                    .my(px(2.0))
                    .mx(px(-4.0))
                    .h(px(1.0))
                    .flex_none()
                    .bg(theme.border.opacity(0.6)),
            )
            .child(new_project)
            .into_any_element()
    }
}
