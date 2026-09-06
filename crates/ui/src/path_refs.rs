//! Path references: files and folders attached through the picker, drag and
//! drop, or paste, sent to the agent as plain prompt text. A reference points
//! at live filesystem contents — attaching never uploads, copies, snapshots,
//! or eagerly reads the target, and attaching a folder never expands it into
//! references to its descendants.

use std::path::{Path, PathBuf};

/// One attachment-area chip: a file or folder the user pointed at, bound to
/// its absolute target at selection time (spec: binding happens on selection,
/// never re-resolved against a later Space at send time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRef {
    pub id: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

impl PathRef {
    /// Chip label: the basename (short name on screen; the full target is the
    /// chip's hover text).
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.to_string_lossy().into_owned())
    }

    /// The complete target path as hover text (trailing `/` marks folders,
    /// matching the mention-link convention).
    pub fn full_path(&self) -> String {
        let mut path = self.path.to_string_lossy().into_owned();
        if self.is_dir && !path.ends_with('/') {
            path.push('/');
        }
        path
    }
}

/// Bind a picked or dropped path to its live target. Canonicalizing makes the
/// reference absolute and catches a selection that no longer resolves; the
/// caller reports the failure and keeps the selection's other paths.
pub fn bind(path: &Path) -> Result<PathRef, String> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|err| format!("Couldn't attach {}: {err}", path.display()))?;
    let is_dir = canonical.is_dir();
    Ok(PathRef {
        id: uuid::Uuid::new_v4().to_string(),
        path: canonical,
        is_dir,
    })
}

/// Attachment-area dedup: the same target attached twice stays one reference.
/// A folder and a file inside it are different paths and coexist.
/// Returns true when the reference was added.
pub fn push_unique(refs: &mut Vec<PathRef>, reference: PathRef) -> bool {
    if refs.iter().any(|existing| existing.path == reference.path) {
        return false;
    }
    refs.push(reference);
    true
}

/// The prompt-text form of one reference: the absolute path double-quoted,
/// with backslash escapes for the characters a line-oriented list cannot
/// carry raw. One consistent rule covers spaces, Unicode, quotes, Markdown
/// punctuation, and newlines without changing the target; folders keep their
/// trailing `/` inside the quotes.
pub fn format_reference(path: &str, is_dir: bool) -> String {
    let mut out = String::with_capacity(path.len() + 3);
    out.push('"');
    for ch in path.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                out.push_str(&format!("\\u{{{:x}}}", c as u32));
            }
            c => out.push(c),
        }
    }
    if is_dir && !path.ends_with('/') {
        out.push('/');
    }
    out.push('"');
    out
}

/// The header line introducing the appended path list.
pub const REFS_HEADER: &str = "Referenced paths:";

/// Append the explicit path list to the message body (spec: one list at the
/// end of the prompt, editable as ordinary text in the queue afterwards).
/// An empty body with references stays instruction-free — a references-only
/// send must not invent a task.
pub fn append_references(text: &str, refs: &[PathRef]) -> String {
    if refs.is_empty() {
        return text.to_string();
    }
    let mut out = text.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(REFS_HEADER);
    for reference in refs {
        out.push_str("\n- ");
        out.push_str(&format_reference(
            &reference.path.to_string_lossy(),
            reference.is_dir,
        ));
    }
    out
}

/// `append_references` for an optional body (skill extra instructions).
pub fn append_references_opt(text: Option<&str>, refs: &[PathRef]) -> Option<String> {
    if refs.is_empty() {
        return text.map(str::to_string);
    }
    Some(append_references(text.unwrap_or(""), refs))
}

