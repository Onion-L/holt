//! Skill mention syntax shared by the composer and the engine: `$name` and
//! the linked form `[$name](SKILL.md path)`, recognized anywhere inside a
//! user message. The parser is pure text — no catalog knowledge; resolving a
//! mention against the skill catalog is the engine's job, and an unresolved
//! mention stays ordinary prompt text.

use std::ops::Range;

/// One `$` mention parsed out of a prompt. `path` carries the linked form's
/// target (the `SKILL.md` the label points at); the bare `$name` form has
/// none and resolves by name alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMention {
    /// The skill name — the `$label` without the sigil.
    pub name: String,
    /// The linked form's target path, verbatim (`~` unexpanded, relative
    /// unnormalized); `None` for the bare form.
    pub path: Option<String>,
    /// Byte range of the full token (`$name` or `[$name](path)`) in the
    /// scanned text.
    pub range: Range<usize>,
}

/// Characters a skill name may contain (the agents-skills slug set plus the
/// plugin namespacing colon).
fn is_name_char(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b':')
}

fn is_name_byte_boundary(text: &str, at: usize) -> bool {
    at == 0 || text[..at].ends_with(char::is_whitespace)
}

/// The linked form's target must be a `SKILL.md` file — the file name is the
/// discriminator that keeps ordinary Markdown links out of the mention set.
fn is_skill_path(path: &str) -> bool {
    path.rsplit(['/', '\\'])
        .next()
        .is_some_and(|base| base.eq_ignore_ascii_case("SKILL.md"))
}

/// Parse `[$name](…SKILL.md)` links, then bare `$name` tokens, anywhere in
/// the text. Plain Markdown links (labels without the `$` sigil, targets
/// that are not a `SKILL.md`) and `$`-prefixed words with invalid name
/// characters never match.
pub fn skill_mentions(text: &str) -> Vec<SkillMention> {
    let bytes = text.as_bytes();
    let mut mentions = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'[' {
            if let Some(mention) = parse_linked(text, bytes, at) {
                at = mention.range.end;
                mentions.push(mention);
                continue;
            }
            at += 1;
            continue;
        }
        if bytes[at] != b'$' || !is_name_byte_boundary(text, at) {
            at += 1;
            continue;
        }
        let name_start = at + 1;
        let mut name_end = name_start;
        while bytes.get(name_end).is_some_and(|&byte| is_name_char(byte)) {
            name_end += 1;
        }
        if name_end > name_start {
            mentions.push(SkillMention {
                name: text[name_start..name_end].to_string(),
                path: None,
                range: at..name_end,
            });
        }
        at = name_end.max(name_start);
    }
    mentions
}

/// `[$name](path)` — the label's `$sigil` marks a mention; the target must be
/// a `SKILL.md` (case-insensitive). Anything else is an ordinary Markdown
/// link and stays prompt text.
fn parse_linked(text: &str, bytes: &[u8], start: usize) -> Option<SkillMention> {
    let sigil = start + 1;
    if bytes.get(sigil) != Some(&b'$') {
        return None;
    }
    let name_start = sigil + 1;
    if !bytes
        .get(name_start)
        .is_some_and(|&byte| is_name_char(byte))
    {
        return None;
    }
    let mut name_end = name_start + 1;
    while bytes.get(name_end).is_some_and(|&byte| is_name_char(byte)) {
        name_end += 1;
    }
    if bytes.get(name_end) != Some(&b']') || bytes.get(name_end + 1) != Some(&b'(') {
        return None;
    }
    let path_start = name_end + 2;
    let mut path_end = path_start;
    while bytes.get(path_end).is_some_and(|&byte| byte != b')') {
        path_end += 1;
    }
    if bytes.get(path_end) != Some(&b')') {
        return None;
    }
    let path = text[path_start..path_end].trim();
    if path.is_empty() || !is_skill_path(path) {
        return None;
    }
    Some(SkillMention {
        name: text[name_start..name_end].to_string(),
        path: Some(path.to_string()),
        range: start..path_end + 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_and_bare_forms_parse() {
        let text =
            "a [$security-audit](/Users/u/.agents/skills/security-audit/SKILL.md) b $grilling c";
        let mentions = skill_mentions(text);
        assert_eq!(mentions.len(), 2);
        assert_eq!(
            mentions[0],
            SkillMention {
                name: "security-audit".into(),
                path: Some("/Users/u/.agents/skills/security-audit/SKILL.md".into()),
                range: 2..68,
            }
        );
        assert_eq!(mentions[1].name, "grilling");
        assert_eq!(mentions[1].path, None);
        assert_eq!(&text[mentions[1].range.clone()], "$grilling");
        // Ranges are byte-exact slices of the scanned text.
        assert_eq!(
            &text[mentions[0].range.clone()],
            "[$security-audit](/Users/u/.agents/skills/security-audit/SKILL.md)"
        );
    }

    #[test]
    fn name_charset_and_boundaries() {
        let mentions = skill_mentions("$a-b:c9 $x_y plain");
        assert_eq!(
            mentions.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["a-b:c9", "x_y"]
        );
        // `$100` parses (digits are name chars) — an unknown name simply
        // stays literal text after resolution, so no env-var blacklist.
        assert_eq!(skill_mentions("pay $100").len(), 1);
        // Mid-word `$` and missing names stay text.
        assert!(skill_mentions("3$pm raises").is_empty());
        assert!(skill_mentions("email me $ for that").is_empty());
    }

    #[test]
    fn plain_markdown_links_are_not_mentions() {
        assert!(skill_mentions("[docs](https://example.com/SKILL.md)").is_empty());
        assert!(skill_mentions("[a.rs](holt-file:src/a.rs)").is_empty());
        // A $label pointing at a non-SKILL.md target is an ordinary link.
        assert!(skill_mentions("[$grill](/tmp/notes.md)").is_empty());
        // …but a following bare mention in the same text still parses.
        let mentions = skill_mentions("[$grill](/tmp/notes.md) and $audit");
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].name, "audit");
    }

    #[test]
    fn skill_path_is_case_insensitive_and_space_tolerant() {
        let mentions = skill_mentions("[$x](/root/X/skill.md)");
        assert_eq!(mentions[0].path.as_deref(), Some("/root/X/skill.md"));
        let spaced = skill_mentions("[$x](/root/my skills/x/SKILL.md)");
        assert_eq!(
            spaced[0].path.as_deref(),
            Some("/root/my skills/x/SKILL.md")
        );
    }

    #[test]
    fn unterminated_or_malformed_links_stay_text() {
        assert!(skill_mentions("[$x](/a/SKILL.md").is_empty());
        assert!(skill_mentions("[$ x](/a/SKILL.md)").is_empty());
        assert!(skill_mentions("[$x /a/SKILL.md)").is_empty());
        assert!(skill_mentions("[x](/a/SKILL.md)").is_empty());
        assert!(skill_mentions("[](/a/SKILL.md)").is_empty());
    }
}
