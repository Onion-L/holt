//! Port of `pi-core/agent/test/harness/truncate.test.ts`.
//!
//! Deviation: the TypeScript suite includes inputs built from unpaired
//! UTF-16 surrogates (`"a\ud83d"`, …), which cannot be represented in Rust
//! strings. The Rust port runs the same exhaustive and randomized
//! Buffer-tail-equivalence checks over the valid-UTF-8 alphabet instead;
//! the surrogate replacement paths are enforced by the type system.

use pi_core::agent::harness::utils::truncate::{
    TruncatedBy, TruncationOptions, truncate_head, truncate_tail,
};

fn buffer_tail(content: &str, max_bytes: usize) -> String {
    let bytes = content.as_bytes();
    if bytes.len() <= max_bytes {
        return content.to_string();
    }
    let mut start = bytes.len() - max_bytes;
    while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
        start += 1;
    }
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

fn assert_matches_buffer_tail(input: &str, max_byte_values: &[usize]) {
    let total_bytes = input.len();
    let values: Vec<usize> = if max_byte_values.is_empty() {
        (0..total_bytes + 5).collect()
    } else {
        max_byte_values.to_vec()
    };
    for max_bytes in values {
        let result = truncate_tail(
            input,
            TruncationOptions {
                max_bytes: Some(max_bytes),
                max_lines: Some(10),
            },
        );
        let expected = buffer_tail(input, max_bytes);
        assert_eq!(
            result.content, expected,
            "tail mismatch input={input:?} maxBytes={max_bytes}"
        );
        assert!(
            result.content.len() <= max_bytes,
            "tail output exceeded byte limit input={input:?} maxBytes={max_bytes} outputBytes={}",
            result.content.len()
        );
    }
}

fn sampled_byte_limits(input: &str) -> Vec<usize> {
    let total_bytes = input.len();
    let mut candidates = vec![
        0,
        1,
        2,
        3,
        4,
        5,
        8,
        total_bytes / 2,
        total_bytes.saturating_sub(1),
        total_bytes.saturating_sub(8),
        total_bytes.saturating_sub(5),
        total_bytes.saturating_sub(4),
        total_bytes.saturating_sub(3),
        total_bytes.saturating_sub(2),
        total_bytes,
        total_bytes + 1,
        total_bytes + 4,
    ];
    if total_bytes >= 1 {
        candidates.push(total_bytes / 2 + 1);
    }
    let mut unique: Vec<usize> = candidates
        .into_iter()
        .filter(|value| *value <= total_bytes.saturating_add(4))
        .collect();
    unique.sort_unstable();
    unique.dedup();
    unique
}

#[test]
fn counts_utf8_bytes() {
    let content = "aé🙂\nb";
    let result = truncate_head(
        content,
        TruncationOptions {
            max_bytes: Some(100),
            max_lines: Some(10),
        },
    );

    assert!(!result.truncated);
    assert_eq!(result.total_bytes, content.len());
    assert_eq!(result.output_bytes, content.len());
    assert_eq!(result.total_bytes, 9);
}

#[test]
fn does_not_count_a_trailing_newline_as_an_extra_line() {
    let content = "line\nline\nline\n";
    let head = truncate_head(
        content,
        TruncationOptions {
            max_bytes: Some(100),
            max_lines: Some(3),
        },
    );
    let tail = truncate_tail(
        content,
        TruncationOptions {
            max_bytes: Some(100),
            max_lines: Some(3),
        },
    );

    assert!(!head.truncated);
    assert_eq!(head.total_lines, 3);
    assert_eq!(head.output_lines, 3);
    assert!(!tail.truncated);
    assert_eq!(tail.total_lines, 3);
    assert_eq!(tail.output_lines, 3);
}

#[test]
fn truncates_head_on_utf8_byte_limits_without_partial_lines() {
    let content = "éé\nabc";
    let result = truncate_head(
        content,
        TruncationOptions {
            max_bytes: Some(4),
            max_lines: Some(10),
        },
    );

    assert_eq!(result.content, "éé");
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert_eq!(result.output_bytes, 4);
    assert!(!result.first_line_exceeds_limit);
}

#[test]
fn reports_head_truncation_when_the_first_line_exceeds_the_byte_limit() {
    let result = truncate_head(
        "éé\nabc",
        TruncationOptions {
            max_bytes: Some(3),
            max_lines: Some(10),
        },
    );

    assert_eq!(result.content, "");
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert!(result.first_line_exceeds_limit);
}

#[test]
fn truncates_tail_on_utf8_boundaries_when_only_a_partial_last_line_fits() {
    let result = truncate_tail(
        "aé🙂b",
        TruncationOptions {
            max_bytes: Some(5),
            max_lines: Some(10),
        },
    );

    assert_eq!(result.content, "🙂b");
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert!(result.last_line_partial);
    assert_eq!(result.output_bytes, 5);
}

#[test]
fn truncates_an_oversized_single_line_with_a_trailing_newline() {
    let input = format!("{}\n", "X".repeat(300_000));
    let result = truncate_tail(
        &input,
        TruncationOptions {
            max_bytes: Some(1024),
            max_lines: Some(100),
        },
    );

    assert_eq!(result.content, "X".repeat(1024));
    assert_eq!(result.output_bytes, 1024);
    assert_eq!(result.output_lines, 1);
    assert!(result.last_line_partial);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
}

#[test]
fn drops_an_oversized_trailing_character_when_it_cannot_fit_in_tail_byte_limit() {
    let result = truncate_tail(
        "abc🙂",
        TruncationOptions {
            max_bytes: Some(3),
            max_lines: Some(10),
        },
    );

    assert_eq!(result.content, "");
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert!(result.last_line_partial);
    assert_eq!(result.output_bytes, 0);
}

#[test]
fn matches_buffer_tail_truncation_semantics_for_astral_plane_inputs() {
    // The TypeScript surrogate inputs are unrepresentable in Rust strings;
    // the astral-plane (4-byte UTF-8) inputs exercise the same byte
    // boundary logic.
    assert_matches_buffer_tail("👩‍💻", &[]);
    assert_matches_buffer_tail("a🙂", &[]);
    assert_matches_buffer_tail("🙂ab", &[]);
    assert_matches_buffer_tail("é中🙂é中", &[]);
}

#[test]
fn matches_buffer_tail_truncation_semantics_across_deterministic_fuzz_cases() {
    let alphabet: Vec<&str> = vec![
        "a", "\u{7f}", "\u{80}", "é", "\u{7ff}", "\u{800}", "中", "\u{d7ff}", "🙂", "\u{e000}",
        "\u{ffff}",
    ];

    fn check_exhaustive(
        prefix: &str,
        depth: u32,
        alphabet: &[&str],
        sampled: &dyn Fn(&str) -> Vec<usize>,
    ) {
        let limits = sampled(prefix);
        assert_matches_buffer_tail(prefix, &limits);
        if depth == 0 {
            return;
        }
        for character in alphabet {
            check_exhaustive(
                &format!("{prefix}{character}"),
                depth - 1,
                alphabet,
                sampled,
            );
        }
    }
    check_exhaustive("", 3, &alphabet, &sampled_byte_limits);

    let mut seed: u32 = 0x1234_5678;
    let mut random = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(seed) / 4_294_967_296.0
    };
    for _ in 0..1_000 {
        let length = (random() * 80.0) as usize;
        let mut input = String::new();
        for _ in 0..length {
            input.push_str(alphabet[(random() * alphabet.len() as f64) as usize]);
        }
        let limits = sampled_byte_limits(&input);
        assert_matches_buffer_tail(&input, &limits);
    }
}
