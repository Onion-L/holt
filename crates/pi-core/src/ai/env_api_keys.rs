//! Port of `pi-core/ai/src/env-api-keys.ts`: provider env-var discovery for
//! API keys.
//!
//! The Bun/browser lazy-`node:fs` loading collapses to direct filesystem
//! access; the Vertex ADC path check uses the real filesystem with `~`
//! expansion.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use crate::ai::types::ProviderEnv;
use crate::ai::utils::provider_env::get_provider_env_value;

pub const ANTHROPIC_AUTH_TOKEN_ENV: &str = "ANTHROPIC_AUTH_TOKEN";
pub const ANTHROPIC_OAUTH_TOKEN_ENV: &str = "ANTHROPIC_OAUTH_TOKEN";
pub const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

fn adc_cache() -> &'static std::sync::Mutex<Option<bool>> {
    static CACHE: OnceLock<std::sync::Mutex<Option<bool>>> = OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Port of `hasVertexAdcCredentials`.
fn has_vertex_adc_credentials(env: Option<&ProviderEnv>) -> bool {
    if let Some(env) = env
        && let Some(explicit) = env.get("GOOGLE_APPLICATION_CREDENTIALS")
        && !explicit.is_empty()
    {
        return std::path::Path::new(explicit).exists();
    }

    {
        let mut cache = adc_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.is_none() {
            // Check GOOGLE_APPLICATION_CREDENTIALS env var first (standard
            // way).
            let gac_path = get_provider_env_value("GOOGLE_APPLICATION_CREDENTIALS", env);
            let exists = match gac_path {
                Some(path) => std::path::Path::new(&path).exists(),
                None => crate::ai::auth::context::expand_home(
                    "~/.config/gcloud/application_default_credentials.json",
                )
                .exists(),
            };
            *cache = Some(exists);
        }
        cache.unwrap_or(false)
    }
}

/// Port of `getApiKeyEnvVars`.
fn get_api_key_env_vars(provider: &str) -> Option<Vec<&'static str>> {
    if provider == "github-copilot" {
        return Some(vec!["COPILOT_GITHUB_TOKEN"]);
    }

    // ANTHROPIC_AUTH_TOKEN participates in env discovery/status, but
    // getEnvApiKey() skips it because requests must pass it as
    // Authorization: Bearer.
    if provider == "anthropic" {
        return Some(vec![
            ANTHROPIC_AUTH_TOKEN_ENV,
            ANTHROPIC_OAUTH_TOKEN_ENV,
            ANTHROPIC_API_KEY_ENV,
        ]);
    }

    const ENV_MAP: &[(&str, &str)] = &[
        ("ant-ling", "ANT_LING_API_KEY"),
        ("qwen-token-plan", "QWEN_TOKEN_PLAN_API_KEY"),
        ("qwen-token-plan-cn", "QWEN_TOKEN_PLAN_CN_API_KEY"),
        ("qwen-token-plan-individual", "QWEN_TOKEN_PLAN_API_KEY"),
        ("openai", "OPENAI_API_KEY"),
        ("azure-openai-responses", "AZURE_OPENAI_API_KEY"),
        ("nvidia", "NVIDIA_API_KEY"),
        ("deepseek", "DEEPSEEK_API_KEY"),
        ("google", "GEMINI_API_KEY"),
        ("google-vertex", "GOOGLE_CLOUD_API_KEY"),
        ("groq", "GROQ_API_KEY"),
        ("cerebras", "CEREBRAS_API_KEY"),
        ("xai", "XAI_API_KEY"),
        ("radius", "RADIUS_API_KEY"),
        ("openrouter", "OPENROUTER_API_KEY"),
        ("vercel-ai-gateway", "AI_GATEWAY_API_KEY"),
        ("zai", "ZAI_API_KEY"),
        ("zai-coding-cn", "ZAI_CODING_CN_API_KEY"),
        ("mistral", "MISTRAL_API_KEY"),
        ("minimax", "MINIMAX_API_KEY"),
        ("minimax-cn", "MINIMAX_CN_API_KEY"),
        ("moonshotai", "MOONSHOT_API_KEY"),
        ("moonshotai-cn", "MOONSHOT_API_KEY"),
        ("huggingface", "HF_TOKEN"),
        ("fireworks", "FIREWORKS_API_KEY"),
        ("together", "TOGETHER_API_KEY"),
        ("baseten", "BASETEN_API_KEY"),
        ("opencode", "OPENCODE_API_KEY"),
        ("opencode-go", "OPENCODE_API_KEY"),
        ("kimi-coding", "KIMI_API_KEY"),
        ("cloudflare-workers-ai", "CLOUDFLARE_API_KEY"),
        ("cloudflare-ai-gateway", "CLOUDFLARE_API_KEY"),
        ("xiaomi", "XIAOMI_API_KEY"),
        ("xiaomi-token-plan-cn", "XIAOMI_TOKEN_PLAN_CN_API_KEY"),
        ("xiaomi-token-plan-ams", "XIAOMI_TOKEN_PLAN_AMS_API_KEY"),
        ("xiaomi-token-plan-sgp", "XIAOMI_TOKEN_PLAN_SGP_API_KEY"),
    ];
    let env_map: BTreeMap<&str, &str> = ENV_MAP.iter().copied().collect();
    env_map.get(provider).map(|env_var| vec![*env_var])
}

