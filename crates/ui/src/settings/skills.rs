//! The Skills settings page: every catalog entry across the three skill
//! roots with its status — the diagnostics surface the `/` menu
//! deliberately is not (ADR-0005). Read-only: a skill becomes available by
//! being placed in a root, and that is the only way in.

use gpui::{Context, Entity, IntoElement, Render, SharedString, Task, Window, div, prelude::*, px};
use holt_proto::{InvalidSkillEntry, ShadowedSkillEntry, SkillEntry, SkillListing, SkillRoot};
use holt_rpc::methods;

use crate::{
    popover::{self, Loadable},
    settings::widgets,
    state::AppState,
    theme::Theme,
};

/// One rendered catalog row, carrying everything the page explains: the
/// invocable winners, the shadowed losers, and the loader's diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SkillRow {
    Ok(SkillEntry),
    Shadowed(ShadowedSkillEntry),
    Invalid(InvalidSkillEntry),
}

/// Assemble the page's rows from a listing: invocable skills first, then
/// shadowed entries, then invalid ones — each group in catalog order.
fn skill_rows(listing: &SkillListing) -> Vec<SkillRow> {
    listing
        .skills
        .iter()
        .cloned()
        .map(SkillRow::Ok)
        .chain(listing.shadowed.iter().cloned().map(SkillRow::Shadowed))
        .chain(listing.invalid.iter().cloned().map(SkillRow::Invalid))
        .collect()
}

impl SkillRow {
    fn name(&self) -> String {
        match self {
            SkillRow::Ok(skill) => skill.name.clone(),
            SkillRow::Shadowed(entry) => entry.name.clone(),
            // A parse failure may have no name; the file stands in.
            SkillRow::Invalid(entry) => entry
                .name
                .clone()
                .unwrap_or_else(|| file_basename(&entry.file)),
        }
    }

    fn root(&self) -> SkillRoot {
        match self {
            SkillRow::Ok(skill) => skill.root,
            SkillRow::Shadowed(entry) => entry.root,
            SkillRow::Invalid(entry) => entry.root,
        }
    }
}

fn file_basename(path: &str) -> String {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
        .to_string()
}

fn root_label(root: SkillRoot) -> &'static str {
    match root {
        SkillRoot::Project => "project",
        SkillRoot::Personal => "personal",
        SkillRoot::Holt => "holt",
    }
}

pub struct SkillsPage {
    state: Entity<AppState>,
    listing: Loadable<SkillListing>,
    task: Option<Task<()>>,
}

