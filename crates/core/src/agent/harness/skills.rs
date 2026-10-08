//! Port of `pi-core/agent/src/harness/skills.ts`.
//!
//! Deviation: the TypeScript traversal matches ignore files through the
//! `ignore` npm package; the Rust port ships a small gitignore-style
//! matcher covering the pattern shapes the loader feeds it (anchored
//! relative paths, `*`/`**` wildcards, trailing-`/` directory patterns,
//! and `!` negation). The YAML frontmatter parses through `serde_yaml`,
//! matching the `yaml` package's parse failures as diagnostics.

use std::sync::Arc;

use crate::agent::harness::types::{ExecutionEnv, FileErrorCode, FileInfo, FileKind, Skill};

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;
const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];

/// Port of `SkillDiagnosticCode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillDiagnosticCode {
    FileInfoFailed,
    ListFailed,
    ReadFailed,
    ParseFailed,
    InvalidMetadata,
}

/// Port of `SkillDiagnostic`.
#[derive(Clone, Debug, PartialEq)]
pub struct SkillDiagnostic {
    pub code: SkillDiagnosticCode,
    pub message: String,
    pub path: String,
}

/// Result of [`load_skills`].
pub struct LoadedSkills {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

/// Format a skill invocation prompt, optionally appending additional user
/// instructions.
pub fn format_skill_invocation(skill: &Skill, additional_instructions: Option<&str>) -> String {
    let skill_block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name,
        skill.file_path,
        dirname_env_path(&skill.file_path),
        skill.content
    );
    match additional_instructions {
        Some(instructions) => format!("{skill_block}\n\n{instructions}"),
        None => skill_block,
    }
}

/// Load skills from one or more directories.
///
/// Traverses directories recursively, loads `SKILL.md` files, loads direct
/// root `.md` files with skill frontmatter, honors ignore files, and
/// returns diagnostics for invalid declared skill files. Missing input
/// directories are skipped.
pub async fn load_skills(env: &Arc<dyn ExecutionEnv>, dirs: &[String]) -> LoadedSkills {
    let mut skills: Vec<Skill> = Vec::new();
    let mut diagnostics: Vec<SkillDiagnostic> = Vec::new();
    for dir in dirs {
        let root_info = match env.file_info(dir, None).await {
            Ok(info) => info,
            Err(error) => {
                if error.code != FileErrorCode::NotFound {
                    diagnostics.push(SkillDiagnostic {
                        code: SkillDiagnosticCode::FileInfoFailed,
                        message: error.message,
                        path: dir.clone(),
                    });
                }
                continue;
            }
        };
        if resolve_kind(env, &root_info, &mut diagnostics).await != Some(FileKind::Directory) {
            continue;
        }
        let mut ignore = IgnoreMatcher::default();
        let result = Box::pin(load_skills_from_dir_internal(
            env,
            &root_info.path,
            true,
            &mut ignore,
            &root_info.path,
        ))
        .await;
        skills.extend(result.skills);
        diagnostics.extend(result.diagnostics);
    }
    LoadedSkills {
        skills,
        diagnostics,
    }
}

/// Load skills from source-tagged directories (port of
/// `loadSourcedSkills`); sources ride as opaque JSON tags.
pub async fn load_sourced_skills(
    env: &Arc<dyn ExecutionEnv>,
    inputs: &[(String, serde_json::Value)],
) -> (
    Vec<(Skill, serde_json::Value)>,
    Vec<(SkillDiagnostic, serde_json::Value)>,
) {
    let mut skills: Vec<(Skill, serde_json::Value)> = Vec::new();
    let mut diagnostics: Vec<(SkillDiagnostic, serde_json::Value)> = Vec::new();
    for (path, source) in inputs {
        let result = load_skills(env, std::slice::from_ref(path)).await;
        for skill in result.skills {
            skills.push((skill, source.clone()));
        }
        for diagnostic in result.diagnostics {
            diagnostics.push((diagnostic, source.clone()));
        }
    }
    (skills, diagnostics)
}

