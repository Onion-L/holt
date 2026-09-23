//! The skills capability (ADR-0005/0006): root resolution, catalog
//! assembly, precedence/shadowing, and invocation-prompt building over the
//! upstream agent-loop loader. Skills are referenced in place — nothing is
//! copied, moved, or persisted; the filesystem is the registry, and every
//! catalog use rescans, so there is no cache and no invalidation problem.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use holt_proto::{InvalidSkillEntry, ShadowedSkillEntry, SkillEntry, SkillListing, SkillRoot};
use pi_core::agent::harness::skills::{
    LoadedSkills, SkillDiagnostic, format_skill_invocation, load_skills,
};
use pi_core::agent::harness::system_prompt::format_skills_for_system_prompt;
use pi_core::agent::harness::types::{ExecutionEnv, Skill};

use crate::tools::LocalExecutionEnv;

/// Character budget for the system-prompt skill block (ADR-0006): past the
/// limit descriptions truncate, then drop, then trailing skills drop —
/// never an error, and the task stays the bulk of the context window.
pub(crate) const SKILL_LISTING_BUDGET: usize = 8 * 1024;
/// Description length (chars) the first truncation pass clamps to.
const SKILL_DESCRIPTION_BUDGET: usize = 160;

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
        let wanted = path.to_string_lossy();
        self.winners
            .iter()
            .chain(self.shadowed.iter())
            .find(|(skill, _)| skill.file_path == wanted)
            .map(|(skill, _)| skill)
    }
}

/// The model-visible skill advertisement (ADR-0006): a metadata-only
/// `<available_skills>` block from the upstream formatter, built from the
/// precedence winners. `disable-model-invocation` entries are filtered
/// (by the formatter and here, so the budget counts only what renders);
/// content never enters the block — the model self-serves `SKILL.md`
/// through the read tool on the advertised location.
pub(crate) fn skills_block(winners: &[(Skill, SkillRoot)]) -> String {
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
    if full.chars().count() <= SKILL_LISTING_BUDGET {
        return full;
    }

    // Pass 2: clamp each description…
    let clamped: Vec<Skill> = visible
        .iter()
        .map(|skill| {
            let mut skill = skill.clone();
            skill.description = truncate_chars(&skill.description, SKILL_DESCRIPTION_BUDGET);
            skill
        })
        .collect();
    let clamped_block = format_skills_for_system_prompt(&clamped);
    if clamped_block.chars().count() <= SKILL_LISTING_BUDGET {
        return clamped_block;
    }

    // …pass 3: drop descriptions entirely.
    let stripped: Vec<Skill> = clamped
        .iter()
        .map(|skill| {
            let mut skill = skill.clone();
            skill.description = String::new();
            skill
        })
        .collect();
    let stripped_block = format_skills_for_system_prompt(&stripped);
    if stripped_block.chars().count() <= SKILL_LISTING_BUDGET {
        return stripped_block;
    }
    // …pass 4: keep only the leading skills that fit. Prefix search over
    // the upstream formatter so the size math never assumes its layout;
    // `fits(0)` is the empty block, so this always lands.
    let fits = |count: usize| {
        format_skills_for_system_prompt(&stripped[..count])
            .chars()
            .count()
            <= SKILL_LISTING_BUDGET
    };
    let mut low = 0;
    let mut high = stripped.len();
    while low < high {
        let mid = (low + high).div_ceil(2);
        if fits(mid) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    format_skills_for_system_prompt(&stripped[..low])
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(limit.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
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
                std::env::var_os("HOME")
                    .filter(|home| !home.is_empty())
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

    /// The roots for one chat, nearest first (ADR-0005 precedence).
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
        let env: Arc<dyn ExecutionEnv> = Arc::new(LocalExecutionEnv::new("/"));
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

    /// Resolve one inline mention (ADR-0035): the linked form's path is
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
            // declaration. No host-side argument substitution (ADR-0006).
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

/// `~` at the start of a mention path expands against `HOME`; anything else
/// passes through untouched.
fn expand_home(path: &str) -> String {
    path.strip_prefix("~/")
        .map(|rest| {
            std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
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
        // Content never enters the advertisement (ADR-0006).
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
        // that file — the path is authoritative (ADR-0035).
        let linked = holt_doc::SkillMention {
            name: "near".into(),
            path: Some(shadowed_file.clone()),
            range: 0..0,
        };
        let resolved = skills.resolve_mention(Some(&cwd), &linked).await.unwrap();
        assert_eq!(resolved.file_path, shadowed_file);
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
                if chip_file == &file
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

    fn skill_in(root: &Path, name: &str, frontmatter: &str, body: &str) -> String {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("SKILL.md");
        std::fs::write(&file, format!("---\n{frontmatter}---\n{body}")).unwrap();
        file.to_string_lossy().into_owned()
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
        assert_eq!(listing.skills[0].file, personal_file);
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
        assert_eq!(listing.skills[0].file, project_file);
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
