//! Slash-command interception (ADR-0006): the composer recognizes `/skill`
//! on submit and handles it itself — the raw directive never becomes a
//! prompt. Pure over the input text so the recognition is unit-testable
//! per the picker-logic pattern. Also owns the `/` popup's mixed-source
//! candidate model (skills + provider commands).

use holt_proto::{SkillListing, SkillRoot, SlashCommand};

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
    /// `/compact` — intercepted; never sent as prompt text (ADR-0011).
    Compact,
    /// `/compact` with arguments — still intercepted, with the usage
    /// message for the composer to surface.
    MalformedCompact,
}

/// One `/` popup row, from either source: a catalog skill (ADR-0005 —
/// provider-agnostic, survives provider switches) or a provider
/// prompt-prefix completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlashCandidate {
    Skill {
        name: String,
        description: String,
        /// The source root, shown as the row's right-aligned tag.
        root: SkillRoot,
    },
    Command {
        name: String,
        description: String,
        input_hint: Option<String>,
    },
}

impl SlashCandidate {
    /// What accepting fills into the composer: `/skill <name>` for a skill
    /// — ready for extra instructions and submit — `/name` for a command
    /// (`/compact` submits on accept; there is nothing to edit).
    pub(crate) fn title(&self) -> String {
        match self {
            SlashCandidate::Skill { name, .. } => format!("/skill {name}"),
            SlashCandidate::Command { name, .. } => format!("/{name}"),
        }
    }

    /// The row's primary label: a skill shows its bare name (the reference
    /// menu style), a command its slash word — each row names its own
    /// identity.
    pub(crate) fn row_label(&self) -> String {
        match self {
            SlashCandidate::Skill { name, .. } => name.to_string(),
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

    /// What per-keystroke local filtering matches against: name plus
    /// description for skills, the slash word plus description for
    /// commands (typing `co` still finds `/compact`).
    pub(crate) fn filter_label(&self) -> String {
        match self {
            SlashCandidate::Skill {
                name, description, ..
            } => format!("{name} {description}"),
            SlashCandidate::Command { .. } => {
                format!("{} {}", self.row_label(), self.description())
            }
        }
    }

    /// The right-aligned source-root tag; commands carry none.
    pub(crate) fn root_tag(&self) -> Option<&'static str> {
        match self {
            SlashCandidate::Skill { root, .. } => Some(match root {
                SkillRoot::Project => "project",
                SkillRoot::Personal => "personal",
                SkillRoot::Holt => "holt",
            }),
            SlashCandidate::Command { .. } => None,
        }
    }
}

/// Merge the popup's two sources: the provider's commands first, invocable
/// skills after. The valid-entry filter rides the listing's shape — only
/// `skills` entries are invocable; shadowed and invalid entries never
/// enter the menu.
pub(crate) fn popup_candidates(
    listing: &SkillListing,
    commands: &[SlashCommand],
) -> Vec<SlashCandidate> {
    commands
        .iter()
        .map(|command| SlashCandidate::Command {
            name: command.name.clone(),
            description: command.description.clone(),
            input_hint: command.input_hint.clone(),
        })
        .chain(listing.skills.iter().map(|skill| SlashCandidate::Skill {
            name: skill.name.clone(),
            description: skill.description.clone(),
            root: skill.root,
        }))
        .collect()
}

/// Recognize a leading `/skill` directive. Only the start of the input is
/// a command (`hello /skill x` is ordinary text), and `/skills`-style
/// longer words are not `/skill`.
pub(crate) fn parse(text: &str) -> Parsed {
    let Some(rest) = text.trim_start().strip_prefix("/skill") else {
        return parse_compact(text);
    };
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return parse_compact(text);
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

/// `/compact` takes no arguments; `/compacted`-style longer words stay
/// plain text.
fn parse_compact(text: &str) -> Parsed {
    let Some(rest) = text.trim_start().strip_prefix("/compact") else {
        return Parsed::Plain;
    };
    if rest.is_empty() || rest.trim().is_empty() {
        return Parsed::Compact;
    }
    if rest.starts_with(char::is_whitespace) {
        return Parsed::MalformedCompact;
    }
    // `/compacted …` and friends are ordinary text.
    Parsed::Plain
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
    fn recognizes_compact_and_refuses_arguments() {
        assert_eq!(parse("/compact"), Parsed::Compact);
        assert_eq!(parse("  /compact  "), Parsed::Compact);
        // Arguments are refused — still intercepted, usage surfaced.
        assert_eq!(parse("/compact now"), Parsed::MalformedCompact);
        // Longer words stay ordinary text.
        assert_eq!(parse("/compacted"), Parsed::Plain);
        assert_eq!(parse("/compaction please"), Parsed::Plain);
        // Mid-text directives stay ordinary text.
        assert_eq!(parse("please /compact"), Parsed::Plain);
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
    fn popup_candidates_merge_commands_before_skills() {
        let listing = SkillListing {
            skills: vec![listing_entry("grill", "Grill a plan.")],
            ..SkillListing::default()
        };
        let commands = vec![SlashCommand {
            name: "compact".into(),
            description: "Compact the session.".into(),
            input_hint: None,
        }];
        let candidates = popup_candidates(&listing, &commands);
        assert_eq!(
            candidates,
            vec![
                SlashCandidate::Command {
                    name: "compact".into(),
                    description: "Compact the session.".into(),
                    input_hint: None,
                },
                SlashCandidate::Skill {
                    name: "grill".into(),
                    description: "Grill a plan.".into(),
                    root: SkillRoot::Personal,
                },
            ]
        );
        // The fill text stays `/skill grill` — ready for extra instructions
        // and submit — while the ROW shows the bare name and its root tag.
        assert_eq!(candidates[1].title(), "/skill grill");
        assert_eq!(candidates[1].row_label(), "grill");
        assert_eq!(candidates[1].root_tag(), Some("personal"));
        assert_eq!(candidates[0].row_label(), "/compact");
        assert_eq!(candidates[0].root_tag(), None);
        // Filtering matches name+description for skills, slash word for
        // commands.
        assert!(candidates[1].filter_label().contains("Grill a plan."));
        assert!(candidates[0].filter_label().starts_with("/compact"));
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
