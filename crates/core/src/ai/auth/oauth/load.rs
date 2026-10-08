//! Port of `pi-core/ai/src/auth/oauth/load.ts`.
//!
//! The TypeScript loaders exist so bundlers cannot follow dynamic imports
//! into Node-only flow code and so standalone binaries can register bundled
//! loaders. Rust links the flows statically, so each loader returns the
//! shared flow value directly and `registerBundledOAuthFlowLoaders` has no
//! counterpart.

use std::sync::Arc;

use crate::ai::auth::oauth::anthropic::anthropic_oauth;
use crate::ai::auth::oauth::github_copilot::github_copilot_oauth;
use crate::ai::auth::oauth::kimi_coding::kimi_coding_oauth;
use crate::ai::auth::oauth::openai_codex::openai_codex_oauth;
use crate::ai::auth::oauth::openrouter::open_router_oauth;
use crate::ai::auth::oauth::xai::xai_oauth;
use crate::ai::auth::types::OAuthAuth;

pub use crate::ai::auth::oauth::radius::{RadiusOAuthOptions, create_radius_oauth};

/// Port of `loadAnthropicOAuth`.
pub fn load_anthropic_oauth() -> Arc<dyn OAuthAuth> {
    anthropic_oauth()
}

/// Port of `loadOpenAICodexOAuth`.
pub fn load_openai_codex_oauth() -> Arc<dyn OAuthAuth> {
    openai_codex_oauth()
}

/// Port of `loadGitHubCopilotOAuth`.
pub fn load_github_copilot_oauth() -> Arc<dyn OAuthAuth> {
    github_copilot_oauth()
}

/// Port of `loadOpenRouterOAuth`.
pub fn load_open_router_oauth() -> Arc<dyn OAuthAuth> {
    open_router_oauth()
}

/// Port of `loadKimiCodingOAuth`.
pub fn load_kimi_coding_oauth() -> Arc<dyn OAuthAuth> {
    kimi_coding_oauth()
}

/// Port of `loadXaiOAuth`.
pub fn load_xai_oauth() -> Arc<dyn OAuthAuth> {
    xai_oauth()
}
