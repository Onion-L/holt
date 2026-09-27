//! Token counts as the UI prints them: one formatter for every surface that
//! shows a count — a subagent chip's summary and the status strip's usage
//! line — so the same number never reads two ways.

const SUFFIXES: [&str; 4] = ["k", "M", "B", "T"];

/// A compact token count: `999`, `1.5k`, `1M`, `1.5B`. The scale switches
/// where a branch's own rounding would carry into the next suffix (a plain
/// `>= 1k` test prints `999_999` as `1000.0k`), and a trailing `.0` is
/// dropped so a billion reads `1B`, not `1.0B`.
pub(crate) fn compact_tokens(tokens: u64) -> String {
    compact_tokens_at(tokens, 1)
}

/// The same format one decimal finer — a tooltip's echo of the tile's
/// [`compact_tokens`] value: `47.7M` on the tile reads `47.71M` on hover.
/// Width is bounded the same way, so a tooltip never grows with the count
/// it spells.
pub(crate) fn compact_tokens_precise(tokens: u64) -> String {
    compact_tokens_at(tokens, 2)
}

fn compact_tokens_at(tokens: u64, decimals: u32) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    // The suffix switches where the mantissa's own rounding would carry
    // into the next one (`999_950` prints as `1000.0k` at one decimal),
    // so step up while the value still rounds to a full 1000. The
    // threshold stays in integers: `carries(s)` is the smallest count
    // whose mantissa at scale 1000^s rounds up to 1000.
    let carries = |suffix: u32| 1000u64.pow(suffix + 1) - 5 * 10u64.pow(3 * suffix - decimals - 1);
    let mut suffix = 1;
    while suffix < SUFFIXES.len() as u32 && tokens >= carries(suffix) {
        suffix += 1;
    }
    let value = format!(
        "{:.*}",
        decimals as usize,
        tokens as f64 / 1000f64.powi(suffix as i32)
    );
    let value = value.trim_end_matches('0').trim_end_matches('.');
    format!("{value}{}", SUFFIXES[suffix as usize - 1])
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

    #[test]
    fn precise_counts_give_one_extra_digit_but_stay_bounded() {
        assert_eq!(compact_tokens_precise(0), "0");
        assert_eq!(compact_tokens_precise(999), "999");
        assert_eq!(compact_tokens_precise(47_707_741), "47.71M");
        assert_eq!(compact_tokens_precise(999_949), "999.95k");
        assert_eq!(compact_tokens_precise(999_949_999), "999.95M");
        // The carry thresholds move with the decimals: one decimal flips
        // to the next suffix at 999_950, two at 999_995 / 999_999_500.
        assert_eq!(compact_tokens_precise(999_994), "999.99k");
        assert_eq!(compact_tokens_precise(999_995), "1M");
        assert_eq!(compact_tokens_precise(999_999_499), "1B");
        assert_eq!(compact_tokens_precise(1_519_624_030), "1.52B");
        assert_eq!(compact_tokens_precise(1_567_331_771), "1.57B");
    }
}