/// Port of `findEnvKeys`: reports configured env vars that can provide an API
/// key. Ambient credential sources are intentionally excluded.
pub fn find_env_keys(provider: &str, env: Option<&ProviderEnv>) -> Option<Vec<String>> {
    let env_vars = get_api_key_env_vars(provider)?;
    let found: Vec<String> = env_vars
        .into_iter()
        .filter(|env_var| get_provider_env_value(env_var, env).is_some())
        .map(str::to_string)
        .collect();
    (!found.is_empty()).then_some(found)
}

/// Port of `getEnvApiKey`: resolves an API key from known provider env vars.
/// Returns `"<authenticated>"` for ambient-credential providers (Vertex ADC,
/// Amazon Bedrock) that are configured without a literal key.
pub fn get_env_api_key(provider: &str, env: Option<&ProviderEnv>) -> Option<String> {
    if let Some(env_keys) = find_env_keys(provider, env)
        && let Some(api_key_env) = if provider == "anthropic" {
            env_keys
                .iter()
                .find(|key| key.as_str() != ANTHROPIC_AUTH_TOKEN_ENV)
        } else {
            env_keys.first()
        }
    {
        return get_provider_env_value(api_key_env, env);
    }

    // Vertex AI supports either an explicit API key or Application Default
    // Credentials via `gcloud auth application-default login`.
    if provider == "google-vertex" {
        let has_credentials = has_vertex_adc_credentials(env);
        let has_project = get_provider_env_value("GOOGLE_CLOUD_PROJECT", env).is_some()
            || get_provider_env_value("GCLOUD_PROJECT", env).is_some();
        let has_location = get_provider_env_value("GOOGLE_CLOUD_LOCATION", env).is_some();
        if has_credentials && has_project && has_location {
            return Some("<authenticated>".to_string());
        }
    }

    if provider == "amazon-bedrock" {
        // Amazon Bedrock supports multiple credential sources: AWS_PROFILE,
        // IAM keys, bearer tokens, ECS task roles, and IRSA.
        let configured = get_provider_env_value("AWS_PROFILE", env).is_some()
            || (get_provider_env_value("AWS_ACCESS_KEY_ID", env).is_some()
                && get_provider_env_value("AWS_SECRET_ACCESS_KEY", env).is_some())
            || get_provider_env_value("AWS_BEARER_TOKEN_BEDROCK", env).is_some()
            || get_provider_env_value("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", env).is_some()
            || get_provider_env_value("AWS_CONTAINER_CREDENTIALS_FULL_URI", env).is_some()
            || get_provider_env_value("AWS_WEB_IDENTITY_TOKEN_FILE", env).is_some();
        if configured {
            return Some("<authenticated>".to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_from(pairs: &[(&str, &str)]) -> ProviderEnv {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn finds_and_resolves_provider_env_keys() {
        // Reads the ambient environment; share the env-var lock with tests
        // that mutate it.
        let _guard = crate::ai::test_env_lock();
        let env = env_from(&[
            ("OPENAI_API_KEY", "sk-openai"),
            ("GEMINI_API_KEY", "gemini-key"),
        ]);
        assert_eq!(
            find_env_keys("openai", Some(&env)),
            Some(vec!["OPENAI_API_KEY".to_string()])
        );
        assert_eq!(
            get_env_api_key("openai", Some(&env)),
            Some("sk-openai".to_string())
        );
        assert_eq!(
            get_env_api_key("google", Some(&env)),
            Some("gemini-key".to_string())
        );
        assert_eq!(get_env_api_key("openai", None), None);
        assert_eq!(find_env_keys("unknown-provider", Some(&env)), None);
    }

    #[test]
    fn anthropic_skips_auth_token_for_key_resolution() {
        let env = env_from(&[
            ("ANTHROPIC_AUTH_TOKEN", "bearer-token"),
            ("ANTHROPIC_API_KEY", "sk-ant"),
        ]);
        // findEnvKeys reports all configured candidates including the token.
        assert_eq!(
            find_env_keys("anthropic", Some(&env)),
            Some(vec![
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                "ANTHROPIC_API_KEY".to_string()
            ])
        );
        // getEnvApiKey skips ANTHROPIC_AUTH_TOKEN.
        assert_eq!(
            get_env_api_key("anthropic", Some(&env)),
            Some("sk-ant".to_string())
        );
    }

    #[test]
    fn bedrock_reports_authenticated_without_a_key() {
        let _guard = crate::ai::test_env_lock();
        let env = env_from(&[
            ("AWS_ACCESS_KEY_ID", "id"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
        ]);
        assert_eq!(
            get_env_api_key("amazon-bedrock", Some(&env)),
            Some("<authenticated>".to_string())
        );
        assert_eq!(get_env_api_key("amazon-bedrock", None), None);
    }

    // Port of "does not treat generic GitHub tokens as GitHub Copilot
    // credentials". The scoped env carries GH_TOKEN/GITHUB_TOKEN; neither is a
    // Copilot credential source. Ambient COPILOT_GITHUB_TOKEN would leak
    // through the process-env fallback, so share the env lock.
    #[test]
    fn generic_github_tokens_are_not_copilot_credentials() {
        let _guard = crate::ai::test_env_lock();
        let env = env_from(&[("GH_TOKEN", "gh-token"), ("GITHUB_TOKEN", "github-token")]);
        assert_eq!(find_env_keys("github-copilot", Some(&env)), None);
        assert_eq!(get_env_api_key("github-copilot", Some(&env)), None);
    }

    // Port of "resolves GitHub Copilot credentials from
    // COPILOT_GITHUB_TOKEN": the dedicated env var wins over generic GitHub
    // tokens.
    #[test]
    fn copilot_credentials_resolve_from_copilot_github_token() {
        let _guard = crate::ai::test_env_lock();
        let env = env_from(&[
            ("COPILOT_GITHUB_TOKEN", "copilot-token"),
            ("GH_TOKEN", "gh-token"),
            ("GITHUB_TOKEN", "github-token"),
        ]);
        assert_eq!(
            find_env_keys("github-copilot", Some(&env)),
            Some(vec!["COPILOT_GITHUB_TOKEN".to_string()])
        );
        assert_eq!(
            get_env_api_key("github-copilot", Some(&env)),
            Some("copilot-token".to_string())
        );
    }

    // Port of "resolves ZAI China Coding Plan credentials from
    // ZAI_CODING_CN_API_KEY".
    #[test]
    fn zai_coding_cn_resolves_from_zai_coding_cn_api_key() {
        let _guard = crate::ai::test_env_lock();
        let env = env_from(&[("ZAI_CODING_CN_API_KEY", "zai-coding-cn-token")]);
        assert_eq!(
            find_env_keys("zai-coding-cn", Some(&env)),
            Some(vec!["ZAI_CODING_CN_API_KEY".to_string()])
        );
        assert_eq!(
            get_env_api_key("zai-coding-cn", Some(&env)),
            Some("zai-coding-cn-token".to_string())
        );
    }

    // Port of "reports ANTHROPIC_AUTH_TOKEN but preserves OAuth token API key
    // lookup": all three env vars are reported, and the first non-bearer
    // candidate (the OAuth token) wins the key lookup.
    #[test]
    fn anthropic_reports_auth_token_and_oauth_wins_key_lookup() {
        let _guard = crate::ai::test_env_lock();
        let env = env_from(&[
            ("ANTHROPIC_AUTH_TOKEN", "auth-token"),
            ("ANTHROPIC_OAUTH_TOKEN", "oauth-token"),
            ("ANTHROPIC_API_KEY", "api-key"),
        ]);
        assert_eq!(
            find_env_keys("anthropic", Some(&env)),
            Some(vec![
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                "ANTHROPIC_OAUTH_TOKEN".to_string(),
                "ANTHROPIC_API_KEY".to_string(),
            ])
        );
        assert_eq!(
            get_env_api_key("anthropic", Some(&env)),
            Some("oauth-token".to_string())
        );
    }

    // Port of "does not return ANTHROPIC_AUTH_TOKEN as an API key": the token
    // is reported by discovery but never resolves to a key.
    #[test]
    fn anthropic_auth_token_alone_yields_no_api_key() {
        let _guard = crate::ai::test_env_lock();
        let env = env_from(&[("ANTHROPIC_AUTH_TOKEN", "auth-token")]);
        assert_eq!(
            find_env_keys("anthropic", Some(&env)),
            Some(vec!["ANTHROPIC_AUTH_TOKEN".to_string()])
        );
        assert_eq!(get_env_api_key("anthropic", Some(&env)), None);
    }

    // Port of "preserves ANTHROPIC_OAUTH_TOKEN as an API key".
    #[test]
    fn anthropic_oauth_token_alone_is_the_api_key() {
        let _guard = crate::ai::test_env_lock();
        let env = env_from(&[("ANTHROPIC_OAUTH_TOKEN", "oauth-token")]);
        assert_eq!(
            find_env_keys("anthropic", Some(&env)),
            Some(vec!["ANTHROPIC_OAUTH_TOKEN".to_string()])
        );
        assert_eq!(
            get_env_api_key("anthropic", Some(&env)),
            Some("oauth-token".to_string())
        );
    }
}
