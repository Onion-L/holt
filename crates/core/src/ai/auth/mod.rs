//! Port of `pi-core/ai/src/auth`: authentication contracts, credential
//! storage, and resolution.
//!
//! `lazyOAuth` from `helpers.ts` is not ported as a distinct API: it wraps a
//! dynamically imported `OAuthAuth` for bundler tree-shaking, which has no
//! Rust counterpart — provider definitions hold `Arc<dyn OAuthAuth>` directly
//! and the runtime behavior (delegate on first call) is identical.

pub mod context;
pub mod credential_store;
pub mod helpers;
pub mod oauth;
pub mod resolve;
pub mod types;
