//! Token counts as the UI prints them: one formatter for every surface that
//! shows a count — a subagent chip's summary and the status strip's usage
//! line — so the same number never reads two ways.

/// A compact token count: `999`, `1.5k`, `1M`, `1.5B`. The scale switches
/// where a branch's own rounding would carry into the next suffix (a plain
/// `>= 1k` test prints `999_999` as `1000.0k`), and a trailing `.0` is
/// dropped so a billion reads `1B`, not `1.0B`.
pub(crate) fn compact_tokens(tokens: u64) -> String {
    let (scale, suffix) = if tokens >= 999_950_000 {
        (1_000_000_000.0, "B")
    } else if tokens >= 999_950 {
        (1_000_000.0, "M")
    } else if tokens >= 1_000 {
        (1_000.0, "k")
    } else {
        return tokens.to_string();
    };
    let value = format!("{:.1}", tokens as f64 / scale);
    format!("{}{suffix}", value.trim_end_matches(".0"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_counts_never_print_a_carried_suffix() {
        assert_eq!(compact_tokens(0), "0");
        assert_eq!(compact_tokens(999), "999");
        assert_eq!(compact_tokens(1_000), "1k");
        assert_eq!(compact_tokens(1_500), "1.5k");
        assert_eq!(compact_tokens(12_300), "12.3k");
        assert_eq!(compact_tokens(999_949), "999.9k");
        assert_eq!(compact_tokens(999_950), "1M");
        assert_eq!(compact_tokens(999_999), "1M");
        assert_eq!(compact_tokens(1_000_000), "1M");
        assert_eq!(compact_tokens(272_000), "272k");
        assert_eq!(compact_tokens(1_050_000), "1.1M");
        assert_eq!(compact_tokens(999_949_999), "999.9M");
        assert_eq!(compact_tokens(999_950_000), "1B");
        assert_eq!(compact_tokens(1_019_364_363), "1B");
        assert_eq!(compact_tokens(1_050_000_000), "1.1B");
    }
}
