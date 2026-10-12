//! The composer's appended path-reference trailer: a header line plus
//! `- "path"` items riding the prompt as plain text — the model reads the
//! list, the transcript lifts it back out to render attachment rows. Both
//! ends share the header (the UI appends the trailer, the engine strips it
//! from title input), so the format can never drift between crates.

/// The header line introducing the appended path list.
pub const REFS_HEADER: &str = "Referenced paths:";
