//! The skills capability (ADR-skills-referenced-in-place-from-standard-roots/0006): root resolution, catalog
//! assembly, precedence/shadowing, and invocation-prompt building over the
//! upstream agent-loop loader. Skills are referenced in place — nothing is
//! copied, moved, or persisted; the filesystem is the registry, and every
//! catalog use rescans, so there is no cache and no invalidation problem.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use holt_core::agent::harness::skills::{
    LoadedSkills, SkillDiagnostic, format_skill_invocation, load_skills,
};
use holt_core::agent::harness::system_prompt::format_skills_for_system_prompt;
use holt_core::agent::harness::types::{ExecutionEnv, Skill};
use holt_proto::{InvalidSkillEntry, ShadowedSkillEntry, SkillEntry, SkillListing, SkillRoot};

use crate::tools::LocalExecutionEnv;

/// Unknown-window fallback for the system-prompt skill block. Known model
/// windows use 2% of their tokens instead (about four chars per token).
pub(crate) const SKILL_LISTING_BUDGET: usize = 8_000;
const SKILL_LISTING_CONTEXT_PERCENT: usize = 2;
const APPROX_CHARS_PER_TOKEN: usize = 4;
/// Description length (chars) the first truncation pass clamps to.
const SKILL_DESCRIPTION_BUDGET: usize = 160;

fn listing_budget(context_window: Option<u64>) -> usize {
    context_window
        .filter(|window| *window > 0)
        .and_then(|window| usize::try_from(window).ok())
        .map(|window| {
            (window / 100 * SKILL_LISTING_CONTEXT_PERCENT)
                .max(1)
                .saturating_mul(APPROX_CHARS_PER_TOKEN)
        })
        .unwrap_or(SKILL_LISTING_BUDGET)
}

/// The loader's per-root scan, kept as a pair so precedence can group by
/// origin before anything crosses the RPC seam.
struct RootScan {
    root: SkillRoot,
    skills: Vec<Skill>,
    diagnostics: Vec<SkillDiagnostic>,
}

/// A full catalog scan: the precedence winners plus everything the Settings
/// page explains — shadowed losers and load diagnostics. Shadowed entries
/// keep their full [`Skill`] so a path-authoritative mention can still reach
/// the exact file it points at (a linked mention bypasses shadowing).
#[derive(Default)]
pub(crate) struct Catalog {
    /// Valid, unshadowed skills in root-precedence order — the invocable
    /// set the system-prompt block, `/` menu, and name resolutions
    /// resolve against.
    pub(crate) winners: Vec<(Skill, SkillRoot)>,
    shadowed: Vec<(Skill, SkillRoot)>,
    invalid: Vec<InvalidSkillEntry>,
}

impl Catalog {
    /// The `ListSkills` reply shape.
    pub(crate) fn listing(&self) -> SkillListing {
        SkillListing {
            skills: self
                .winners
                .iter()
                .map(|(skill, root)| SkillEntry {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    file: skill.file_path.clone(),
                    root: *root,
                    disable_model_invocation: skill.disable_model_invocation.unwrap_or(false),
                })
                .collect(),
            shadowed: self
                .shadowed
                .iter()
                .map(|(skill, root)| ShadowedSkillEntry {
                    name: skill.name.clone(),
                    file: skill.file_path.clone(),
                    root: *root,
                    shadowed_by: self
                        .winners
                        .iter()
                        .find(|(won, _)| won.name == skill.name)
                        .map(|(_, won_root)| *won_root)
                        .unwrap_or(SkillRoot::Project),
                })
                .collect(),
            invalid: self.invalid.clone(),
        }
    }

    /// Resolve one exact `SKILL.md` path against the scan — winners first,
    /// then shadowed entries. This is the linked-mention lookup: the path is
    /// authoritative, so the skill the link points at wins over any
    /// same-name skill in a nearer root.
    fn by_path(&self, path: &Path) -> Option<&Skill> {
        // The mention side is Path-joined (native spelling), the catalog
        // side loader-produced (addressed spelling) — compare as paths,
        // which is separator-agnostic, never as strings.
        self.winners
            .iter()
            .chain(self.shadowed.iter())
            .find(|(skill, _)| Path::new(&skill.file_path) == path)
            .map(|(skill, _)| skill)
    }
}

