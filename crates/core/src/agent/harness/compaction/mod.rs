//! Port of `pi-core/agent/src/harness/compaction/`.

// `compaction::compaction` mirrors the TypeScript `compaction.ts` module name.
pub mod branch_summarization;
#[allow(clippy::module_inception)]
pub mod compaction;
pub mod utils;

pub use utils::{
    FileOperations, compute_file_lists, create_file_ops, extract_file_ops_from_message,
    format_file_operations, serialize_conversation,
};
