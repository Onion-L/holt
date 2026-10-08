//! Port of `pi-core/agent/src/harness/session/jsonl/` — the JSONL v4
//! session codec, storage, and repository.
//!
//! Byte contract: lines are emitted in the TypeScript construction order
//! (`kind` first; entry mutations carry the provisioned entry fields
//! before `parentId`/`seq`/`timestamp`, records before `seq`/`timestamp`),
//! verified byte-for-byte against oracle goldens under
//! `tests/goldens/session-jsonl/`.

pub mod codec;
pub mod errors;
pub mod repo;
pub mod storage;
pub mod types;

pub use repo::{JsonlSessionRepo, list_jsonl_session_metadata, load_jsonl_session_storage};
pub use storage::JsonlSessionStorage;
pub use types::{
    JsonlSessionCreateOptions, JsonlSessionListOptions, JsonlSessionMetadata,
    JsonlSessionRepoOptions, JsonlV4Header,
};
