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
    /// True for a Managed image the engine saved from pasted bytes — the
    /// only kind of reference whose file Holt may reclaim when the chip is
    /// removed and no durable store still needs it.
    pub managed: bool,
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
        managed: false,
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

// ---------------------------------------------------------------------------
// Transcript projection: sent messages render path references as the same
// `@name` chips the composer showed. Display-only — the raw text (what the
// queue editor and the model see) is untouched.
// ---------------------------------------------------------------------------

use std::ops::Range;

use gpui::SharedString;

use crate::composer::SentMentionSpan;

/// One quoted absolute path found in sent text: its raw byte range (quotes
/// included) and the unescaped target.
fn quoted_paths(text: &str) -> Vec<(Range<usize>, String)> {
    let mut paths = Vec::new();
    let mut at = 0;
    while let Some(relative) = text[at..].find('"') {
        let start = at + relative;
        let mut cursor = start + 1;
        let mut path = String::new();
        let mut close = None;
        while cursor < text.len() {
            let ch = text[cursor..].chars().next().expect("cursor at boundary");
            match ch {
                '"' => {
                    close = Some(cursor);
                    break;
                }
                // A reference never spans a raw newline — its control
                // characters are escaped. Anything else is not our format.
                '\n' | '\r' => break,
                '\\' => {
                    let rest = &text[cursor + 1..];
                    let mut chars = rest.chars();
                    match chars.next() {
                        Some('"') => path.push('"'),
                        Some('\\') => path.push('\\'),
                        Some('n') => path.push('\n'),
                        Some('r') => path.push('\r'),
                        Some('t') => path.push('\t'),
                        Some('u') => {
                            // The `\u{XX}` control-char escape.
                            let Some(end) = rest.find('}') else { break };
                            let Some(hex) = rest[..end].strip_prefix('{') else {
                                break;
                            };
                            let Ok(code) = u32::from_str_radix(hex, 16) else {
                                break;
                            };
                            let Some(c) = char::from_u32(code) else { break };
                            path.push(c);
                            cursor += 2 + end + 1;
                            continue;
                        }
                        // Unknown escape: not our format.
                        _ => break,
                    }
                    cursor += 2;
                }
                c => {
                    path.push(c);
                    cursor += c.len_utf8();
                }
            }
        }
        match close {
            Some(end) => {
                if path.starts_with('/') {
                    paths.push((start..end + 1, path));
                }
                at = end + 1;
            }
            None => at = start + 1,
        }
    }
    paths
}

/// Chip label for an absolute target: the basename, with a trailing `/` for
/// folders (text chips have no icon to carry that distinction).
fn chip_label(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().filter(|name| !name.is_empty())?;
    Some(if path.ends_with('/') {
        format!("{name}/")
    } else {
        name.to_string()
    })
}

/// One reference lifted out of a sent message's appended list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentReference {
    /// Chip label: the basename, with a trailing `/` for folders.
    pub label: String,
    /// The full absolute target (hover text).
    pub path: String,
    pub is_dir: bool,
}

/// Lift the appended path list OUT of a sent message: the trailer rides the
/// prompt for the model, but the bubble shows only the user's own text — the
/// transcript renders the references as a separate attachment row instead.
/// Returns the text unchanged when no well-formed trailer ends it.
pub fn split_sent_references(text: &str) -> (String, Vec<SentReference>) {
    let none = || (text.to_string(), Vec::new());
    let Some(header_at) = text.rfind(REFS_HEADER) else {
        return none();
    };
    // The header must start a line, and everything after it must be
    // `- "..."` items — anything else is user text that happens to mention
    // the header and stays put.
    if header_at > 0 && !text[..header_at].ends_with('\n') {
        return none();
    }
    let Some(trailer) = text[header_at + REFS_HEADER.len()..].strip_prefix('\n') else {
        return none();
    };
    let mut refs = Vec::new();
    for line in trailer.lines() {
        let Some(item) = line.strip_prefix("- ") else {
            return none();
        };
        let quoted = quoted_paths(item);
        let [(range, path)] = quoted.as_slice() else {
            return none();
        };
        if range.start != 0 || range.end != item.len() {
            return none();
        }
        let Some(label) = chip_label(path) else {
            return none();
        };
        refs.push(SentReference {
            label,
            path: path.clone(),
            is_dir: path.ends_with('/'),
        });
    }
    if refs.is_empty() {
        return none();
    }
    (text[..header_at].trim_end().to_string(), refs)
}

