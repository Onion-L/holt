//! Port of `pi-core/ai/src/utils/provider-env.ts`.

use crate::ai::types::ProviderEnv;

/// Port of `getProviderEnvValue`: resolves a provider env value from scoped
/// overrides, then normal process environment.
///
/// The TypeScript Bun-sandbox fallback (reading `/proc/self/environ` when
/// `process.env` is empty) is Bun-specific and has no Rust counterpart;
/// `std::env::var` reads the real process environment directly.
///
/// Like the TypeScript `||` chain, empty-string values fall through to the
/// next source.
pub fn get_provider_env_value(name: &str, env: Option<&ProviderEnv>) -> Option<String> {
    fn non_empty(value: String) -> Option<String> {
        (!value.is_empty()).then_some(value)
    }

    let from_env = env
        .and_then(|env| env.get(name).cloned())
        .and_then(non_empty);
    from_env.or_else(|| std::env::var(name).ok().and_then(non_empty))
}