impl SkillsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            listing: Loadable::Idle,
            task: None,
        };
        page.load(cx);
        page
    }

    /// Fresh catalog scan from the engine — the filesystem is the registry,
    /// so every open reflects drops/edits since the last visit without a
    /// restart. The project root derives from the selected chat's cwd (or
    /// the picked space's folder).
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.listing = Loadable::Error("Engine not connected".into());
            return;
        };
        let cwd = {
            let state = self.state.read(cx);
            state
                .selected_chat_row()
                .and_then(|chat| chat.cwd.clone())
                .or_else(|| state.selected_space_row().map(|space| space.path.clone()))
        };
        self.listing = Loadable::Loading;
        self.task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            if let Some(cwd) = &cwd {
                params.insert("cwd".into(), cwd.clone().into());
            }
            let result = engine
                .client()
                .call(methods::LIST_SKILLS, serde_json::Value::Object(params))
                .await;
            this.update(cx, |page, cx| {
                page.listing = match result {
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
}

impl Render for SkillsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let body = match &self.listing {
            Loadable::Idle | Loadable::Loading => {
                popover::skeleton_rows("skills-skeleton", &theme, 4, cx.entity_id(), cx)
                    .into_any_element()
            }
            Loadable::Error(error) => {
                widgets::error_strip(&theme, error.clone()).into_any_element()
            }
            Loadable::Ready(listing) => {
                let rows = skill_rows(listing);
                if rows.is_empty() {
                    // The fresh-machine case: an empty catalog is normal, not
                    // an error (ADR-0005).
                    div()
                        .mt(px(24.0))
                        .text_size(crate::typography::ui_rems(12.5))
                        .text_color(theme.text_muted)
                        .child(
                            "No skills found. Place a skill directory under \
                                .agents/skills in your project, ~/.agents/skills, \
                                or ~/.holt/skills and it appears here.",
                        )
                        .into_any_element()
                } else {
                    let card = rows.iter().enumerate().fold(
                        widgets::section_card(&theme),
                        |card, (index, row)| {
                            let name = row.name();
                            let status: gpui::SharedString = match row {
                                SkillRow::Ok(_) => "ok".into(),
                                SkillRow::Shadowed(entry) => {
                                    format!("shadowed by {}", root_label(entry.shadowed_by)).into()
                                }
                                SkillRow::Invalid(_) => "invalid".into(),
                            };
                            let secondary: gpui::SharedString = match row {
                                SkillRow::Ok(skill) => {
                                    if skill.disable_model_invocation {
                                        SharedString::from(
                                            "Manual only (/skill) · not advertised to the model",
                                        )
                                    } else {
                                        SharedString::from(skill.description.clone())
                                    }
                                }
                                SkillRow::Shadowed(entry) => SharedString::from(entry.file.clone()),
                                SkillRow::Invalid(entry) => {
                                    SharedString::from(entry.message.clone())
                                }
                            };
                            let row_theme = theme.clone();
                            let status_color = match row {
                                SkillRow::Ok(_) => row_theme.text_muted.opacity(0.7),
                                SkillRow::Shadowed(_) => row_theme.warning_muted.opacity(0.9),
                                SkillRow::Invalid(_) => row_theme.danger_muted.opacity(0.9),
                            };
                            let secondary_color = match row {
                                SkillRow::Invalid(_) => row_theme.danger_muted.opacity(0.9),
                                _ => row_theme.text_muted,
                            };
                            card.child(
                                widgets::card_row(&row_theme, index == 0)
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .gap(px(3.0))
                                            .child(widgets::row_title(&row_theme, name))
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_size(crate::typography::ui_rems(12.0))
                                                    .line_height(px(18.0))
                                                    .text_color(secondary_color)
                                                    .child(secondary),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .flex()
                                            .flex_col()
                                            .items_end()
                                            .gap(px(6.0))
                                            .child(widgets::badge(
                                                &row_theme,
                                                root_label(row.root()),
                                            ))
                                            .child(
                                                div()
                                                    .text_size(crate::typography::ui_rems(10.5))
                                                    .text_color(status_color)
                                                    .child(status),
                                            ),
                                    ),
                            )
                        },
                    );
                    card.mt(px(24.0)).into_any_element()
                }
            }
        };
        div()
            .id("skills-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(
                        &theme,
                        "Skills",
                        self.listing.ready().map(|listing| listing.skills.len()),
                    ))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Every skill discovered across your project, personal, and Holt \
                         roots — referenced in place, never copied. Nearest root wins on \
                         name collisions; broken entries explain themselves here.",
                    ))
                    .child(body),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, root: SkillRoot) -> SkillEntry {
        SkillEntry {
            name: name.into(),
            description: "Does things.".into(),
            file: format!("/roots/{name}/SKILL.md"),
            root,
            disable_model_invocation: false,
        }
    }

    #[test]
    fn rows_group_ok_then_shadowed_then_invalid() {
        let listing = SkillListing {
            skills: vec![entry("grill", SkillRoot::Project)],
            shadowed: vec![ShadowedSkillEntry {
                name: "grill".into(),
                file: "/holt/grill/SKILL.md".into(),
                root: SkillRoot::Holt,
                shadowed_by: SkillRoot::Project,
            }],
            invalid: vec![InvalidSkillEntry {
                file: "/repo/.agents/skills/draft/SKILL.md".into(),
                root: SkillRoot::Project,
                name: None,
                message: "description is required".into(),
            }],
        };
        let rows = skill_rows(&listing);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name(), "grill");
        assert_eq!(rows[1].name(), "grill");
        assert_eq!(rows[1].root(), SkillRoot::Holt);
        // A nameless invalid entry falls back to its file's basename.
        assert_eq!(rows[2].name(), "SKILL.md");
        assert_eq!(root_label(rows[0].root()), "project");
    }
}
