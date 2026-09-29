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

    /// The picker's project rows: every synced project, sorted — Home
    /// excluded (ADR-0039): its seat is the "Work outside a project" row,
    /// never a listing among real folders.
    fn scoped_space_rows(&self, cx: &App) -> Vec<Space> {
        let state = self.state.read(cx);
        state
            .spaces_sorted()
            .into_iter()
            .filter(|space| space.id != holt_proto::HOME_SPACE_ID)
            .cloned()
            .collect()
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
    /// [`NO_ACTIVE_ROW`] when nothing is selected — the Home selection too:
    /// the canvas must not open with row 0 wearing a phantom highlight
    /// (user report).
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
        self.state.update(cx, |s, cx| s.select_space(space_id, cx));
        self.remember_target(cx);
        self.close(cx);
    }

    /// Re-home the canvas onto the Home space — the "Work outside a
    /// project" pick (ADR-0039). Persisted like any other pick.
    pub(super) fn pick_home(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| {
            s.select_space(holt_proto::HOME_SPACE_ID.to_string(), cx)
        });
        self.remember_target(cx);
        self.close(cx);
    }

    /// Persist the project pick — the "last selected" default the next
    /// boot's canvas restores. `project` may be the Home space id: the
    /// remembered "Work outside a project" state.
    fn remember_target(&mut self, cx: &App) {
        self.defaults.project = self.state.read(cx).selected_space.clone();
        if let Some(dir) = &self.data_dir
            && let Err(err) = self.defaults.save(dir)
        {
            tracing::warn!(error = %err, "composer-defaults save failed");
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
        // Action rows under a hairline: the Home target (ADR-0039) and
        // minting a project.
        let home_selected = self
            .state
            .read(cx)
            .selected_space_row()
            .is_some_and(|space| space.id == holt_proto::HOME_SPACE_ID);
        let work_outside =
            popover::menu_row_nav(&theme, home_selected, false, "project-home".to_string())
                .id("project-home")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.pick_home(cx);
                }))
                .child(
                    crate::icons::icon(crate::icons::HOME)
                        .size(px(12.0))
                        .flex_none()
                        .text_color(theme.text_muted.opacity(0.7)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from("Work outside a project")),
                );
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
            .child(work_outside)
            .child(new_project)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::super::Pickers;
    use crate::state::AppState;
    use gpui::AppContext as _;
    use holt_proto::{HOME_SPACE_ID, Space};

    fn space(id: &str, name: &str) -> Space {
        Space {
            id: id.into(),
            device_id: "d".into(),
            path: format!("/tmp/{id}"),
            name: Some(name.into()),
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: chrono::Utc::now(),
        }
    }

    /// Home never lists among the project rows (ADR-0039): its seat is the
    /// "Work outside a project" action, which selects it and persists the
    /// pick like any project.
    #[gpui::test]
    fn home_is_an_action_not_a_row(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        {
            let state = cx.new(|_| AppState::new());
            state.update(cx, |state, _| {
                state.spaces = vec![space(HOME_SPACE_ID, "Home"), space("p1", "api")];
                state.selected_space = Some("p1".into());
            });
            let pickers = cx.new(|cx| Pickers::new(state.clone(), cx));

            pickers.update(cx, |this, cx| {
                let rows = this.scoped_space_rows(cx);
                let ids: Vec<&str> = rows.iter().map(|s| s.id.as_str()).collect();
                assert_eq!(ids, ["p1"]);
                // No phantom highlight while Home is selected.
                this.pick_home(cx);
            });
            state.update(cx, |state, _| {
                assert_eq!(state.selected_space.as_deref(), Some(HOME_SPACE_ID));
            });
            pickers.update(cx, |this, cx| {
                assert_eq!(this.selected_space_index(cx), super::super::NO_ACTIVE_ROW);
                assert!(this.defaults.project.as_deref() == Some(HOME_SPACE_ID));
            });
        }
    }
}
