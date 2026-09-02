//! The skills capability (ADR-0005/0006): root resolution, catalog
//! assembly, precedence/shadowing, and invocation-prompt building over the
//! upstream agent-loop loader. Skills are referenced in place — nothing is
//! copied, moved, or persisted; the filesystem is the registry, and every
//! catalog use rescans, so there is no cache and no invalidation problem.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use holt_proto::{InvalidSkillEntry, ShadowedSkillEntry, SkillEntry, SkillListing, SkillRoot};
use pi_core::agent::harness::skills::{LoadedSkills, SkillDiagnostic, load_skills};
use pi_core::agent::harness::types::{ExecutionEnv, Skill};

use crate::tools::LocalExecutionEnv;

/// The loader's per-root scan, kept as a pair so precedence can group by
/// origin before anything crosses the RPC seam.
struct RootScan {
    root: SkillRoot,
    skills: Vec<Skill>,
    diagnostics: Vec<SkillDiagnostic>,
}

/// A full catalog scan: the precedence winners plus everything the Settings
/// page explains — shadowed losers and load diagnostics.
pub(crate) struct Catalog {
    /// Valid, unshadowed skills in root-precedence order — the invocable
    /// set the system-prompt block, `/` menu, and invocations resolve
    /// against.
    pub(crate) winners: Vec<(Skill, SkillRoot)>,
    shadowed: Vec<ShadowedSkillEntry>,
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
            shadowed: self.shadowed.clone(),
            invalid: self.invalid.clone(),
        }
    }
}

/// The engine's fixed skill roots: the project root derives from each
/// chat's cwd, the other two are resolved once at assembly.
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
}

/// Group the per-root scans into the catalog: a skill the loader flagged
/// (any diagnostic naming its file) is invalid, not invocable; diagnostics
/// that match no loaded skill (parse failures, traversal faults) surface on
/// their own so the Settings page can show them.
fn assemble_catalog(scans: Vec<RootScan>) -> Catalog {
    let mut winners: Vec<(Skill, SkillRoot)> = Vec::new();
    let mut shadowed: Vec<ShadowedSkillEntry> = Vec::new();
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
            // shadowed by that winner.
            match winners
                .iter()
                .find(|(won, _)| won.name == skill.name)
                .map(|(_, won_root)| *won_root)
            {
                Some(won_root) => shadowed.push(ShadowedSkillEntry {
                    name: skill.name.clone(),
                    file: skill.file_path.clone(),
                    root,
                    shadowed_by: won_root,
                }),
                None => winners.push((skill, root)),
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
