//! Slash-command interception (ADR-0006): the composer recognizes `/skill`
//! on submit and handles it itself — the raw directive never becomes a
//! prompt. Pure over the input text so the recognition is unit-testable
//! per the picker-logic pattern. Also owns the `/` popup's mixed-source
//! candidate model (skills + provider commands).

use holt_proto::{SkillListing, SlashCommand};

/// What the composer learned from one input string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Parsed {
    /// Ordinary text — send it as the prompt, untouched.
    Plain,
    /// `/skill <name> [extra…]` — intercepted; never sent as prompt text.
    Skill {
        name: String,
        /// Everything after the name, trimmed; `None` when nothing follows.
        extra: Option<String>,
    },
    /// `/skill` with no name — still intercepted (the raw directive must
    /// never leak), with the usage message for the composer to surface.
    Malformed,
}

/// One `/` popup row, from either source: a catalog skill (ADR-0005 —
/// provider-agnostic, survives provider switches) or a provider
/// prompt-prefix completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlashCandidate {
    Skill {
        name: String,
        description: String,
    },
    Command {
        name: String,
        description: String,
        input_hint: Option<String>,
    },
}

impl SlashCandidate {
    /// The row's primary label and what accepting fills into the composer.
    pub(crate) fn title(&self) -> String {
        match self {
            SlashCandidate::Skill { name, .. } => format!("/skill {name}"),
            SlashCandidate::Command { name, .. } => format!("/{name}"),
        }
    }

    /// The row's secondary line.
    pub(crate) fn description(&self) -> String {
        match self {
            SlashCandidate::Skill { description, .. } => description.clone(),
            SlashCandidate::Command {
                description,
                input_hint: Some(hint),
                ..
            } if description.is_empty() => format!("<{hint}>"),
            SlashCandidate::Command {
                description,
                input_hint: Some(hint),
                ..
            } => format!("{description} · <{hint}>"),
            SlashCandidate::Command {
                description,
                input_hint: None,
                ..
            } => description.clone(),
        }
    }
}

/// Merge the popup's two sources: invocable skills first, the provider's
/// commands after. The valid-entry filter rides the listing's shape — only
/// `skills` entries are invocable; shadowed and invalid entries never
/// enter the menu.
pub(crate) fn popup_candidates(
    listing: &SkillListing,
    commands: &[SlashCommand],
) -> Vec<SlashCandidate> {
    listing
        .skills
        .iter()
        .map(|skill| SlashCandidate::Skill {
            name: skill.name.clone(),
            description: skill.description.clone(),
        })
        .chain(commands.iter().map(|command| SlashCandidate::Command {
            name: command.name.clone(),
            description: command.description.clone(),
            input_hint: command.input_hint.clone(),
        }))
        .collect()
}

/// Recognize a leading `/skill` directive. Only the start of the input is
/// a command (`hello /skill x` is ordinary text), and `/skills`-style
/// longer words are not `/skill`.
pub(crate) fn parse(text: &str) -> Parsed {
    let Some(rest) = text.trim_start().strip_prefix("/skill") else {
        return Parsed::Plain;
    };
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return Parsed::Plain;
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return Parsed::Malformed;
    }
    let name = rest.split_whitespace().next().unwrap_or_default();
    if name.is_empty() {
        return Parsed::Malformed;
    }
    let extra = rest[name.len()..].trim();
    Parsed::Skill {
        name: name.to_string(),
        extra: (!extra.is_empty()).then(|| extra.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_name_and_extra() {
        assert_eq!(
            parse("/skill grill"),
            Parsed::Skill {
                name: "grill".into(),
                extra: None,
            }
        );
        assert_eq!(
            parse("  /skill grill  focus on the data layer  "),
            Parsed::Skill {
                name: "grill".into(),
                extra: Some("focus on the data layer".into()),
            }
        );
    }

    #[test]
    fn bare_directive_is_malformed_not_plain() {
        // The raw directive must never fall through to the prompt path.
        assert_eq!(parse("/skill"), Parsed::Malformed);
        assert_eq!(parse("   /skill   "), Parsed::Malformed);
    }

    #[test]
    fn longer_words_and_mid_text_directives_stay_plain() {
        assert_eq!(parse("/skills grill"), Parsed::Plain);
        assert_eq!(parse("/skillbook"), Parsed::Plain);
        assert_eq!(parse("run /skill grill please"), Parsed::Plain);
        assert_eq!(parse("just a normal prompt"), Parsed::Plain);
        assert_eq!(parse(""), Parsed::Plain);
    }

    fn listing_entry(name: &str, description: &str) -> holt_proto::SkillEntry {
        holt_proto::SkillEntry {
            name: name.into(),
            description: description.into(),
            file: format!("/roots/{name}/SKILL.md"),
            root: holt_proto::SkillRoot::Personal,
            disable_model_invocation: false,
        }
    }

    #[test]
    fn popup_candidates_merge_skills_before_commands() {
        let listing = SkillListing {
            skills: vec![listing_entry("grill", "Grill a plan.")],
            ..SkillListing::default()
        };
        let commands = vec![SlashCommand {
            name: "compact".into(),
            description: "Compact the session.".into(),
            input_hint: None,
        }];
        assert_eq!(
            popup_candidates(&listing, &commands),
            vec![
                SlashCandidate::Skill {
                    name: "grill".into(),
                    description: "Grill a plan.".into(),
                },
                SlashCandidate::Command {
                    name: "compact".into(),
                    description: "Compact the session.".into(),
                    input_hint: None,
                },
            ]
        );
        // The row label is also the fill text: `/skill grill` is ready for
        // extra instructions and submit.
        assert_eq!(
            popup_candidates(&listing, &commands)[0].title(),
            "/skill grill"
        );
    }

    #[test]
    fn popup_candidates_take_only_invocable_entries_from_a_listing() {
        // Shadowed and invalid entries ride their own arrays; the menu
        // builds from `skills` alone — `disable-model-invocation` stays
        // manually invocable, so it stays.
        let listing = SkillListing {
            skills: vec![
                listing_entry("grill", "Grill a plan."),
                holt_proto::SkillEntry {
                    disable_model_invocation: true,
                    ..listing_entry("manual-only", "Forced by the user.")
                },
            ],
            shadowed: vec![holt_proto::ShadowedSkillEntry {
                name: "grill".into(),
                file: "/holt/skills/grill/SKILL.md".into(),
                root: holt_proto::SkillRoot::Holt,
                shadowed_by: holt_proto::SkillRoot::Project,
            }],
            invalid: vec![holt_proto::InvalidSkillEntry {
                file: "/repo/.agents/skills/draft/SKILL.md".into(),
                root: holt_proto::SkillRoot::Project,
                name: None,
                message: "description is required".into(),
            }],
        };
        let candidates = popup_candidates(&listing, &[]);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].title(), "/skill grill");
        assert_eq!(candidates[1].title(), "/skill manual-only");
    }
}