async fn load_skills_from_dir_internal(
    env: &Arc<dyn ExecutionEnv>,
    dir: &str,
    include_root_files: bool,
    ignore_matcher: &mut IgnoreMatcher,
    root_dir: &str,
) -> LoadedSkills {
    let mut skills: Vec<Skill> = Vec::new();
    let mut diagnostics: Vec<SkillDiagnostic> = Vec::new();

    let dir_info = match env.file_info(dir, None).await {
        Ok(info) => info,
        Err(error) => {
            if error.code != FileErrorCode::NotFound {
                diagnostics.push(SkillDiagnostic {
                    code: SkillDiagnosticCode::FileInfoFailed,
                    message: error.message,
                    path: dir.to_string(),
                });
            }
            return LoadedSkills {
                skills,
                diagnostics,
            };
        }
    };
    if resolve_kind(env, &dir_info, &mut diagnostics).await != Some(FileKind::Directory) {
        return LoadedSkills {
            skills,
            diagnostics,
        };
    }

    add_ignore_rules(env, ignore_matcher, dir, root_dir, &mut diagnostics).await;

    let mut entries = match env.list_dir(dir, None).await {
        Ok(entries) => entries,
        Err(error) => {
            diagnostics.push(SkillDiagnostic {
                code: SkillDiagnosticCode::ListFailed,
                message: error.message,
                path: dir.to_string(),
            });
            return LoadedSkills {
                skills,
                diagnostics,
            };
        }
    };

    for entry in &entries {
        if entry.name != "SKILL.md" {
            continue;
        }
        let kind = resolve_kind(env, entry, &mut diagnostics).await;
        if kind != Some(FileKind::File) {
            continue;
        }
        let rel_path = relative_env_path(root_dir, &entry.path);
        if ignore_matcher.ignores(&rel_path, false) {
            continue;
        }

        let result = load_skill_from_file(env, &entry.path, &dir_info.name).await;
        if let Some(skill) = result.skill {
            skills.push(skill);
        }
        diagnostics.extend(result.diagnostics);
        return LoadedSkills {
            skills,
            diagnostics,
        };
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in &entries {
        if entry.name.starts_with('.') || entry.name == "node_modules" {
            continue;
        }
        let kind = resolve_kind(env, entry, &mut diagnostics).await;
        let Some(kind) = kind else {
            continue;
        };

        let rel_path = relative_env_path(root_dir, &entry.path);
        let is_dir = kind == FileKind::Directory;
        let ignore_path = if is_dir {
            format!("{rel_path}/")
        } else {
            rel_path.clone()
        };
        if ignore_matcher.ignores(&ignore_path, is_dir) {
            continue;
        }

        if is_dir {
            let result = Box::pin(load_skills_from_dir_internal(
                env,
                &entry.path,
                false,
                ignore_matcher,
                root_dir,
            ))
            .await;
            skills.extend(result.skills);
            diagnostics.extend(result.diagnostics);
            continue;
        }

        if kind != FileKind::File || !include_root_files || !entry.name.ends_with(".md") {
            continue;
        }
        let result = load_skill_from_file(env, &entry.path, &dir_info.name).await;
        if let Some(skill) = result.skill {
            skills.push(skill);
        }
        diagnostics.extend(result.diagnostics);
    }

    LoadedSkills {
        skills,
        diagnostics,
    }
}

async fn add_ignore_rules(
    env: &Arc<dyn ExecutionEnv>,
    matcher: &mut IgnoreMatcher,
    dir: &str,
    root_dir: &str,
    diagnostics: &mut Vec<SkillDiagnostic>,
) {
    let relative_dir = relative_env_path(root_dir, dir);
    let prefix = if relative_dir.is_empty() {
        String::new()
    } else {
        format!("{relative_dir}/")
    };

    for filename in IGNORE_FILE_NAMES {
        let ignore_path = match env
            .join_path(&[dir.to_string(), filename.to_string()], None)
            .await
        {
            Ok(path) => path,
            Err(error) => {
                diagnostics.push(SkillDiagnostic {
                    code: SkillDiagnosticCode::FileInfoFailed,
                    message: error.message,
                    path: dir.to_string(),
                });
                continue;
            }
        };
        let info = match env.file_info(&ignore_path, None).await {
            Ok(info) => info,
            Err(error) => {
                if error.code != FileErrorCode::NotFound {
                    diagnostics.push(SkillDiagnostic {
                        code: SkillDiagnosticCode::FileInfoFailed,
                        message: error.message,
                        path: ignore_path,
                    });
                }
                continue;
            }
        };
        if info.kind != FileKind::File {
            continue;
        }
        let content = match env.read_text_file(&ignore_path, None).await {
            Ok(content) => content,
            Err(error) => {
                diagnostics.push(SkillDiagnostic {
                    code: SkillDiagnosticCode::ReadFailed,
                    message: error.message,
                    path: ignore_path,
                });
                continue;
            }
        };
        let patterns: Vec<String> = content
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .filter_map(|line| prefix_ignore_pattern(line, &prefix))
            .collect();
        if !patterns.is_empty() {
            matcher.add(&patterns);
        }
    }
}

fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('#') && !trimmed.starts_with("\\#") {
        return None;
    }

    let mut pattern = line;
    let mut negated = false;
    if let Some(rest) = pattern.strip_prefix('!') {
        negated = true;
        pattern = rest;
    } else if let Some(rest) = pattern.strip_prefix("\\!") {
        pattern = rest;
    }
    if let Some(rest) = pattern.strip_prefix('/') {
        pattern = rest;
    }
    let prefixed = if prefix.is_empty() {
        pattern.to_string()
    } else {
        format!("{prefix}{pattern}")
    };
    Some(if negated {
        format!("!{prefixed}")
    } else {
        prefixed
    })
}