/// The model-visible skill advertisement (ADR-metadata-only-skill-advertising): a metadata-only
/// `<available_skills>` block from the upstream formatter, built from the
/// precedence winners. `disable-model-invocation` entries are filtered
/// (by the formatter and here, so the budget counts only what renders);
/// content never enters the block — the model self-serves `SKILL.md`
/// through the read tool on the advertised location.
#[cfg(test)]
pub(crate) fn skills_block(winners: &[(Skill, SkillRoot)]) -> String {
    render_skills_block(winners, SKILL_LISTING_BUDGET)
}

/// Same catalog with the resolved model's budget: 2% of its context window,
/// falling back to the fixed unknown-window value.
pub(crate) fn skills_block_for_context_window(
    winners: &[(Skill, SkillRoot)],
    context_window: Option<u64>,
) -> String {
    render_skills_block(winners, listing_budget(context_window))
}

fn render_skills_block(winners: &[(Skill, SkillRoot)], budget: usize) -> String {
    let visible: Vec<Skill> = winners
        .iter()
        .map(|(skill, _)| skill)
        .filter(|skill| !skill.disable_model_invocation.unwrap_or(false))
        .cloned()
        .collect();
    if visible.is_empty() {
        return String::new();
    }

    let full = format_skills_for_system_prompt(&visible);
    if full.chars().count() <= budget {
        return full;
    }

    // First bound every description. Most catalogs stop here.
    let clamped: Vec<Skill> = visible
        .iter()
        .map(|skill| {
            let mut skill = skill.clone();
            skill.description = truncate_chars(&skill.description, SKILL_DESCRIPTION_BUDGET);
            skill
        })
        .collect();
    let clamped_block = format_skills_for_system_prompt(&clamped);
    if clamped_block.chars().count() <= budget {
        return clamped_block;
    }

    // Names and locations are the minimum useful discovery surface. When
    // they fit, distribute the remaining description space round-robin —
    // never wipe every description just because the catalog is large.
    let minimal: Vec<Skill> = clamped
        .iter()
        .map(|skill| {
            let mut skill = skill.clone();
            skill.description = String::new();
            skill
        })
        .collect();
    let minimal_block = format_skills_for_system_prompt(&minimal);
    let minimal_chars = minimal_block.chars().count();
    if minimal_chars <= budget {
        // The block XML-escapes descriptions (`&` renders as five chars),
        // so the round-robin grants measured in RENDERED chars — a raw
        // count would let one ampersand-heavy description blow the budget
        // after the fact. A description that stays cut also pays one char
        // for its `…` mark, reserved when the first cut char is granted
        // and returned when the description ends up fitting whole.
        let mut remaining = budget - minimal_chars;
        let mut allocations = vec![0usize; clamped.len()];
        let mut cut_marks = vec![false; clamped.len()];
        loop {
            let mut changed = false;
            for (index, skill) in clamped.iter().enumerate() {
                if remaining == 0 {
                    break;
                }
                let length = skill.description.chars().count();
                if allocations[index] >= length {
                    continue;
                }
                let next = skill.description.chars().nth(allocations[index]);
                let Some(next) = next else {
                    continue;
                };
                let mut cost = escaped_char_len(next);
                if !cut_marks[index] && allocations[index] + 1 < length {
                    cost += 1;
                    if cost > remaining {
                        continue;
                    }
                    cut_marks[index] = true;
                    remaining -= 1;
                    cost -= 1;
                }
                if cost > remaining {
                    continue;
                }
                allocations[index] += 1;
                remaining -= cost;
                changed = true;
                if allocations[index] == length && cut_marks[index] {
                    cut_marks[index] = false;
                    remaining += 1;
                }
            }
            if !changed {
                break;
            }
        }
        let fitted: Vec<Skill> = clamped
            .iter()
            .zip(allocations)
            .zip(cut_marks)
            .map(|((skill, allocation), cut)| {
                let mut skill = skill.clone();
                let prefix: String = skill.description.chars().take(allocation).collect();
                skill.description = if cut { format!("{prefix}…") } else { prefix };
                skill
            })
            .collect();
        return format_skills_for_system_prompt(&fitted);
    }

    // Only when even the minimal discovery surface is too large do we omit
    // trailing skills, preserving root precedence instead of hiding the cost.
    let fits = |count: usize| {
        format_skills_for_system_prompt(&minimal[..count])
            .chars()
            .count()
            <= budget
    };
    let mut low = 0;
    let mut high = minimal.len();
    while low < high {
        let mid = (low + high).div_ceil(2);
        if fits(mid) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    format_skills_for_system_prompt(&minimal[..low])
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    // A zero-width slot gets nothing — not even the cut mark, which would
    // itself overflow the budget the caller is holding us to.
    if limit == 0 {
        return String::new();
    }
    let mut truncated: String = text.chars().take(limit.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

/// One char's rendered width inside the skills block: the upstream
/// formatter XML-escapes descriptions, so `&` counts five chars, a quote
/// six, and everything else one. The budget math grants chars in these
/// units so a fitted block can never exceed its budget after escaping.
fn escaped_char_len(ch: char) -> usize {
    match ch {
        '&' => 5,
        '<' | '>' => 4,
        '"' | '\'' => 6,
        _ => 1,
    }
}

/// The engine's fixed skill roots: the project root derives from each
/// chat's cwd, the other two are resolved once at assembly.
#[derive(Clone)]
pub(crate) struct Skills {
    personal: PathBuf,
    holt: PathBuf,
}

impl Skills {
    /// `holt` is `<data_dir>/skills`; `personal` defaults to
    /// `~/.agents/skills` unless an override pins it (tests, deployments).
    pub(crate) fn new(data_dir: &Path, personal_override: Option<&Path>) -> Self {
        let personal = personal_override
            .map(Path::to_path_buf)
            .or_else(|| {
                crate::local_fs::home_dir()
                    .map(|home| Path::new(&home).join(".agents").join("skills"))
            })
            .unwrap_or_else(|| PathBuf::from(".agents/skills"));
        Self {
            personal,
            holt: data_dir.join("skills"),
        }
    }

    /// The skill directories that live OUTSIDE any workspace root — personal
    /// and holt (the project root sits under the chat's cwd, already inside
    /// the workspace fence). The sidebar's file READ allows these so a
    /// skill's `SKILL.md` opens in the file tab wherever the skill lives;
    /// saves and every other workspace verb stay workspace-fenced.
    pub(crate) fn out_of_workspace_roots(&self) -> Vec<PathBuf> {
        vec![self.personal.clone(), self.holt.clone()]
    }

    /// The roots for one chat, nearest first (ADR-skills-referenced-in-place-from-standard-roots precedence).
    fn roots(&self, cwd: Option<&str>) -> Vec<(SkillRoot, PathBuf)> {
        let mut roots = Vec::new();
        if let Some(cwd) = cwd.filter(|cwd| !cwd.trim().is_empty()) {
            roots.push((
                SkillRoot::Project,
                Path::new(cwd).join(".agents").join("skills"),
            ));
        }
        roots.push((SkillRoot::Personal, self.personal.clone()));
        roots.push((SkillRoot::Holt, self.holt.clone()));
        roots
    }

    /// Scan every root fresh and assemble the catalog: nearest root wins on
    /// name collisions, losers surface as shadowed, loader diagnostics as
    /// invalid entries. Missing roots are skipped silently (fresh machines
    /// get an empty catalog, never an error).
    pub(crate) async fn catalog(&self, cwd: Option<&str>) -> Catalog {
        // The anchor is a placeholder — every scanned root is absolute, so
        // the env's cwd never joins into a probe path — but it must be a
        // valid root spelling on the host platform (LocalExecutionEnv
        // canonicalizes the anchor).
        let env: Arc<dyn ExecutionEnv> = Arc::new(LocalExecutionEnv::new(if cfg!(windows) {
            "C:\\"
        } else {
            "/"
        }));
        let mut scans = Vec::new();
        for (root, dir) in self.roots(cwd) {
            let LoadedSkills {
                skills,
                diagnostics,
            } = load_skills(&env, &[dir.to_string_lossy().into_owned()]).await;
            scans.push(RootScan {
                root,
                skills,
                diagnostics,
            });
        }
        assemble_catalog(scans)
    }

    /// Resolve one inline mention (ADR-skill-invocation-is-an-inline-mention): the linked form's path is
    /// authoritative — exact file match against winners AND shadowed
    /// entries (`~` and relative paths expand against the chat's working
    /// directory first) — falling back to a name lookup against the
    /// invocable winners (shadowed and invalid entries are as good as
    /// absent to a name). The bare `$name` form resolves by name alone.
    pub(crate) async fn resolve_mention(
        &self,
        cwd: Option<&str>,
        mention: &holt_doc::SkillMention,
    ) -> Option<Skill> {
        let catalog = self.catalog(cwd).await;
        if let Some(path) = &mention.path {
            let path = expand_home(path);
            let path = match (Path::new(&path).is_absolute(), cwd) {
                (false, Some(cwd)) => Path::new(cwd).join(&path),
                _ => PathBuf::from(&path),
            };
            if let Some(skill) = catalog.by_path(&path) {
                return Some(skill.clone());
            }
        }
        catalog
            .winners
            .into_iter()
            .find(|(skill, _)| skill.name == mention.name)
            .map(|(skill, _)| skill)
    }

    /// Rewrite a user prompt for the model: every resolved mention's
    /// `<skill>` block is prepended (blocks joined by a blank line, the
    /// message text after them, verbatim); unresolved mentions stay ordinary
    /// text. The returned chips ride the AGENT entry so the transcript
    /// shows exactly what was injected. Skipped entirely when the text
    /// holds no `$` — the common prompt never pays for a catalog scan.
    pub(crate) async fn resolve_prompt_mentions(
        &self,
        cwd: &str,
        text: &str,
    ) -> (String, Vec<holt_doc::MessagePart>) {
        if !text.contains('$') {
            return (text.to_string(), Vec::new());
        }
        let mentions = holt_doc::skill_mentions(text);
        if mentions.is_empty() {
            return (text.to_string(), Vec::new());
        }
        let mut blocks: Vec<String> = Vec::new();
        let mut chips: Vec<holt_doc::MessagePart> = Vec::new();
        for mention in mentions {
            let Some(skill) = self.resolve_mention(Some(cwd), &mention).await else {
                continue;
            };
            // The model-visible prompt of a mention hit: the upstream
            // `<skill>` block — full content with its relative-path-resolve
            // declaration. No host-side argument substitution (ADR-metadata-only-skill-advertising).
            let block = invocation_prompt(&skill, None);
            blocks.push(block.clone());
            chips.push(holt_doc::MessagePart::Skill {
                id: format!("s{}", chips.len()),
                name: skill.name.clone(),
                file: skill.file_path.clone(),
                content: Some(block),
            });
        }
        if blocks.is_empty() {
            return (text.to_string(), Vec::new());
        }
        (format!("{}\n\n{text}", blocks.join("\n\n")), chips)
    }
}

/// `~` at the start of a mention path expands against the user's home
/// directory; anything else passes through untouched.
fn expand_home(path: &str) -> String {
    let rest = match path.strip_prefix("~/") {
        Some(rest) => Some(rest),
        // A Windows-spelled home path; on unix `~\` is a plain filename.
        None if cfg!(windows) => path.strip_prefix("~\\"),
        None => None,
    };
    rest.map(|rest| {
        crate::local_fs::home_dir()
            .map(|home| Path::new(&home).join(rest).to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string())
    })
    .unwrap_or_else(|| path.to_string())
}

/// The model-visible prompt of a `/skill` invocation: the upstream
/// `<skill>` block — full content with its relative-path-resolve
/// declaration — plus any extra instructions verbatim. Still the legacy
/// pending-item admission shape; inline mentions reuse the block through
/// [`Self::resolve_prompt_mentions`].
pub(crate) fn invocation_prompt(skill: &Skill, extra_instructions: Option<&str>) -> String {
    format_skill_invocation(skill, extra_instructions)
}

/// Group the per-root scans into the catalog: a skill the loader flagged
/// (any diagnostic naming its file) is invalid, not invocable; diagnostics
/// that match no loaded skill (parse failures, traversal faults) surface on
/// their own so the Settings page can show them.
fn assemble_catalog(scans: Vec<RootScan>) -> Catalog {
    let mut winners: Vec<(Skill, SkillRoot)> = Vec::new();
    let mut shadowed: Vec<(Skill, SkillRoot)> = Vec::new();
    let mut invalid: Vec<InvalidSkillEntry> = Vec::new();

    for RootScan {
        root,
        skills,
        diagnostics,
    } in scans
    {
        for skill in skills {
            let messages: Vec<&str> = diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.path == skill.file_path)
                .map(|diagnostic| diagnostic.message.as_str())
                .collect();
            if !messages.is_empty() {
                invalid.push(InvalidSkillEntry {
                    file: skill.file_path.clone(),
                    root,
                    name: Some(skill.name.clone()),
                    message: messages.join("; "),
                });
                continue;
            }
            // Nearest root wins: roots arrive in precedence order, so the
            // first root to claim a name keeps it; later same-names are
            // shadowed by that winner (kept in full for path resolution).
            if winners.iter().any(|(won, _)| won.name == skill.name) {
                shadowed.push((skill, root));
            } else {
                winners.push((skill, root));
            }
        }
        // Diagnostics left after the skill pass belong to entries that never
        // loaded (parse failures) or to the roots themselves (traversal
        // faults); anything already carried by an invalid entry is spent.
        for diagnostic in diagnostics {
            let carried = invalid.iter().any(|entry| entry.file == diagnostic.path);
            if !carried {
                invalid.push(InvalidSkillEntry {
                    file: diagnostic.path.clone(),
                    root,
                    name: None,
                    message: diagnostic.message,
                });
            }
        }
    }
    Catalog {
        winners,
        shadowed,
        invalid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_skill(name: &str, description: &str, disable: bool) -> (Skill, SkillRoot) {
        (
            Skill {
                name: name.into(),
                description: description.into(),
                content: "SECRET INSTRUCTIONS".into(),
                file_path: format!("/roots/{name}/SKILL.md"),
                disable_model_invocation: Some(disable),
            },
            SkillRoot::Personal,
        )
    }

    #[test]
    fn skills_block_is_metadata_only() {
        let block = skills_block(&[catalog_skill("grill", "Grill a plan.", false)]);
        assert!(block.contains("<available_skills>"));
        assert!(block.contains("<name>grill</name>"));
        assert!(block.contains("<description>Grill a plan.</description>"));
        assert!(block.contains("<location>/roots/grill/SKILL.md</location>"));
        // Content never enters the advertisement (ADR-metadata-only-skill-advertising).
        assert!(!block.contains("SECRET INSTRUCTIONS"));
    }

    #[test]
    fn skills_block_filters_disable_model_invocation() {
        let winners = [
            catalog_skill("visible", "Shown.", false),
            catalog_skill("hidden", "Never advertised.", true),
        ];
        let block = skills_block(&winners);
        assert!(block.contains("<name>visible</name>"));
        assert!(!block.contains("hidden"));
    }

    #[test]
    fn skills_block_is_empty_without_visible_skills() {
        assert_eq!(skills_block(&[]), "");
        assert_eq!(skills_block(&[catalog_skill("hidden", "Nope.", true)]), "");
    }

    #[tokio::test]
    async fn mentions_resolve_path_first_name_fallback() {
        let base = tempfile::tempdir().unwrap();
        let personal = base.path().join("personal");
        skill_in(
            &personal,
            "near",
            "name: near\ndescription: Winner.\n",
            "content",
        );
        // Shadowed by the project root…
        let project = base.path().join("project");
        let project_skills = project.join(".agents").join("skills");
        std::fs::create_dir_all(&project_skills).unwrap();
        let shadowed_file = skill_in(
            &project_skills,
            "near",
            "name: near\ndescription: Nearer.\n",
            "content",
        );
        // …and invalid (name ≠ directory).
        skill_in(
            &personal,
            "busted",
            "name: wrong\ndescription: Broken.\n",
            "content",
        );

        let skills = Skills::new(&base.path().join("data"), Some(&personal));
        let cwd = project.to_string_lossy().into_owned();
        let bare = |name: &str| holt_doc::SkillMention {
            name: name.into(),
            path: None,
            range: 0..0,
        };
        // A bare name resolves to the winner…
        let resolved = skills
            .resolve_mention(Some(&cwd), &bare("near"))
            .await
            .unwrap();
        assert!(resolved.file_path.contains("project"));
        // …while a linked form pointing at the SHADOWED file gets exactly
        // that file — the path is authoritative (ADR-skill-invocation-is-an-inline-mention).
        let linked = holt_doc::SkillMention {
            name: "near".into(),
            path: Some(shadowed_file.clone()),
            range: 0..0,
        };
        let resolved = skills.resolve_mention(Some(&cwd), &linked).await.unwrap();
        assert_eq!(resolved.file_path, addressed(&shadowed_file));
        // Invalid and absent names stay unresolved even via link fallback.
        assert!(
            skills
                .resolve_mention(Some(&cwd), &bare("busted"))
                .await
                .is_none()
        );
        assert!(
            skills
                .resolve_mention(Some(&cwd), &bare("absent"))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn prompt_mentions_prepend_blocks_and_keep_text_verbatim() {
        let base = tempfile::tempdir().unwrap();
        let personal = base.path().join("personal");
        let file = skill_in(
            &personal,
            "grill",
            "name: grill\ndescription: Grill a plan.\n",
            "SECRET INSTRUCTIONS",
        );
        let skills = Skills::new(&base.path().join("data"), Some(&personal));
        let text = format!("[$grill]({file}) focus on the data layer, and $100 stays literal");
        let (prompt, chips) = skills.resolve_prompt_mentions("/nowhere", &text).await;
        assert!(prompt.starts_with("<skill name=\"grill\""));
        assert!(prompt.ends_with("focus on the data layer, and $100 stays literal"));
        assert_eq!(chips.len(), 1);
        assert!(matches!(
            &chips[0],
            holt_doc::MessagePart::Skill { file: chip_file, .. }
                if chip_file == &addressed(&file)
        ));
        // No mention resolves → the text is returned untouched, no chips.
        let (prompt, chips) = skills
            .resolve_prompt_mentions("/nowhere", "just $absent here")
            .await;
        assert_eq!(prompt, "just $absent here");
        assert!(chips.is_empty());
    }

    #[test]
    fn invocation_prompt_is_the_block_plus_extra_verbatim() {
        let (skill, _) = catalog_skill("grill", "Grill a plan.", false);
        let prompt = invocation_prompt(&skill, Some("Focus on the data layer."));
        assert!(prompt.starts_with("<skill name=\"grill\""));
        assert!(prompt.contains("References are relative to /roots/grill."));
        assert!(prompt.contains("SECRET INSTRUCTIONS"));
        assert!(prompt.ends_with("</skill>\n\nFocus on the data layer."));
        // No argument substitution — extra rides verbatim, raw $ tokens intact.
        let verbatim = invocation_prompt(&skill, Some("Use $ARGUMENTS literally."));
        assert!(verbatim.contains("Use $ARGUMENTS literally."));
        assert_eq!(
            invocation_prompt(&skill, None),
            format_skill_invocation(&skill, None)
        );
    }

    #[test]
    fn known_context_windows_scale_the_listing_budget() {
        assert_eq!(listing_budget(None), 8_000);
        assert_eq!(listing_budget(Some(200_000)), 16_000);

        let winners: Vec<(Skill, SkillRoot)> = (0..20)
            .map(|i| catalog_skill(&format!("skill-{i}"), &"x".repeat(400), false))
            .collect();
        let visible: Vec<Skill> = winners.iter().map(|(skill, _)| skill.clone()).collect();
        let full = format_skills_for_system_prompt(&visible);
        assert!(full.chars().count() > SKILL_LISTING_BUDGET);
        assert!(full.chars().count() <= 16_000);
        let block = skills_block_for_context_window(&winners, Some(200_000));
        assert_eq!(block, full, "every full description fits at 2%");
    }

    #[test]
    fn a_small_budget_truncates_descriptions_instead_of_dropping_them_all() {
        let winners: Vec<(Skill, SkillRoot)> = (0..4)
            .map(|i| catalog_skill(&format!("skill-{i}"), &"abcdefghij".repeat(16), false))
            .collect();
        let block = render_skills_block(&winners, 1_200);
        assert!(block.chars().count() <= 1_200);
        assert_eq!(block.matches("<skill>").count(), 4);
        assert!(!block.contains("<description></description>"));
        assert!(block.contains('…'));
        // Every skill gets a useful, truncated description rather than one
        // skill consuming the whole remaining budget.
        assert_eq!(block.matches('…').count(), 4);
    }

    #[test]
    fn oversized_catalog_truncates_under_the_budget() {
        let long = "very long description ".repeat(120);
        let winners: Vec<(Skill, SkillRoot)> = (0..400)
            .map(|i| catalog_skill(&format!("bulk-skill-{i:03}"), &long, false))
            .collect();
        let block = skills_block(&winners);
        assert!(block.chars().count() <= SKILL_LISTING_BUDGET);
        assert!(!block.contains(&long));
        // The nearest-first skills survive the cut.
        assert!(block.contains("<name>bulk-skill-000</name>"));
    }

    #[test]
    fn escape_heavy_descriptions_stay_inside_the_budget() {
        // The block XML-escapes descriptions, so `&` renders five chars wide
        // and a quote six: the round-robin must grant chars by their
        // RENDERED width, or one ampersand-heavy catalog would blow the
        // budget after the fact.
        let spicy = "&\"'<>&\"'<>'".repeat(16);
        let winners: Vec<(Skill, SkillRoot)> = (0..4)
            .map(|i| catalog_skill(&format!("skill-{i}"), &spicy, false))
            .collect();
        let block = render_skills_block(&winners, 1_200);
        assert!(
            block.chars().count() <= 1_200,
            "rendered {} chars for a 1_200 budget:\n{block}",
            block.chars().count()
        );
        assert_eq!(block.matches("<skill>").count(), 4);
        // Every description was cut, and each cut mark was paid for out of
        // the same budget.
        assert_eq!(block.matches('…').count(), 4);

        // A budget that only fits names and locations grants no description
        // chars at all — and no cut marks sneak past a zero-width slot.
        let minimal: Vec<Skill> = winners
            .iter()
            .map(|(skill, _)| {
                let mut skill = skill.clone();
                skill.description = String::new();
                skill
            })
            .collect();
        let minimal_chars = format_skills_for_system_prompt(&minimal).chars().count();
        let bare = render_skills_block(&winners, minimal_chars);
        assert_eq!(bare.chars().count(), minimal_chars);
        assert!(!bare.contains('…'));
        assert!(bare.contains("<description></description>"));
        // The zero-width clamp holds directly too: nothing, not even the cut
        // mark, fits a zero budget.
        assert_eq!(truncate_chars("anything", 0), "");
    }

    fn skill_in(root: &Path, name: &str, frontmatter: &str, body: &str) -> String {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("SKILL.md");
        std::fs::write(&file, format!("---\n{frontmatter}---\n{body}")).unwrap();
        file.to_string_lossy().into_owned()
    }

    /// Catalog paths are in the addressed spelling (`/` separators); the
    /// tempdir fixtures spell natively on Windows.
    fn addressed(path: &str) -> String {
        if cfg!(windows) {
            path.replace('\\', "/")
        } else {
            path.to_string()
        }
    }

    #[tokio::test]
    async fn fresh_machine_yields_an_empty_catalog() {
        let base = tempfile::tempdir().unwrap();
        let skills = Skills::new(
            &base.path().join("data"),
            Some(&base.path().join("personal")),
        );
        let catalog = skills.catalog(Some("/definitely/not/here")).await;
        assert_eq!(catalog.listing(), SkillListing::default());
    }

    #[tokio::test]
    async fn three_root_discovery_and_nearest_wins() {
        let base = tempfile::tempdir().unwrap();
        let project = base.path().join("project");
        let personal = base.path().join("personal");
        let data = base.path().join("data");
        let project_skills = project.join(".agents").join("skills");
        let personal_skills = personal.join(".agents").join("skills");
        std::fs::create_dir_all(&project_skills).unwrap();
        std::fs::create_dir_all(&personal_skills).unwrap();
        let cwd = project.to_string_lossy().into_owned();

        let skills = Skills::new(&data, Some(&personal_skills));
        assert_eq!(
            skills.catalog(Some(&cwd)).await.listing(),
            SkillListing::default()
        );

        let personal_file = skill_in(
            &personal_skills,
            "shared-name",
            "name: shared-name\ndescription: Only in personal.\n",
            "body",
        );
        let listing = skills.catalog(Some(&cwd)).await.listing();
        assert_eq!(listing.skills.len(), 1);
        assert_eq!(listing.skills[0].file, addressed(&personal_file));
        assert_eq!(listing.skills[0].root, SkillRoot::Personal);

        // Same name in a nearer root shadows the personal entry.
        let project_file = skill_in(
            &project_skills,
            "shared-name",
            "name: shared-name\ndescription: From the project.\n",
            "body",
        );
        let listing = skills.catalog(Some(&cwd)).await.listing();
        assert_eq!(listing.skills.len(), 1);
        assert_eq!(listing.skills[0].file, addressed(&project_file));
        assert_eq!(listing.skills[0].root, SkillRoot::Project);
        assert_eq!(listing.shadowed.len(), 1);
        assert_eq!(listing.shadowed[0].root, SkillRoot::Personal);
        assert_eq!(listing.shadowed[0].shadowed_by, SkillRoot::Project);

        // Holt's own root loses to both, and the scan is fresh: the entry
        // appeared without rebuilding anything.
        skill_in(
            &data.join("skills"),
            "shared-name",
            "name: shared-name\ndescription: From holt.\n",
            "body",
        );
        let listing = skills.catalog(Some(&cwd)).await.listing();
        assert_eq!(listing.skills[0].root, SkillRoot::Project);
        assert_eq!(listing.shadowed.len(), 2);
        assert!(
            listing
                .shadowed
                .iter()
                .any(|entry| entry.root == SkillRoot::Holt)
        );
    }

    #[tokio::test]
    async fn invalid_skills_surface_diagnostics_and_stay_uninvocable() {
        let base = tempfile::tempdir().unwrap();
        let personal = base.path().join("personal");
        // Name does not match the parent directory.
        skill_in(
            &personal,
            "actual-dir",
            "name: wrong-name\ndescription: fine.\n",
            "body",
        );
        // No description at all.
        skill_in(
            &personal,
            "no-description",
            "name: no-description\n",
            "body",
        );

        let skills = Skills::new(&base.path().join("data"), Some(&personal));
        let listing = skills.catalog(None).await.listing();
        assert_eq!(listing.skills, vec![]);
        assert_eq!(listing.invalid.len(), 2);
        assert!(
            listing
                .invalid
                .iter()
                .any(|entry| entry.name.as_deref() == Some("wrong-name")
                    && entry.message.contains("does not match parent directory"))
        );
        assert!(
            listing
                .invalid
                .iter()
                .any(|entry| entry.message.contains("description is required"))
        );
    }

    #[tokio::test]
    async fn ignore_files_keep_entries_out_of_the_catalog() {
        let base = tempfile::tempdir().unwrap();
        let personal = base.path().join("personal");
        skill_in(
            &personal,
            "kept",
            "name: kept\ndescription: kept.\n",
            "body",
        );
        skill_in(
            &personal,
            "draft",
            "name: draft\ndescription: drafted.\n",
            "body",
        );
        std::fs::write(personal.join(".gitignore"), "draft/\n").unwrap();

        let skills = Skills::new(&base.path().join("data"), Some(&personal));
        let listing = skills.catalog(None).await.listing();
        assert_eq!(listing.skills.len(), 1);
        assert_eq!(listing.skills[0].name, "kept");
    }
}
