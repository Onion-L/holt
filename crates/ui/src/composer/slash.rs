//! Slash-command interception (ADR-0006): the composer recognizes `/skill`
//! on submit and handles it itself — the raw directive never becomes a
//! prompt. Pure over the input text so the recognition is unit-testable
//! per the picker-logic pattern.

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
}

/// Recognize a leading `/skill` directive. Only the start of the input is
/// a command (`hello /skill x` is ordinary text), and `/skills`-style
/// longer words are not `/skill`.
pub(crate) fn parse(text: &str) -> Parsed {
    let Some(rest) = text.trim_start().strip_prefix("/skill") else {
        return Parsed::Plain;
    };
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return Parsed::Plain;
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
    fn longer_words_and_mid_text_directives_stay_plain() {
        assert_eq!(parse("/skills grill"), Parsed::Plain);
        assert_eq!(parse("/skillbook"), Parsed::Plain);
        assert_eq!(parse("run /skill grill please"), Parsed::Plain);
        assert_eq!(parse("just a normal prompt"), Parsed::Plain);
        assert_eq!(parse(""), Parsed::Plain);
    }
}
