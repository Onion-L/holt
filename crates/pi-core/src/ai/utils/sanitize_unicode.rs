//! Port of `pi-core/ai/src/utils/sanitize-unicode.ts`.
//!
//! The TypeScript helper strips unpaired UTF-16 surrogates, which JS strings
//! can carry and which break JSON serialization on many providers. Rust
//! `String` values are always valid UTF-8 and cannot contain unpaired
//! surrogates, so the sanitizer is an identity function kept for API parity.

/// Port of `sanitizeSurrogates`. In Rust this is structurally the identity
/// function: a `String` cannot hold unpaired surrogates. Valid emoji and
/// other non-BMP characters are unaffected, as in TypeScript.
pub fn sanitize_surrogates(text: &str) -> String {
    text.to_string()
}
