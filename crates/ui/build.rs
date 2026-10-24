//! The `i18n!` macro (crate root, `lib.rs`) reads `locales/` at compile time
//! and embeds the parsed translations as generated code — cargo tracks
//! neither the reads nor the yml files, so a catalog edit alone would never
//! trigger a rebuild. Point the fingerprint at the directory.
fn main() {
    println!("cargo:rerun-if-changed=locales");
}
