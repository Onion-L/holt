//! Port of `pi-core/agent/src/harness/prompt-templates.ts`.
//!
//! Deviation: the TypeScript frontmatter parser uses the `yaml` package;
//! the Rust port parses YAML with `serde_yaml` (same input shapes; parse
//! failures surface identically as `parse_failed` diagnostics).

use std::sync::Arc;

use crate::agent::harness::types::{ExecutionEnv, FileErrorCode, FileInfo, PromptTemplate};

/// Port of `PromptTemplateDiagnosticCode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptTemplateDiagnosticCode {
    FileInfoFailed,
    ListFailed,
    ReadFailed,
    ParseFailed,
}

/// Port of `PromptTemplateDiagnostic`.
#[derive(Clone, Debug, PartialEq)]
pub struct PromptTemplateDiagnostic {
    pub code: PromptTemplateDiagnosticCode,
    pub message: String,
    pub path: String,
}

/// Result of [`load_prompt_templates`].
pub struct LoadedPromptTemplates {
    pub prompt_templates: Vec<PromptTemplate>,
    pub diagnostics: Vec<PromptTemplateDiagnostic>,
}

/// Load prompt templates from one or more paths.
///
/// Directory inputs load direct `.md` children non-recursively. File
/// inputs load explicit `.md` files. Missing paths and non-markdown files
/// are skipped. Read and parse failures are returned as diagnostics.
pub async fn load_prompt_templates(
    env: &Arc<dyn ExecutionEnv>,
    paths: &[String],
) -> LoadedPromptTemplates {
    let mut prompt_templates: Vec<PromptTemplate> = Vec::new();
    let mut diagnostics: Vec<PromptTemplateDiagnostic> = Vec::new();
    for path in paths {
        let info = match env.file_info(path, None).await {
            Ok(info) => info,
            Err(error) => {
                if error.code != FileErrorCode::NotFound {
                    diagnostics.push(PromptTemplateDiagnostic {
                        code: PromptTemplateDiagnosticCode::FileInfoFailed,
                        message: error.message,
                        path: path.clone(),
                    });
                }
                continue;
            }
        };
        let kind = resolve_kind(env, &info, &mut diagnostics).await;
        match kind {
            Some(FileKind::Directory) => {
                let result = load_templates_from_dir(env, &info.path).await;
                prompt_templates.extend(result.prompt_templates);
                diagnostics.extend(result.diagnostics);
            }
            Some(FileKind::File) if info.name.ends_with(".md") => {
                let result = load_template_from_file(env, &info.path, &info.name).await;
                if let Some(template) = result.prompt_template {
                    prompt_templates.push(template);
                }
                diagnostics.extend(result.diagnostics);
            }
            _ => {}
        }
    }
    LoadedPromptTemplates {
        prompt_templates,
        diagnostics,
    }
}

use crate::agent::harness::types::FileKind;

async fn load_templates_from_dir(env: &Arc<dyn ExecutionEnv>, dir: &str) -> LoadedPromptTemplates {
    let mut prompt_templates: Vec<PromptTemplate> = Vec::new();
    let mut diagnostics: Vec<PromptTemplateDiagnostic> = Vec::new();
    let mut entries = match env.list_dir(dir, None).await {
        Ok(entries) => entries,
        Err(error) => {
            diagnostics.push(PromptTemplateDiagnostic {
                code: PromptTemplateDiagnosticCode::ListFailed,
                message: error.message,
                path: dir.to_string(),
            });
            return LoadedPromptTemplates {
                prompt_templates,
                diagnostics,
            };
        }
    };
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    for entry in entries {
        let kind = resolve_kind(env, &entry, &mut diagnostics).await;
        if kind != Some(FileKind::File) || !entry.name.ends_with(".md") {
            continue;
        }
        let result = load_template_from_file(env, &entry.path, &entry.name).await;
        if let Some(template) = result.prompt_template {
            prompt_templates.push(template);
        }
        diagnostics.extend(result.diagnostics);
    }
    LoadedPromptTemplates {
        prompt_templates,
        diagnostics,
    }
}

struct FileTemplate {
    prompt_template: Option<PromptTemplate>,
    diagnostics: Vec<PromptTemplateDiagnostic>,
}

async fn load_template_from_file(
    env: &Arc<dyn ExecutionEnv>,
    file_path: &str,
    file_name: &str,
) -> FileTemplate {
    let raw_content = match env.read_text_file(file_path, None).await {
        Ok(content) => content,
        Err(error) => {
            return FileTemplate {
                prompt_template: None,
                diagnostics: vec![PromptTemplateDiagnostic {
                    code: PromptTemplateDiagnosticCode::ReadFailed,
                    message: error.message,
                    path: file_path.to_string(),
                }],
            };
        }
    };

    let (frontmatter, body) = match parse_frontmatter(&raw_content) {
        Ok(parsed) => parsed,
        Err(message) => {
            return FileTemplate {
                prompt_template: None,
                diagnostics: vec![PromptTemplateDiagnostic {
                    code: PromptTemplateDiagnosticCode::ParseFailed,
                    message,
                    path: file_path.to_string(),
                }],
            };
        }
    };

    let first_line = body
        .split('\n')
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let mut description = frontmatter
        .get("description")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    if description.is_empty() && !first_line.is_empty() {
        description = first_line.chars().take(60).collect();
        if first_line.chars().count() > 60 {
            description.push_str("...");
        }
    }
    FileTemplate {
        prompt_template: Some(PromptTemplate {
            name: file_name
                .strip_suffix(".md")
                .or_else(|| file_name.strip_suffix(".MD"))
                .unwrap_or(file_name)
                .to_string(),
            description: if description.is_empty() {
                None
            } else {
                Some(description)
            },
            content: body,
        }),
        diagnostics: Vec::new(),
    }
}

