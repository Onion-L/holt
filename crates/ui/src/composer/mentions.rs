//! File mentions: a strict local-only Markdown link protocol plus the raw ↔
//! display text projection behind chips, shared with the transcript.

use std::ops::Range;

use gpui::SharedString;

/// The literal `@` a chip displays before its file name. Projected as TEXT so
/// it shapes, wraps, and hit-tests with the label — the earlier SVG icons
/// painted into a reserved whitespace slot never sat right at text size
/// (user report). Chips read as inline code: `@name` in the mono font over
/// the code wash.
const MENTION_PREFIX: char = '@';
const MENTION_SIDE_PAD: &str = "\u{00A0}";
/// A private URI scheme keeps file mentions distinguishable from ordinary
/// Markdown links pasted into the composer.
const FILE_MENTION_SCHEME: &str = "holt-file:";
/// A strict, local-only Markdown representation of a file mention. The
/// underlying prompt always contains this form; the editor projects it to a
/// chip for display without leaking a second data model into submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileMentionLink {
    pub(super) range: Range<usize>,
    basename: String,
    pub(super) path: String,
    pub(super) is_dir: bool,
}

fn percent_encode_path(path: &str) -> String {
    let mut out = String::new();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

fn percent_decode_path(encoded: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(encoded.len());
    let raw = encoded.as_bytes();
    let mut at = 0;
    while at < raw.len() {
        if raw[at] == b'%' {
            let hex = std::str::from_utf8(raw.get(at + 1..at + 3)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            bytes.push(raw[at]);
            at += 1;
        }
    }
    String::from_utf8(bytes).ok()
}

fn escape_mention_label(label: &str) -> String {
    label
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

pub(super) fn local_file_link(path: &str, is_dir: bool) -> String {
    let path = path.trim_end_matches('/');
    let basename = path
        .rsplit('/')
        .next()
        .filter(|part| !part.is_empty())
        .unwrap_or(path);
    format!(
        "[{}]({}{})",
        escape_mention_label(basename),
        FILE_MENTION_SCHEME,
        percent_encode_path(&format!("{path}{}", if is_dir { "/" } else { "" }))
    )
}

fn local_path_is_safe(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && !path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

fn label_close(text: &str, start: usize) -> Option<usize> {
    let mut escaped = false;
    for (at, ch) in text[start..].char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == ']' && text[start + at + 1..].starts_with('(') {
            return Some(start + at);
        }
    }
    None
}

fn file_mention_links(text: &str) -> Vec<FileMentionLink> {
    let mut links = Vec::new();
    let mut search = 0;
    while let Some(relative_start) = text[search..].find('[') {
        let start = search + relative_start;
        let Some(label_end) = label_close(text, start + 1) else {
            search = start + 1;
            continue;
        };
        let target_start = label_end + 2;
        let Some(relative_end) = text[target_start..].find(')') else {
            search = start + 1;
            continue;
        };
        let end = target_start + relative_end + 1;
        let label = &text[start + 1..label_end];
        let Some(encoded) = text[target_start..end - 1].strip_prefix(FILE_MENTION_SCHEME) else {
            search = end;
            continue;
        };
        let parsed = percent_decode_path(encoded).and_then(|target| {
            let is_dir = target.ends_with('/');
            let path = target.strip_suffix('/').unwrap_or(&target);
            (local_path_is_safe(path)
                && percent_encode_path(&target) == encoded
                && path
                    .rsplit('/')
                    .next()
                    .is_some_and(|basename| escape_mention_label(basename) == label))
            .then(|| (path.to_string(), is_dir))
        });
        if let Some((path, is_dir)) = parsed {
            let basename = path.rsplit('/').next().unwrap_or_default().to_string();
            links.push(FileMentionLink {
                range: start..end,
                basename,
                path,
                is_dir,
            });
        }
        search = end;
    }
    links
}

/// A leading `/skill <name>` token. The composer projects it to the chip
/// treatment skill invocations get elsewhere (accent colour, name only);
/// the raw text stays the submission format (`slash::parse`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SkillToken {
    pub(super) range: Range<usize>,
    name: String,
}

/// Skill names are single words; brackets are excluded so a token can never
/// overlap a file-mention link's Markdown.
fn skill_token(text: &str) -> Option<SkillToken> {
    const PREFIX: &str = "/skill ";
    let rest = text.strip_prefix(PREFIX)?;
    let name_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..name_len];
    if name.is_empty() || name.contains(['[', ']', '(', ')']) {
        return None;
    }
    Some(SkillToken {
        range: 0..PREFIX.len() + name_len,
        name: name.to_string(),
    })
}

/// The glyph a masked character projects to (U+2022 bullet, three UTF-8
/// bytes).
const MASK_CHAR: char = '•';

#[derive(Debug, Clone, Default)]
pub(super) struct TextProjection {
    pub(super) display: String,
    pub(super) mentions: Vec<(FileMentionLink, Range<usize>)>,
    /// Leading `/skill <name>` chip: at most one, always at raw offset 0.
    pub(super) skills: Vec<(SkillToken, Range<usize>)>,
    /// Secret projection: one bullet per raw char. Holds the raw byte offset
    /// of every char plus the end offset, for raw↔display translation.
    /// Mutually exclusive with `mentions` — secret inputs never enable them.
    mask_starts: Option<Vec<usize>>,
}
impl TextProjection {
    /// The identity projection for plain (non-mention, non-secret) inputs.
    pub(super) fn plain(raw: &str) -> Self {
        Self {
            display: raw.to_string(),
            mentions: Vec::new(),
            skills: Vec::new(),
            mask_starts: None,
        }
    }

    /// The secret projection: every character renders as a bullet. Unlike
    /// [`Self::new`], the display differs in byte length from the raw text,
    /// so the char-start table carries all offset translation.
    pub(super) fn masked(raw: &str) -> Self {
        let chars = raw.chars().count();
        let mut mask_starts = Vec::with_capacity(chars + 1);
        let mut display = String::with_capacity(chars * MASK_CHAR.len_utf8());
        for (offset, _) in raw.char_indices() {
            mask_starts.push(offset);
            display.push(MASK_CHAR);
        }
        mask_starts.push(raw.len());
        Self {
            display,
            mentions: Vec::new(),
            skills: Vec::new(),
            mask_starts: Some(mask_starts),
        }
    }

    /// Every chip — skill tokens and file mentions — as (raw range, display
    /// range) pairs in raw-text order. Skill chips sit at offset 0, so a
    /// stable sort keeps them ahead of any mention.
    fn chips(&self) -> Vec<(&Range<usize>, &Range<usize>)> {
        let mut chips: Vec<_> = self
            .skills
            .iter()
            .map(|(token, display)| (&token.range, display))
            .chain(
                self.mentions
                    .iter()
                    .map(|(link, display)| (&link.range, display)),
            )
            .collect();
        chips.sort_by_key(|(raw, _)| raw.start);
        chips
    }

    /// Chip display ranges in order, skill chips flagged — the text layout
    /// paints skill chips in the accent colour, file mentions as inline code.
    pub(super) fn chip_spans(&self) -> Vec<(Range<usize>, bool)> {
        let mut spans: Vec<_> = self
            .skills
            .iter()
            .map(|(_, display)| (display.clone(), true))
            .chain(
                self.mentions
                    .iter()
                    .map(|(_, display)| (display.clone(), false)),
            )
            .collect();
        spans.sort_by_key(|(display, _)| display.start);
        spans
    }

    pub(super) fn new(raw: &str) -> Self {
        let skill = skill_token(raw);
        let mut links = file_mention_links(raw);
        if let Some(skill) = &skill {
            links.retain(|link| link.range.start >= skill.range.end);
        }
        let labels = mention_display_labels(&links);
        let mut projection = Self::default();
        let mut raw_at = 0;
        if let Some(skill) = skill {
            let display_start = projection.display.len();
            projection.display.push_str(MENTION_SIDE_PAD);
            for ch in skill.name.chars() {
                projection
                    .display
                    .push(if ch == ' ' { '\u{00A0}' } else { ch });
            }
            projection.display.push('\u{00A0}');
            let display_end = projection.display.len();
            raw_at = skill.range.end;
            projection.skills.push((skill, display_start..display_end));
        }
        for (link, label) in links.into_iter().zip(labels) {
            projection.display.push_str(&raw[raw_at..link.range.start]);
            let display_start = projection.display.len();
            // The chip is plain projected text — `@` plus the label between
            // non-breaking side bearings; the rounded code wash beneath it is
            // painted by `ComposerTextElement::paint`. Every character here
            // must exist in Geist (no exotic whitespace — U+2003/U+202F shape
            // at fallback width and collapsed the chip once already).
            projection.display.push_str(MENTION_SIDE_PAD);
            projection.display.push(MENTION_PREFIX);
            for ch in label.chars() {
                projection
                    .display
                    .push(if ch == ' ' { '\u{00A0}' } else { ch });
            }
            projection.display.push('\u{00A0}');
            let display_end = projection.display.len();
            projection
                .mentions
                .push((link.clone(), display_start..display_end));
            raw_at = link.range.end;
        }
        projection.display.push_str(&raw[raw_at..]);
        projection
    }

    pub(super) fn raw_to_display(&self, raw: usize) -> usize {
        if let Some(starts) = &self.mask_starts {
            // Count whole chars before `raw` (a mid-char raw offset floors to
            // its char start, matching the caret's char-boundary indices).
            let chars_before = starts.partition_point(|&start| start < raw);
            return chars_before * MASK_CHAR.len_utf8();
        }
        let mut raw_at = 0;
        let mut display_at = 0;
        for (link, display) in self.chips() {
            if raw <= link.start {
                return display_at + raw.saturating_sub(raw_at);
            }
            if raw < link.end {
                return display.start;
            }
            raw_at = link.end;
            display_at = display.end;
        }
        display_at + raw.saturating_sub(raw_at)
    }

    pub(super) fn display_to_raw(&self, display_offset: usize) -> usize {
        if let Some(starts) = &self.mask_starts {
            // Shaped-line indices sit on bullet boundaries; anything else
            // (a stray mid-bullet offset) floors to the bullet's char start.
            let chars = (display_offset / MASK_CHAR.len_utf8()).min(starts.len() - 1);
            return starts[chars];
        }
        let mut raw_at = 0;
        let mut display_at = 0;
        for (link, display) in self.chips() {
            if display_offset <= display.start {
                return raw_at + display_offset.saturating_sub(display_at);
            }
            if display_offset < display.end {
                return if display_offset - display.start < display.len() / 2 {
                    link.start
                } else {
                    link.end
                };
            }
            raw_at = link.end;
            display_at = display.end;
        }
        raw_at + display_offset.saturating_sub(display_at)
    }

    pub(super) fn normalize_range(&self, range: Range<usize>) -> Range<usize> {
        if range.is_empty() {
            for (link, _) in self.chips() {
                if link.start < range.start && range.start < link.end {
                    let midpoint = link.start + link.len() / 2;
                    let at = if range.start < midpoint {
                        link.start
                    } else {
                        link.end
                    };
                    return at..at;
                }
            }
            return range;
        }
        let mut normalized = range;
        for (link, _) in self.chips() {
            if normalized.start < link.end && normalized.end > link.start {
                normalized.start = normalized.start.min(link.start);
                normalized.end = normalized.end.max(link.end);
            }
        }
        normalized
    }

    pub(super) fn previous_boundary(&self, raw: usize) -> Option<usize> {
        self.chips()
            .into_iter()
            .find_map(|(link, _)| (raw == link.end).then_some(link.start))
    }

    pub(super) fn next_boundary(&self, raw: usize) -> Option<usize> {
        self.chips()
            .into_iter()
            .find_map(|(link, _)| (raw == link.start).then_some(link.end))
    }
}
/// Basenames are compact in the common case. When the same basename appears
/// more than once, use the shortest unique path suffix so chips remain
/// distinguishable without always expanding to full paths.
fn mention_display_labels(links: &[FileMentionLink]) -> Vec<String> {
    links
        .iter()
        .enumerate()
        .map(|(ix, link)| {
            if links
                .iter()
                .filter(|other| other.basename == link.basename)
                .count()
                == 1
            {
                return link.basename.clone();
            }
            let parts: Vec<_> = link.path.split('/').collect();
            (1..=parts.len())
                .map(|count| parts[parts.len() - count..].join("/"))
                .find(|suffix| {
                    let suffix: Vec<_> = suffix.split('/').collect();
                    links.iter().enumerate().all(|(other_ix, other)| {
                        other_ix == ix
                            || !other
                                .path
                                .split('/')
                                .rev()
                                .take(suffix.len())
                                .eq(suffix.iter().rev().copied())
                    })
                })
                .unwrap_or_else(|| link.path.clone())
        })
        .collect()
}
/// One chip in a *sent* message: its byte range over the projected display
/// string (`@label` between side bearings). The transcript renders these
/// read-only — no editing state, no tooltip machinery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMentionSpan {
    pub range: Range<usize>,
    /// Full workspace-relative path (labels can be shortened to basenames).
    pub path: SharedString,
    pub is_dir: bool,
}

