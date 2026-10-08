//! Port of `pi-core/ai/src/auth/oauth`: OAuth flows and shared helpers.

pub mod anthropic;
pub mod device_code;
pub mod github_copilot;
pub mod kimi_coding;
pub mod load;
pub mod oauth_page;
pub mod openai_codex;
pub mod openrouter;
pub mod pkce;
pub mod radius;
pub mod xai;

/// Injectable wall clock (epoch milliseconds) shared by the OAuth flows.
pub type NowMs = std::sync::Arc<dyn Fn() -> i64 + Send + Sync>;