async fn resolve_kind(
    env: &Arc<dyn ExecutionEnv>,
    info: &FileInfo,
    diagnostics: &mut Vec<PromptTemplateDiagnostic>,
) -> Option<FileKind> {
    if info.kind == FileKind::File || info.kind == FileKind::Directory {
        return Some(info.kind);
    }
    let canonical_path = match env.canonical_path(&info.path, None).await {
        Ok(path) => path,
        Err(error) => {
            if error.code != FileErrorCode::NotFound {
                diagnostics.push(PromptTemplateDiagnostic {
                    code: PromptTemplateDiagnosticCode::FileInfoFailed,
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
                diagnostics.push(PromptTemplateDiagnostic {
                    code: PromptTemplateDiagnosticCode::FileInfoFailed,
                    message: error.message,
                    path: info.path.clone(),
                });
            }
            None
        }
    }
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

/// Parse an argument string using simple shell-style single and double
/// quotes.
pub fn parse_command_args(args_string: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;

    for character in args_string.chars() {
        if let Some(quote) = in_quote {
            if character == quote {
                in_quote = None;
            } else {
                current.push(character);
            }
        } else if character == '"' || character == '\'' {
            in_quote = Some(character);
        } else if character == ' ' || character == '\t' {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(character);
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

fn replace_numbered(content: &str, args: &[String]) -> String {
    let mut result = String::with_capacity(content.len());
    let chars: Vec<char> = content.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '$' && chars.get(index + 1).is_some_and(|c| c.is_ascii_digit()) {
            let mut number_end = index + 1;
            while number_end < chars.len() && chars[number_end].is_ascii_digit() {
                number_end += 1;
            }
            let number: usize = chars[index + 1..number_end]
                .iter()
                .collect::<String>()
                .parse()
                .unwrap_or_default();
            let replacement = args
                .get(number.saturating_sub(1))
                .cloned()
                .unwrap_or_default();
            result.push_str(&replacement);
            index = number_end;
            continue;
        }
        result.push(chars[index]);
        index += 1;
    }
    result
}

fn replace_slice_tokens(content: &str, args: &[String]) -> String {
    let mut result = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find("${@:") {
        result.push_str(&rest[..start]);
        let after = &rest[start + 4..];
        let Some(close) = after.find('}') else {
            result.push_str(&rest[start..]);
            return result;
        };
        let token = &after[..close];
        let (start_str, length_str) = match token.split_once(':') {
            Some((start, length)) => (start, Some(length)),
            None => (token, None),
        };
        let start = start_str.parse::<usize>().unwrap_or(1).saturating_sub(1);
        let replacement = match length_str {
            Some(length) => {
                let length: usize = length.parse().unwrap_or_default();
                args.iter()
                    .skip(start)
                    .take(length)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            None => args
                .iter()
                .skip(start)
                .cloned()
                .collect::<Vec<_>>()
                .join(" "),
        };
        result.push_str(&replacement);
        rest = &after[close + 1..];
    }
    result.push_str(rest);
    result
}

fn replace_all_occurrences(content: &str, marker: &str, replacement: &str) -> String {
    content.replace(marker, replacement)
}

/// Substitute prompt template placeholders (`$1`, `$@`, `$ARGUMENTS`,
/// `${@:N}`, `${@:N:L}`) with command arguments.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let mut result = replace_numbered(content, args);
    result = replace_slice_tokens(&result, args);
    let all_args = args.join(" ");
    result = replace_all_occurrences(&result, "$ARGUMENTS", &all_args);
    result = replace_all_occurrences(&result, "$@", &all_args);
    result
}

/// Format a prompt template invocation with positional arguments.
pub fn format_prompt_template_invocation(template: &PromptTemplate, args: &[String]) -> String {
    substitute_args(&template.content, args)
}

/// A sourced prompt-template pair (the `loadSourcedPromptTemplates` output).
pub struct SourcedPromptTemplate<TSource> {
    pub prompt_template: PromptTemplate,
    pub source: TSource,
}

/// A sourced prompt-template diagnostic.
pub struct SourcedPromptTemplateDiagnostic<TSource> {
    pub diagnostic: PromptTemplateDiagnostic,
    pub source: TSource,
}

/// Port of `loadSourcedPromptTemplates`: loads each input path and pairs the
/// templates and diagnostics with the input's source.
pub async fn load_sourced_prompt_templates<TSource: Clone>(
    env: &Arc<dyn ExecutionEnv>,
    inputs: &[SourcedInput<TSource>],
) -> (
    Vec<SourcedPromptTemplate<TSource>>,
    Vec<SourcedPromptTemplateDiagnostic<TSource>>,
) {
    let mut prompt_templates = Vec::new();
    let mut diagnostics = Vec::new();
    for input in inputs {
        let result = load_prompt_templates(env, std::slice::from_ref(&input.path)).await;
        for prompt_template in result.prompt_templates {
            prompt_templates.push(SourcedPromptTemplate {
                prompt_template,
                source: input.source.clone(),
            });
        }
        for diagnostic in result.diagnostics {
            diagnostics.push(SourcedPromptTemplateDiagnostic {
                diagnostic,
                source: input.source.clone(),
            });
        }
    }
    (prompt_templates, diagnostics)
}

/// One sourced loader input (`{ path, source }`).
pub struct SourcedInput<TSource> {
    pub path: String,
    pub source: TSource,
}
