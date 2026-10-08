//! Port of `@earendil-works/pi-agent-core` v0.84.4 (`pi-core/agent/src`).
//!
//! Ported in dependency order after pi-ai; per-module status lives in
//! `MIGRATION.md` at the repository root.

// `agent::agent` mirrors the TypeScript `agent.ts` module name.
#[allow(clippy::module_inception)]
pub mod agent;
pub mod agent_loop;
pub mod harness;
pub mod node;
pub mod proxy;
pub mod search;
pub mod stream_fn;
pub mod types;