struct FileSkill {
    skill: Option<Skill>,
    diagnostics: Vec<SkillDiagnostic>,
}

async fn load_skill_from_file(
    env: &Arc<dyn ExecutionEnv>,
    file_path: &str,
    parent_dir_name: &str,
) -> FileSkill {
    let mut diagnostics: Vec<SkillDiagnostic> = Vec::new();
    let normalized = file_path.trim_end_matches(['/', '\\']);
    let is_declared_skill = normalized
        .rsplit(['/', '\\'])
        .next()
        .is_some_and(|segment| segment == "SKILL.md");
    let raw_content = match env.read_text_file(file_path, None).await {
        Ok(content) => content,
        Err(error) => {
            diagnostics.push(SkillDiagnostic {
                code: SkillDiagnosticCode::ReadFailed,
                message: error.message,
                path: file_path.to_string(),
            });
            return FileSkill {
                skill: None,
                diagnostics,
            };
        }
    };

    let (frontmatter, body) = match parse_frontmatter(&raw_content) {
        Ok(parsed) => parsed,
        Err(message) => {
            if is_declared_skill {
                diagnostics.push(SkillDiagnostic {
                    code: SkillDiagnosticCode::ParseFailed,
                    message,
                    path: file_path.to_string(),
                });
            }
            return FileSkill {
                skill: None,
                diagnostics,
            };
        }
    };

    let description = frontmatter
        .as_mapping()
        .and_then(|map| map.get(serde_yaml::Value::String("description".to_string())))
        .and_then(|value| value.as_str())
        .map(str::to_string);
    if !is_declared_skill
        && description
            .as_deref()
            .is_none_or(|description| description.trim().is_empty())
    {
        return FileSkill {
            skill: None,
            diagnostics,
        };
    }

    for error in validate_description(description.as_deref()) {
        diagnostics.push(SkillDiagnostic {
            code: SkillDiagnosticCode::InvalidMetadata,
            message: error,
            path: file_path.to_string(),
        });
    }

    let frontmatter_name = frontmatter
        .as_mapping()
        .and_then(|map| map.get(serde_yaml::Value::String("name".to_string())))
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let name = frontmatter_name.unwrap_or_else(|| parent_dir_name.to_string());
    for error in validate_name(&name, parent_dir_name) {
        diagnostics.push(SkillDiagnostic {
            code: SkillDiagnosticCode::InvalidMetadata,
            message: error,
            path: file_path.to_string(),
        });
    }

    let Some(description) = description else {
        return FileSkill {
            skill: None,
            diagnostics,
        };
    };
    if description.trim().is_empty() {
        return FileSkill {
            skill: None,
            diagnostics,
        };
    }

    let disable_model_invocation = frontmatter.as_mapping().and_then(|map| {
        map.get(serde_yaml::Value::String(
            "disable-model-invocation".to_string(),
        ))
    }) == Some(&serde_yaml::Value::Bool(true));

    FileSkill {
        skill: Some(Skill {
            name,
            description,
            content: body,
            file_path: file_path.to_string(),
            disable_model_invocation: Some(disable_model_invocation),
        }),
        diagnostics,
    }
}

fn validate_name(name: &str, parent_dir_name: &str) -> Vec<String> {
    let mut errors: Vec<String> = Vec::new();
    if name != parent_dir_name {
        errors.push(format!(
            "name \"{name}\" does not match parent directory \"{parent_dir_name}\""
        ));
    }
    if name.chars().count() > MAX_NAME_LENGTH {
        errors.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({})",
            name.chars().count()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_string());
    }
    errors
}

fn validate_description(description: Option<&str>) -> Vec<String> {
    let mut errors: Vec<String> = Vec::new();
    match description {
        None => errors.push("description is required".to_string()),
        Some(description) if description.trim().is_empty() => {
            errors.push("description is required".to_string());
        }
        Some(description) if description.chars().count() > MAX_DESCRIPTION_LENGTH => {
            errors.push(format!(
                "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
                description.chars().count()
            ));
        }
        _ => {}
    }
    errors
}

fn parse_frontmatter(content: &str) -> Result<(serde_yaml::Value, String), String> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return Ok((serde_yaml::Value::Null, normalized));
    }
    let Some(end_index) = normalized[3..].find("\n---").map(|offset| offset + 3) else {
        return Ok((serde_yaml::Value::Null, normalized));
    };
    let yaml_string = &normalized[4..end_index];
    let body = normalized[end_index + 4..].trim().to_string();
    let frontmatter = serde_yaml::from_str::<serde_yaml::Value>(yaml_string)
        .map_err(|error| error.to_string())?;
    Ok((frontmatter, body))
}

