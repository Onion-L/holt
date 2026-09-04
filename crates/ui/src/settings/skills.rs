//! The Skills settings page: every catalog entry across the three skill
//! roots with its status — the diagnostics surface the `/` menu
//! deliberately is not (ADR-0005). Placement is still the only way IN (a
//! skill becomes available by being placed in a root), but each invocable
//! row carries an enable switch: a disabled skill hides from the `/` menu
//! and is refused on a typed `/skill` invocation. The disabled set persists
//! by catalog-unique name in `ui-settings.json` (`disabledSkills`).

use gpui::{Context, Entity, IntoElement, Render, SharedString, Task, Window, div, prelude::*, px};
use holt_proto::{InvalidSkillEntry, ShadowedSkillEntry, SkillEntry, SkillListing, SkillRoot};
use holt_rpc::methods;

use crate::{
    popover::{self, Loadable},
    settings::{self, SavePolicy, widgets},
    state::AppState,
    theme::{Theme, ink},
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

    /// The source file — unique per row, so it doubles as the element id key
    /// (stable ids are what make gpui's hover repaint on the transition).
    fn file(&self) -> &str {
        match self {
            SkillRow::Ok(skill) => &skill.file,
            SkillRow::Shadowed(entry) => &entry.file,
            SkillRow::Invalid(entry) => &entry.file,
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
    /// The last enable-switch flip and when it happened. The toggle renders
    /// its animated variant only inside a short window after the click —
    /// the keyed element's first mount IS the transition — and the static
    /// switch (same end state) outside it, so opening the page or reloading
    /// the listing never replays every knob slide at once.
    toggle_flip: Option<(String, std::time::Instant)>,
}

impl SkillsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            listing: Loadable::Idle,
            task: None,
            toggle_flip: None,
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
        let cwd = self.state.read(cx).skills_cwd();
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
                    // UnknownMethod is version skew, same as the composer's
                    // popup: name it rather than echoing the raw error.
                    Err(holt_rpc::RpcError::UnknownMethod(_)) => Loadable::Error(
                        "Skills aren't available — the engine doesn't support them yet".into(),
                    ),
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
                    let disabled = settings::current(cx).disabled_skills;
                    let list = rows.iter().fold(
                        div().mt(px(16.0)).flex().flex_col().gap(px(2.0)),
                        |list, row| {
                            let name = row.name();
                            // Only invocable rows get the enable switch;
                            // shadowed/invalid entries never enter the menu.
                            let enabled = match row {
                                SkillRow::Ok(_) => !disabled.iter().any(|n| n == &name),
                                _ => true,
                            };
                            let status: Option<(gpui::SharedString, gpui::Hsla)> = match row {
                                SkillRow::Ok(skill) if skill.disable_model_invocation => {
                                    Some(("manual only".into(), theme.text_muted.opacity(0.7)))
                                }
                                SkillRow::Ok(_) => None,
                                SkillRow::Shadowed(entry) => Some((
                                    format!("shadowed by {}", root_label(entry.shadowed_by)).into(),
                                    theme.warning_muted.opacity(0.9),
                                )),
                                SkillRow::Invalid(_) => {
                                    Some(("invalid".into(), theme.danger_muted.opacity(0.9)))
                                }
                            };
                            let secondary: gpui::SharedString = match row {
                                SkillRow::Ok(skill) => {
                                    SharedString::from(skill.description.clone())
                                }
                                SkillRow::Shadowed(entry) => SharedString::from(entry.file.clone()),
                                SkillRow::Invalid(entry) => {
                                    SharedString::from(entry.message.clone())
                                }
                            };
                            let row_theme = theme.clone();
                            let secondary_color = match row {
                                SkillRow::Invalid(_) => row_theme.danger_muted.opacity(0.9),
                                _ if !enabled => row_theme.text_muted.opacity(0.5),
                                _ => row_theme.text_muted,
                            };
                            let title_color = if enabled {
                                row_theme.text
                            } else {
                                row_theme.text_muted
                            };
                            let mut side = div()
                                .flex_none()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(10.0))
                                .child(
                                    div()
                                        .text_size(crate::typography::ui_rems(10.5))
                                        .text_color(row_theme.text_muted.opacity(0.7))
                                        .child(root_label(row.root())),
                                )
                                .when_some(status, |side, (label, color)| {
                                    side.child(
                                        div()
                                            .text_size(crate::typography::ui_rems(10.5))
                                            .text_color(color)
                                            .child(label),
                                    )
                                });
                            if let SkillRow::Ok(_) = row {
                                let toggle_name = name.clone();
                                let toggle_file = row.file().to_string();
                                let animating =
                                    self.toggle_flip.as_ref().is_some_and(|(file, at)| {
                                        file == row.file()
                                            && at.elapsed() < std::time::Duration::from_millis(400)
                                    });
                                let switch = if animating {
                                    // The state in the key restarts gpui's
                                    // element-id-keyed clock, so the flip
                                    // plays exactly once as the transition.
                                    widgets::animated_toggle_switch(
                                        &row_theme,
                                        enabled,
                                        format!("skill-switch-{}-{}", row.file(), enabled),
                                    )
                                } else {
                                    widgets::toggle_switch(&row_theme, enabled)
                                };
                                side = side.child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "skill-toggle-{}",
                                            row.file()
                                        )))
                                        .flex_none()
                                        .cursor_pointer()
                                        .on_click(cx.listener(move |page, _, _, cx| {
                                            let name = toggle_name.clone();
                                            settings::update(
                                                SavePolicy::Immediate,
                                                cx,
                                                |settings| {
                                                    if enabled {
                                                        if !settings.disabled_skills.contains(&name)
                                                        {
                                                            settings
                                                                .disabled_skills
                                                                .push(name.clone());
                                                            settings.disabled_skills.sort();
                                                        }
                                                    } else {
                                                        settings
                                                            .disabled_skills
                                                            .retain(|n| n != &name);
                                                    }
                                                },
                                            );
                                            page.toggle_flip = Some((
                                                toggle_file.clone(),
                                                std::time::Instant::now(),
                                            ));
                                            cx.notify();
                                        }))
                                        .child(switch),
                                );
                            }
                            list.child(
                                div()
                                    .id(SharedString::from(format!("skill-row-{}", row.file())))
                                    .w_full()
                                    .px(px(12.0))
                                    .py(px(12.0))
                                    .rounded(px(8.0))
                                    .hover(|s| s.bg(ink(0.03)))
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(14.0))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .gap(px(3.0))
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_size(crate::typography::ui_rems(
                                                        widgets::ROW_TITLE_SIZE,
                                                    ))
                                                    .font_weight(gpui::FontWeight::MEDIUM)
                                                    .text_color(title_color)
                                                    .child(name),
                                            )
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
                                    .child(side),
                            )
                        },
                    );
                    list.into_any_element()
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