/// Expand a leading `~` to the user's home directory (a chat's cwd may carry
/// it; the engine's `expand_tilde` is the backend twin — the UI cannot link
/// it across the RPC boundary).
pub fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) => std::env::home_dir()
            .unwrap_or_default()
            .join(rest.strip_prefix('/').unwrap_or(rest)),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(path: &str, is_dir: bool) -> PathRef {
        PathRef {
            id: path.to_string(),
            path: PathBuf::from(path),
            is_dir,
        }
    }

    #[test]
    fn reference_formatting_quotes_every_path() {
        assert_eq!(format_reference("/abs/a.rs", false), "\"/abs/a.rs\"");
        assert_eq!(format_reference("/abs/dir", true), "\"/abs/dir/\"");
        assert_eq!(
            format_reference("/abs/with space/ünïcode.rs", false),
            "\"/abs/with space/ünïcode.rs\""
        );
    }

    #[test]
    fn reference_formatting_escapes_what_a_line_cannot_carry() {
        assert_eq!(
            format_reference("/abs/say \"hi\".rs", false),
            "\"/abs/say \\\"hi\\\".rs\""
        );
        assert_eq!(
            format_reference("/abs/back\\slash.rs", false),
            "\"/abs/back\\\\slash.rs\""
        );
        assert_eq!(
            format_reference("/abs/line\nbreak.rs", false),
            "\"/abs/line\\nbreak.rs\""
        );
        // Markdown punctuation passes through raw — the quoting protects it.
        assert_eq!(
            format_reference("/abs/[link](x).md", false),
            "\"/abs/[link](x).md\""
        );
    }

    #[test]
    fn the_list_appends_once_at_the_end() {
        let refs = vec![reference("/abs/a.rs", false), reference("/abs/dir", true)];
        assert_eq!(
            append_references("look at these", &refs),
            "look at these\n\nReferenced paths:\n- \"/abs/a.rs\"\n- \"/abs/dir/\""
        );
    }

    #[test]
    fn a_references_only_send_gets_no_invented_instruction() {
        let refs = vec![reference("/abs/a.rs", false)];
        assert_eq!(
            append_references("", &refs),
            "Referenced paths:\n- \"/abs/a.rs\""
        );
        assert_eq!(
            append_references("   ", &refs),
            "Referenced paths:\n- \"/abs/a.rs\""
        );
    }

    #[test]
    fn no_references_leave_the_body_untouched() {
        assert_eq!(append_references("plain", &[]), "plain");
        assert_eq!(append_references_opt(None, &[]), None);
        assert_eq!(
            append_references_opt(Some("extra"), &[reference("/a", false)]),
            Some("extra\n\nReferenced paths:\n- \"/a\"".to_string())
        );
        assert_eq!(
            append_references_opt(None, &[reference("/a", false)]),
            Some("Referenced paths:\n- \"/a\"".to_string())
        );
    }

    #[test]
    fn the_attachment_area_deduplicates_by_path() {
        let mut refs = vec![reference("/abs/dir", true)];
        assert!(!push_unique(&mut refs, reference("/abs/dir", true)));
        // A folder and a file inside it coexist.
        assert!(push_unique(&mut refs, reference("/abs/dir/file.rs", false)));
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn chip_names_are_basenames_and_hover_shows_the_full_target() {
        let file = reference("/abs/dir/file.rs", false);
        assert_eq!(file.name(), "file.rs");
        assert_eq!(file.full_path(), "/abs/dir/file.rs");
        let dir = reference("/abs/dir", true);
        assert_eq!(dir.name(), "dir");
        assert_eq!(dir.full_path(), "/abs/dir/");
    }

    #[test]
    fn tilde_roots_expand_against_home() {
        let home = std::env::home_dir().unwrap();
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/work/repo"), home.join("work/repo"));
        assert_eq!(expand_home("/abs/path"), PathBuf::from("/abs/path"));
        assert_eq!(expand_home("relative/path"), PathBuf::from("relative/path"));
    }

    #[test]
    fn binding_resolves_a_live_target_and_reports_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a file.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let bound = bind(&file).unwrap();
        assert!(bound.path.is_absolute());
        assert!(!bound.is_dir);
        let bound_dir = bind(dir.path()).unwrap();
        assert!(bound_dir.is_dir);
        let missing = bind(&dir.path().join("nope.rs"));
        assert!(missing.is_err());
    }
}