/// Project a sent message's path references for the transcript: every quoted
/// absolute path (`format_reference`'s output — inline mentions and the
/// appended list alike) collapses to the composer's `@name` chip, everything
/// else passes through. `None` when the text carries no reference, keeping
/// ordinary prompts on the zero-allocation path.
pub fn sent_reference_display(raw: &str) -> Option<(String, Vec<SentMentionSpan>)> {
    if !raw.contains("\"/") {
        return None;
    }
    let paths: Vec<_> = quoted_paths(raw)
        .into_iter()
        .filter_map(|(range, path)| chip_label(&path).map(|label| (range, path, label)))
        .collect();
    if paths.is_empty() {
        return None;
    }
    let mut display = String::with_capacity(raw.len());
    let mut spans = Vec::with_capacity(paths.len());
    let mut at = 0;
    for (range, path, label) in paths {
        display.push_str(&raw[at..range.start]);
        let start = display.len();
        display.push('\u{00A0}');
        display.push('@');
        for ch in label.chars() {
            display.push(if ch == ' ' || ch.is_control() {
                '\u{00A0}'
            } else {
                ch
            });
        }
        display.push('\u{00A0}');
        spans.push(SentMentionSpan {
            range: start..display.len(),
            path: SharedString::from(path.clone()),
            is_dir: path.ends_with('/'),
        });
        at = range.end;
    }
    display.push_str(&raw[at..]);
    Some((display, spans))
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
            managed: false,
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
    fn sent_display_chips_inline_paths_like_the_composer() {
        let raw = "open \"/abs/space/src/a file.rs\" and \"/abs/space/assets/\" now";
        let (display, spans) = sent_reference_display(raw).expect("references project");
        // Spaces in the label shape as NBSPs, exactly like the composer's
        // mention projection.
        assert_eq!(
            display,
            "open \u{00A0}@a\u{00A0}file.rs\u{00A0} and \u{00A0}@assets/\u{00A0} now"
        );
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].path.as_ref(), "/abs/space/src/a file.rs");
        assert!(!spans[0].is_dir);
        assert_eq!(spans[1].path.as_ref(), "/abs/space/assets/");
        assert!(spans[1].is_dir);
        assert_eq!(
            &display[spans[0].range.clone()],
            "\u{00A0}@a\u{00A0}file.rs\u{00A0}"
        );
    }

    #[test]
    fn sent_display_chips_the_appended_list_and_repeats() {
        let text = append_references(
            "look at \"/abs/a.rs\" twice \"/abs/a.rs\"",
            &[reference("/abs/dir", true), reference("/abs/b.rs", false)],
        );
        let (display, spans) = sent_reference_display(&text).expect("references project");
        assert!(display.contains(REFS_HEADER), "{display}");
        assert!(display.contains("- \u{00A0}@dir/\u{00A0}"), "{display}");
        assert!(display.contains("- \u{00A0}@b.rs\u{00A0}"), "{display}");
        assert_eq!(spans.len(), 4, "inline repeats each project: {display}");
        assert!(!display.contains('"'), "no raw quotes remain: {display}");
    }

    #[test]
    fn sent_display_unescapes_what_formatting_escaped() {
        let raw = "check \"/abs/say \\\"hi\\\".rs\" and \"/abs/line\\nbreak.rs\"";
        let (display, spans) = sent_reference_display(raw).expect("references project");
        assert_eq!(spans[0].path.as_ref(), "/abs/say \"hi\".rs");
        assert_eq!(spans[1].path.as_ref(), "/abs/line\nbreak.rs");
        assert!(!display.contains('\\'), "{display}");
    }

    #[test]
    fn sent_display_leaves_ordinary_quotes_and_text_alone() {
        assert_eq!(sent_reference_display("just a prompt"), None);
        assert_eq!(sent_reference_display("say \"hello\" loudly"), None);
        assert_eq!(sent_reference_display("quote \"relative/path\" no"), None);
        assert_eq!(sent_reference_display("unterminated \"/abs/path"), None);
        // A bare root has no basename to label.
        assert_eq!(sent_reference_display("root is \"/\" here"), None);
    }

    #[test]
    fn split_lifts_the_trailer_off_the_users_own_text() {
        let text = append_references(
            "look at this",
            &[
                reference("/abs/lvdao-logo", true),
                reference("/abs/a.rs", false),
            ],
        );
        let (body, refs) = split_sent_references(&text);
        assert_eq!(body, "look at this");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].label, "lvdao-logo/");
        assert_eq!(refs[0].path, "/abs/lvdao-logo/");
        assert!(refs[0].is_dir);
        assert_eq!(refs[1].label, "a.rs");
        assert!(!refs[1].is_dir);
    }

    #[test]
    fn split_handles_a_references_only_message() {
        let text = append_references("", &[reference("/abs/a.rs", false)]);
        let (body, refs) = split_sent_references(&text);
        assert_eq!(body, "");
        assert_eq!(refs.len(), 1);
    }

    #[test]
    fn split_leaves_inline_paths_and_lookalike_text_alone() {
        // Inline references are composed input — they stay in the body.
        let (body, refs) = split_sent_references("open \"/abs/a.rs\" now");
        assert_eq!(body, "open \"/abs/a.rs\" now");
        assert!(refs.is_empty());
        // A header mention that isn't a well-formed trailer stays put.
        for text in [
            "what does Referenced paths: mean?",
            "Referenced paths:\n- not quoted",
            "Referenced paths:\n- \"/abs/a.rs\"\ntrailing text",
            "note\nReferenced paths:",
        ] {
            let (body, refs) = split_sent_references(text);
            assert_eq!(body, text, "{text:?}");
            assert!(refs.is_empty(), "{text:?}");
        }
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