/// Project a sent message's raw Markdown for transcript display: mention links
/// collapse to the same chip labels the composer shows, everything else passes
/// through untouched. `None` when the text has no valid mention — the
/// substring probe keeps ordinary prompts on the zero-allocation path, so this
/// is safe to call for every user row.
pub fn sent_mention_display(raw: &str) -> Option<(String, Vec<SentMentionSpan>)> {
    if !raw.contains(FILE_MENTION_SCHEME) {
        return None;
    }
    let projection = TextProjection::new(raw);
    if projection.mentions.is_empty() {
        return None;
    }
    let spans = projection
        .mentions
        .iter()
        .map(|(link, display)| SentMentionSpan {
            range: display.clone(),
            path: SharedString::from(format!(
                "{}{}",
                link.path,
                if link.is_dir { "/" } else { "" }
            )),
            is_dir: link.is_dir,
        })
        .collect();
    Some((projection.display, spans))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_projection_translates_offsets_across_bullet_boundaries() {
        let projection = TextProjection::masked("sk-abc");
        assert_eq!(projection.display, "••••••");
        assert!(projection.mentions.is_empty());

        // Every raw char boundary maps to its bullet; the end maps to the end.
        for raw in 0..=6 {
            assert_eq!(
                projection.raw_to_display(raw),
                raw * MASK_CHAR.len_utf8(),
                "raw offset {raw}"
            );
            assert_eq!(
                projection.display_to_raw(raw * MASK_CHAR.len_utf8()),
                raw,
                "display offset {}",
                raw * MASK_CHAR.len_utf8()
            );
        }
        // Mid-bullet and out-of-range display offsets floor/clamp, never panic.
        assert_eq!(projection.display_to_raw(4), 1);
        assert_eq!(projection.display_to_raw(999), 6);
    }

    #[test]
    fn masked_projection_handles_multibyte_content() {
        // The table counts chars, not bytes, so multibyte keys still mask 1:1.
        let projection = TextProjection::masked("aé•z");
        assert_eq!(projection.display.chars().count(), 4);
        assert_eq!(projection.raw_to_display(0), 0);
        assert_eq!(projection.raw_to_display(1), 3); // after 'a'
        assert_eq!(projection.raw_to_display(3), 6); // after 'é' (2 bytes)
        assert_eq!(projection.raw_to_display(7), 12); // end: 4 bullets
        assert_eq!(projection.display_to_raw(12), 7);
    }

    #[test]
    fn plain_projection_is_the_identity() {
        let projection = TextProjection::plain("hello");
        assert_eq!(projection.display, "hello");
        assert_eq!(projection.raw_to_display(3), 3);
        assert_eq!(projection.display_to_raw(3), 3);
    }

    #[test]
    fn file_mentions_serialize_to_strict_local_markdown() {
        let raw = local_file_link("src/a file#[x].rs", false);
        assert_eq!(
            raw,
            "[a file#\\[x\\].rs](holt-file:src/a%20file%23%5Bx%5D.rs)"
        );
        let links = file_mention_links(&raw);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].path, "src/a file#[x].rs");
        assert_eq!(links[0].basename, "a file#[x].rs");
        assert!(!links[0].is_dir);

        let folder = local_file_link("src/components", true);
        assert_eq!(folder, "[components](holt-file:src/components/)");
        let links = file_mention_links(&folder);
        assert_eq!(links[0].path, "src/components");
        assert!(links[0].is_dir);
    }

    #[test]
    fn file_mentions_reject_external_or_noncanonical_markdown() {
        assert!(file_mention_links("[site](https://example.com/a)").is_empty());
        assert!(file_mention_links("[a.rs](../a.rs)").is_empty());
        assert!(file_mention_links("[a.rs](src/a file.rs)").is_empty());
        assert!(file_mention_links("[other](src/a.rs)").is_empty());
        assert!(file_mention_links("[a.rs](src/a.rs)").is_empty());
        assert!(file_mention_links("[a.rs](src%5Cfake%5Ca.rs)").is_empty());
        assert!(file_mention_links("[a.rs](src/a%0A.rs)").is_empty());
    }

    #[test]
    fn duplicate_mention_basenames_use_unique_suffixes() {
        let raw = format!(
            "{} {}",
            local_file_link("src/one/mod.rs", false),
            local_file_link("src/two/mod.rs", false)
        );
        let projection = TextProjection::new(&raw);
        assert!(projection.display.contains("one/mod.rs"));
        assert!(projection.display.contains("two/mod.rs"));
    }

    #[test]
    fn mention_suffixes_compare_path_components() {
        let links = vec![
            FileMentionLink {
                range: 0..0,
                basename: "mod.rs".into(),
                path: "foo/mod.rs".into(),
                is_dir: false,
            },
            FileMentionLink {
                range: 0..0,
                basename: "oomod.rs".into(),
                path: "bar/oomod.rs".into(),
                is_dir: false,
            },
        ];
        assert_eq!(
            mention_display_labels(&links),
            vec!["mod.rs".to_string(), "oomod.rs".to_string()]
        );
    }

    #[test]
    fn projection_maps_and_expands_atomic_chip_ranges() {
        let raw = format!("open {} now", local_file_link("src/composer.rs", false));
        let projection = TextProjection::new(&raw);
        let (link, chip) = &projection.mentions[0];
        assert_eq!(
            &projection.display[chip.clone()],
            "\u{00A0}@composer.rs\u{00A0}"
        );
        assert_eq!(projection.display_to_raw(chip.start + 1), link.range.start);
        assert_eq!(projection.display_to_raw(chip.end - 1), link.range.end);
        assert_eq!(
            projection.previous_boundary(link.range.end),
            Some(link.range.start)
        );
        assert_eq!(
            projection.next_boundary(link.range.start),
            Some(link.range.end)
        );
        assert_eq!(
            projection.normalize_range(link.range.start + 2..link.range.end - 2),
            link.range
        );
    }

    #[test]
    fn leading_skill_token_projects_to_an_accent_chip() {
        let projection = TextProjection::new("/skill setup refactor this");
        let (token, chip) = &projection.skills[0];
        assert_eq!(token.name, "setup");
        assert_eq!(&projection.display[chip.clone()], "\u{00A0}setup\u{00A0}");
        // The chip is atomic: offsets inside it snap to its raw boundaries.
        assert_eq!(projection.display_to_raw(chip.start + 1), token.range.start);
        assert_eq!(projection.display_to_raw(chip.end - 1), token.range.end);
        assert_eq!(
            projection.previous_boundary(token.range.end),
            Some(token.range.start)
        );
        // Extra instructions stay plain text right after the chip.
        let extra_at = projection.raw_to_display(token.range.end);
        assert_eq!(&projection.display[extra_at..], " refactor this");
        assert!(
            projection
                .chip_spans()
                .first()
                .is_some_and(|(_, skill)| *skill)
        );
    }

    #[test]
    fn skill_token_needs_the_leading_command_form() {
        assert!(TextProjection::new("/skill").skills.is_empty());
        assert!(TextProjection::new("/skill  setup").skills.is_empty());
        assert!(TextProjection::new("use /skill setup").skills.is_empty());
    }

    #[test]
    fn sent_mention_display_projects_chips_for_the_transcript() {
        let raw = format!(
            "check {} and {}",
            local_file_link("src/composer.rs", false),
            local_file_link("src/components", true)
        );
        let (display, spans) = sent_mention_display(&raw).expect("mentions project");
        assert!(!display.contains(FILE_MENTION_SCHEME));
        assert!(display.contains("composer.rs"));
        assert!(display.contains("components"));
        assert_eq!(spans.len(), 2);
        assert_eq!(
            &display[spans[0].range.clone()],
            "\u{00A0}@composer.rs\u{00A0}"
        );
        assert!(!spans[0].is_dir);
        assert_eq!(spans[0].path.as_ref(), "src/composer.rs");
        assert!(spans[1].is_dir);
        assert_eq!(spans[1].path.as_ref(), "src/components/");
    }

    /// Ordinary prompts must stay on the zero-cost path, including ones that
    /// merely *talk about* the scheme without containing a valid mention.
    #[test]
    fn sent_mention_display_leaves_plain_prompts_untouched() {
        assert_eq!(sent_mention_display("fix the composer"), None);
        assert_eq!(
            sent_mention_display("what is a holt-file: link?"),
            None,
            "scheme substring without a valid mention link"
        );
        assert_eq!(
            sent_mention_display("[a.rs](holt-file:../a.rs)"),
            None,
            "a hostile path never becomes a chip in the transcript either"
        );
    }
}