async fn resolve_kind(
    env: &Arc<dyn ExecutionEnv>,
    info: &FileInfo,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> Option<FileKind> {
    if info.kind == FileKind::File || info.kind == FileKind::Directory {
        return Some(info.kind);
    }
    let canonical_path = match env.canonical_path(&info.path, None).await {
        Ok(path) => path,
        Err(error) => {
            if error.code != FileErrorCode::NotFound {
                diagnostics.push(SkillDiagnostic {
                    code: SkillDiagnosticCode::FileInfoFailed,
                    message: error.message,
                    path: info.path.clone(),
                });
            }
            return None;
        }
    };
    match env.file_info(&canonical_path, None).await {
        Ok(target) => {
            if target.kind == FileKind::File || target.kind == FileKind::Directory {
                Some(target.kind)
            } else {
                None
            }
        }
        Err(error) => {
            if error.code != FileErrorCode::NotFound {
                diagnostics.push(SkillDiagnostic {
                    code: SkillDiagnosticCode::FileInfoFailed,
                    message: error.message,
                    path: info.path.clone(),
                });
            }
            None
        }
    }
}

fn dirname_env_path(path: &str) -> String {
    let normalized = path.trim_end_matches(['/', '\\']);
    let forward = normalized.rfind('/');
    let backward = normalized.rfind('\\');
    let separator_index = match (forward, backward) {
        (Some(a), Some(b)) => a.max(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => 0,
    };
    if separator_index == 2 && normalized.as_bytes().get(1) == Some(&b':') {
        return normalized[..3].to_string();
    }
    if separator_index == 0 {
        return "/".to_string();
    }
    normalized[..separator_index].to_string()
}

fn relative_env_path(root: &str, path: &str) -> String {
    let normalized_root = root.replace('\\', "/").trim_end_matches('/').to_string();
    let normalized_path = path.replace('\\', "/").trim_end_matches('/').to_string();
    if normalized_path == normalized_root {
        return String::new();
    }
    if normalized_path.starts_with(&format!("{normalized_root}/")) {
        normalized_path[normalized_root.len() + 1..].to_string()
    } else {
        normalized_path.trim_start_matches('/').to_string()
    }
}

/// A minimal gitignore-style matcher (see the module docs).
#[derive(Default)]
struct IgnoreMatcher {
    patterns: Vec<IgnorePattern>,
}

struct IgnorePattern {
    negated: bool,
    directory_only: bool,
    segments: Vec<String>,
}

impl IgnoreMatcher {
    fn add(&mut self, patterns: &[String]) {
        for pattern in patterns {
            if let Some(compiled) = compile_pattern(pattern) {
                self.patterns.push(compiled);
            }
        }
    }

    /// Last matching pattern wins, like gitignore.
    fn ignores(&self, path: &str, is_dir: bool) -> bool {
        let mut ignored = false;
        for pattern in &self.patterns {
            if pattern.directory_only && !is_dir {
                continue;
            }
            if pattern_matches(&pattern.segments, path) {
                ignored = !pattern.negated;
            }
        }
        ignored
    }
}

fn compile_pattern(pattern: &str) -> Option<IgnorePattern> {
    let mut pattern = pattern;
    let mut negated = false;
    if let Some(rest) = pattern.strip_prefix('!') {
        negated = true;
        pattern = rest;
    }
    if pattern.is_empty() {
        return None;
    }
    let directory_only = pattern.ends_with('/');
    let trimmed = pattern.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    Some(IgnorePattern {
        negated,
        directory_only,
        segments: trimmed.split('/').map(str::to_string).collect(),
    })
}

fn pattern_matches(segments: &[String], path: &str) -> bool {
    let path_segments: Vec<&str> = path.split('/').collect();
    match_segments(segments, &path_segments)
}

fn match_segments(pattern: &[String], path: &[&str]) -> bool {
    if pattern.is_empty() {
        return true;
    }
    let segment = &pattern[0];
    if segment == "**" {
        for skip in 0..=path.len() {
            if match_segments(&pattern[1..], &path[skip..]) {
                return true;
            }
        }
        return false;
    }
    if path.is_empty() {
        return false;
    }
    (wildcard_match(segment, path[0]) || path[0] == segment)
        && match_segments(&pattern[1..], &path[1..])
}

/// `*` wildcard match within one segment (no `/` crossing).
fn wildcard_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return false;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut position = 0;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let Some(found) = text[position..].find(part) else {
            return false;
        };
        if index == 0 && found != 0 {
            return false;
        }
        position += found + part.len();
    }
    if let Some(last) = parts.last()
        && !last.is_empty()
        && !text.ends_with(last)
    {
        return false;
    }
    true
}
