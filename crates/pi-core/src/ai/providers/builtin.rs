//! Port of `pi-core/ai/src/providers/all.ts` plus the uniform provider
//! factories it aggregates.
//!
//! The uniform env-key factories (one TS file each) are table-driven here:
//! they differ only in id, name, base URL, auth label, and env var. Bespoke
//! providers live in their own modules. `amazon-bedrock` lands with the
//! bedrock converse-stream module and `radius` with the dynamic radius
//! provider port; both slots are noted below.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::ai::auth::helpers::env_api_key_auth;
use crate::ai::auth::oauth::load::{
    load_kimi_coding_oauth, load_open_router_oauth, load_openai_codex_oauth, load_xai_oauth,
};
use crate::ai::auth::types::{Credential, ProviderAuth};
use crate::ai::images_models::{
    CreateImagesProviderOptions, ImagesModels, ImagesProvider, ProviderImages,
    create_images_models, create_images_provider,
};
use crate::ai::models::{
    CreateModelsOptions, CreateProviderOptions, FilterModelsFn, Models, Provider, ProviderApi,
    ProviderStreams, create_provider,
};
use crate::ai::models_generated::{
    generated_at, image_models_for_provider, models, models_for_provider, provider_ids,
};
use crate::ai::providers::anthropic::anthropic_provider;
use crate::ai::providers::apis::{
    anthropic_messages_api, azure_openai_responses_api, google_generative_ai_api,
    mistral_conversations_api, openai_codex_responses_api, openai_completions_api,
    openai_responses_api,
};
use crate::ai::providers::cloudflare_ai_gateway::cloudflare_ai_gateway_provider;
use crate::ai::providers::cloudflare_workers_ai::cloudflare_workers_ai_provider;
use crate::ai::providers::google_vertex::google_vertex_provider;
use crate::ai::types::{ImagesContext, ImagesModel, ImagesOptions, Model};

/// Rust-side extension: organization grouping for built-in providers, keyed
/// by provider id. Upstream TypeScript has no such concept, so this mapping
/// is maintained by hand next to the provider definitions; providers that
/// stand alone report `None` and consumers fall back to the provider id.
pub(crate) fn organization_for_provider(id: &str) -> Option<&'static str> {
    match id {
        "openai" | "openai-codex" => Some("openai"),
        "google" | "google-vertex" => Some("google"),
        "minimax" | "minimax-cn" => Some("minimax"),
        "moonshotai" | "moonshotai-cn" => Some("moonshot"),
        "kimi-coding" => Some("moonshot-kimi"),
        "xiaomi" | "xiaomi-token-plan-ams" | "xiaomi-token-plan-cn" | "xiaomi-token-plan-sgp" => {
            Some("xiaomi")
        }
        "qwen-token-plan" | "qwen-token-plan-cn" | "qwen-token-plan-individual" => Some("qwen"),
        "zai" | "zai-coding-cn" => Some("zai"),
        "opencode" | "opencode-go" => Some("opencode"),
        "cloudflare-ai-gateway" | "cloudflare-workers-ai" => Some("cloudflare"),
        _ => None,
    }
}

/// One uniform env-key provider row: `createProvider` with an
/// `envApiKeyAuth`, a base URL, and a single API implementation.
struct UniformSpec {
    id: &'static str,
    name: &'static str,
    base_url: Option<&'static str>,
    auth_name: &'static str,
    env_var: &'static str,
}

const UNIFORM_OPENAI_COMPLETIONS: &[UniformSpec] = &[
    spec(
        "ant-ling",
        "Ant Ling",
        Some("https://api.ant-ling.com/v1"),
        "Ant Ling API key",
        "ANT_LING_API_KEY",
    ),
    spec(
        "baseten",
        "Baseten",
        Some("https://inference.baseten.co/v1"),
        "Baseten API key",
        "BASETEN_API_KEY",
    ),
    spec(
        "cerebras",
        "Cerebras",
        Some("https://api.cerebras.ai/v1"),
        "Cerebras API key",
        "CEREBRAS_API_KEY",
    ),
    spec(
        "deepseek",
        "DeepSeek",
        Some("https://api.deepseek.com"),
        "DeepSeek API key",
        "DEEPSEEK_API_KEY",
    ),
    spec(
        "groq",
        "Groq",
        Some("https://api.groq.com/openai/v1"),
        "Groq API key",
        "GROQ_API_KEY",
    ),
    spec(
        "huggingface",
        "Hugging Face",
        Some("https://router.huggingface.co/v1"),
        "Hugging Face token",
        "HF_TOKEN",
    ),
    spec(
        "moonshotai",
        "Moonshot AI",
        Some("https://api.moonshot.ai/v1"),
        "Moonshot AI API key",
        "MOONSHOT_API_KEY",
    ),
    spec(
        "moonshotai-cn",
        "Moonshot AI CN",
        Some("https://api.moonshot.cn/v1"),
        "Moonshot AI API key",
        "MOONSHOT_API_KEY",
    ),
    spec(
        "nvidia",
        "NVIDIA",
        Some("https://integrate.api.nvidia.com/v1"),
        "NVIDIA API key",
        "NVIDIA_API_KEY",
    ),
    spec(
        "together",
        "Together",
        Some("https://api.together.ai/v1"),
        "Together API key",
        "TOGETHER_API_KEY",
    ),
    spec(
        "xiaomi",
        "Xiaomi",
        Some("https://api.xiaomimimo.com/v1"),
        "Xiaomi API key",
        "XIAOMI_API_KEY",
    ),
    spec(
        "xiaomi-token-plan-ams",
        "Xiaomi Token Plan AMS",
        Some("https://token-plan-ams.xiaomimimo.com/v1"),
        "Xiaomi Token Plan AMS API key",
        "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
    ),
    spec(
        "xiaomi-token-plan-cn",
        "Xiaomi Token Plan CN",
        Some("https://token-plan-cn.xiaomimimo.com/v1"),
        "Xiaomi Token Plan CN API key",
        "XIAOMI_TOKEN_PLAN_CN_API_KEY",
    ),
    spec(
        "xiaomi-token-plan-sgp",
        "Xiaomi Token Plan SGP",
        Some("https://token-plan-sgp.xiaomimimo.com/v1"),
        "Xiaomi Token Plan SGP API key",
        "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
    ),
    spec(
        "zai",
        "Z.AI",
        Some("https://api.z.ai/api/coding/paas/v4"),
        "Z.AI API key",
        "ZAI_API_KEY",
    ),
    spec(
        "zai-coding-cn",
        "Z.AI Coding CN",
        Some("https://open.bigmodel.cn/api/coding/paas/v4"),
        "Z.AI Coding CN API key",
        "ZAI_CODING_CN_API_KEY",
    ),
    spec(
        "qwen-token-plan",
        "Qwen Token Plan",
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"),
        "Qwen Token Plan API key",
        "QWEN_TOKEN_PLAN_API_KEY",
    ),
    spec(
        "qwen-token-plan-cn",
        "Qwen Token Plan CN",
        Some("https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1"),
        "Qwen Token Plan CN API key",
        "QWEN_TOKEN_PLAN_CN_API_KEY",
    ),
    spec(
        "qwen-token-plan-individual",
        "Qwen Token Plan Individual",
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"),
        "Qwen Token Plan Individual API key",
        "QWEN_TOKEN_PLAN_API_KEY",
    ),
];

const fn spec(
    id: &'static str,
    name: &'static str,
    base_url: Option<&'static str>,
    auth_name: &'static str,
    env_var: &'static str,
) -> UniformSpec {
    UniformSpec {
        id,
        name,
        base_url,
        auth_name,
        env_var,
    }
}

fn uniform_provider(
    spec: &UniformSpec,
    api: fn() -> Arc<dyn ProviderStreams>,
) -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: spec.id.to_string(),
        organization_id: organization_for_provider(spec.id).map(str::to_string),
        name: Some(spec.name.to_string()),
        base_url: spec.base_url.map(str::to_string),
        headers: None,
        auth: ProviderAuth::api_key(env_api_key_auth(spec.auth_name, &[spec.env_var])),
        models: models_for_provider(spec.id),
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::Single(api()),
    })
}

fn uniform_completion_spec(id: &str) -> &'static UniformSpec {
    UNIFORM_OPENAI_COMPLETIONS
        .iter()
        .find(|spec| spec.id == id)
        .expect("uniform openai-completions spec is registered")
}

fn uniform_other_spec(id: &str) -> (&'static UniformSpec, fn() -> Arc<dyn ProviderStreams>) {
    for (spec, api) in UNIFORM_OTHER {
        if spec.id == id {
            return (spec, *api);
        }
    }
    panic!("uniform api-specific spec is registered")
}

/// Port of the `radiusProvider` re-export from `all.ts`.
pub use crate::ai::providers::radius::radius_provider;

/// Port of `antLingProvider`.
pub fn ant_ling_provider() -> Arc<dyn Provider> {
    uniform_provider(uniform_completion_spec("ant-ling"), openai_completions_api)
}

/// Port of `basetenProvider`.
pub fn baseten_provider() -> Arc<dyn Provider> {
    uniform_provider(uniform_completion_spec("baseten"), openai_completions_api)
}

/// Port of `cerebrasProvider`.
pub fn cerebras_provider() -> Arc<dyn Provider> {
    uniform_provider(uniform_completion_spec("cerebras"), openai_completions_api)
}

/// Port of `deepseekProvider`.
pub fn deepseek_provider() -> Arc<dyn Provider> {
    uniform_provider(uniform_completion_spec("deepseek"), openai_completions_api)
}

/// Port of `groqProvider`.
pub fn groq_provider() -> Arc<dyn Provider> {
    uniform_provider(uniform_completion_spec("groq"), openai_completions_api)
}

/// Port of `huggingfaceProvider`.
pub fn huggingface_provider() -> Arc<dyn Provider> {
    uniform_provider(
        uniform_completion_spec("huggingface"),
        openai_completions_api,
    )
}

/// Port of `moonshotaiProvider`.
pub fn moonshotai_provider() -> Arc<dyn Provider> {
    uniform_provider(
        uniform_completion_spec("moonshotai"),
        openai_completions_api,
    )
}

/// Port of `moonshotaiCnProvider`.
pub fn moonshotai_cn_provider() -> Arc<dyn Provider> {
    uniform_provider(
        uniform_completion_spec("moonshotai-cn"),
        openai_completions_api,
    )
}

/// Port of `nvidiaProvider`.
pub fn nvidia_provider() -> Arc<dyn Provider> {
    uniform_provider(uniform_completion_spec("nvidia"), openai_completions_api)
}

/// Port of `togetherProvider`.
pub fn together_provider() -> Arc<dyn Provider> {
    uniform_provider(uniform_completion_spec("together"), openai_completions_api)
}

/// Port of `xiaomiProvider`.
pub fn xiaomi_provider() -> Arc<dyn Provider> {
    uniform_provider(uniform_completion_spec("xiaomi"), openai_completions_api)
}

/// Port of `xiaomiTokenPlanAmsProvider`.
pub fn xiaomi_token_plan_ams_provider() -> Arc<dyn Provider> {
    uniform_provider(
        uniform_completion_spec("xiaomi-token-plan-ams"),
        openai_completions_api,
    )
}

/// Port of `xiaomiTokenPlanCnProvider`.
pub fn xiaomi_token_plan_cn_provider() -> Arc<dyn Provider> {
    uniform_provider(
        uniform_completion_spec("xiaomi-token-plan-cn"),
        openai_completions_api,
    )
}

/// Port of `xiaomiTokenPlanSgpProvider`.
pub fn xiaomi_token_plan_sgp_provider() -> Arc<dyn Provider> {
    uniform_provider(
        uniform_completion_spec("xiaomi-token-plan-sgp"),
        openai_completions_api,
    )
}

/// Port of `zaiProvider`.
pub fn zai_provider() -> Arc<dyn Provider> {
    uniform_provider(uniform_completion_spec("zai"), openai_completions_api)
}

/// Port of `zaiCodingCnProvider`.
pub fn zai_coding_cn_provider() -> Arc<dyn Provider> {
    uniform_provider(
        uniform_completion_spec("zai-coding-cn"),
        openai_completions_api,
    )
}

/// Port of `qwenTokenPlanProvider`.
pub fn qwen_token_plan_provider() -> Arc<dyn Provider> {
    uniform_provider(
        uniform_completion_spec("qwen-token-plan"),
        openai_completions_api,
    )
}

/// Port of `qwenTokenPlanCnProvider`.
pub fn qwen_token_plan_cn_provider() -> Arc<dyn Provider> {
    uniform_provider(
        uniform_completion_spec("qwen-token-plan-cn"),
        openai_completions_api,
    )
}

/// Port of `qwenTokenPlanIndividualProvider`.
pub fn qwen_token_plan_individual_provider() -> Arc<dyn Provider> {
    uniform_provider(
        uniform_completion_spec("qwen-token-plan-individual"),
        openai_completions_api,
    )
}

/// Port of `openaiProvider`.
pub fn openai_provider() -> Arc<dyn Provider> {
    let (spec, api) = uniform_other_spec("openai");
    uniform_provider(spec, api)
}

/// Port of `azureOpenAIResponsesProvider`.
pub fn azure_openai_responses_provider() -> Arc<dyn Provider> {
    let (spec, api) = uniform_other_spec("azure-openai-responses");
    uniform_provider(spec, api)
}

/// Port of `mistralProvider`.
pub fn mistral_provider() -> Arc<dyn Provider> {
    let (spec, api) = uniform_other_spec("mistral");
    uniform_provider(spec, api)
}

/// Port of `minimaxProvider`.
pub fn minimax_provider() -> Arc<dyn Provider> {
    let (spec, api) = uniform_other_spec("minimax");
    uniform_provider(spec, api)
}

/// Port of `minimaxCnProvider`.
pub fn minimax_cn_provider() -> Arc<dyn Provider> {
    let (spec, api) = uniform_other_spec("minimax-cn");
    uniform_provider(spec, api)
}

/// Port of `vercelAIGatewayProvider`.
pub fn vercel_ai_gateway_provider() -> Arc<dyn Provider> {
    let (spec, api) = uniform_other_spec("vercel-ai-gateway");
    uniform_provider(spec, api)
}

/// Port of `googleProvider`.
pub fn google_provider() -> Arc<dyn Provider> {
    let (spec, api) = uniform_other_spec("google");
    uniform_provider(spec, api)
}

/// Builds an api-keyed map for `ProviderApi::ByApi`.
fn by_api(entries: &[(&str, Arc<dyn ProviderStreams>)]) -> ProviderApi {
    ProviderApi::ByApi(
        entries
            .iter()
            .map(|(api, streams)| (api.to_string(), Arc::clone(streams)))
            .collect::<BTreeMap<String, Arc<dyn ProviderStreams>>>(),
    )
}

/// Port of `fireworksProvider`.
pub fn fireworks_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "fireworks".to_string(),
        organization_id: organization_for_provider("fireworks").map(str::to_string),
        name: Some("Fireworks".to_string()),
        base_url: Some("https://api.fireworks.ai/inference".to_string()),
        headers: None,
        auth: ProviderAuth::api_key(env_api_key_auth(
            "Fireworks API key",
            &["FIREWORKS_API_KEY"],
        )),
        models: models_for_provider("fireworks"),
        fetch_models: None,
        filter_models: None,
        api: by_api(&[
            ("anthropic-messages", anthropic_messages_api()),
            ("openai-completions", openai_completions_api()),
        ]),
    })
}

/// Port of `githubCopilotProvider`.
pub fn github_copilot_provider() -> Arc<dyn Provider> {
    // OAuth credentials may carry the model ids the account can actually
    // use (extra.availableModelIds); only a valid string array filters.
    let filter_models: FilterModelsFn = Arc::new(
        |models: Vec<Model>, credential: Option<&Credential>| match credential {
            Some(Credential::OAuth(oauth)) => {
                let valid_ids: Option<Vec<String>> = oauth
                    .extra
                    .get("availableModelIds")
                    .and_then(|value| value.as_array())
                    .and_then(|ids| {
                        ids.iter()
                            .map(|id| id.as_str().map(str::to_string))
                            .collect::<Option<Vec<_>>>()
                    });
                match valid_ids {
                    Some(available) => models
                        .into_iter()
                        .filter(|model| available.contains(&model.id))
                        .collect(),
                    None => models,
                }
            }
            _ => models,
        },
    );
    create_provider(CreateProviderOptions {
        id: "github-copilot".to_string(),
        organization_id: organization_for_provider("github-copilot").map(str::to_string),
        name: Some("GitHub Copilot".to_string()),
        base_url: Some("https://api.individual.githubcopilot.com".to_string()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth(
                "GitHub Copilot token",
                &["COPILOT_GITHUB_TOKEN"],
            )),
            oauth: Some(crate::ai::auth::oauth::load::load_github_copilot_oauth()),
        },
        models: models_for_provider("github-copilot"),
        fetch_models: None,
        filter_models: Some(filter_models),
        api: by_api(&[
            ("anthropic-messages", anthropic_messages_api()),
            ("openai-completions", openai_completions_api()),
            ("openai-responses", openai_responses_api()),
        ]),
    })
}

/// Port of `opencodeProvider`.
pub fn opencode_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "opencode".to_string(),
        organization_id: organization_for_provider("opencode").map(str::to_string),
        name: Some("OpenCode Zen".to_string()),
        base_url: None,
        headers: None,
        auth: ProviderAuth::api_key(env_api_key_auth("OpenCode API key", &["OPENCODE_API_KEY"])),
        models: models_for_provider("opencode"),
        fetch_models: None,
        filter_models: None,
        api: by_api(&[
            ("anthropic-messages", anthropic_messages_api()),
            ("google-generative-ai", google_generative_ai_api()),
            ("openai-completions", openai_completions_api()),
            ("openai-responses", openai_responses_api()),
        ]),
    })
}

/// Port of `opencodeGoProvider`.
pub fn opencode_go_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "opencode-go".to_string(),
        organization_id: organization_for_provider("opencode-go").map(str::to_string),
        name: Some("OpenCode Go".to_string()),
        base_url: None,
        headers: None,
        auth: ProviderAuth::api_key(env_api_key_auth("OpenCode API key", &["OPENCODE_API_KEY"])),
        models: models_for_provider("opencode-go"),
        fetch_models: None,
        filter_models: None,
        api: by_api(&[
            ("anthropic-messages", anthropic_messages_api()),
            ("openai-completions", openai_completions_api()),
            ("openai-responses", openai_responses_api()),
        ]),
    })
}

/// Port of `openrouterProvider`.
pub fn openrouter_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "openrouter".to_string(),
        organization_id: organization_for_provider("openrouter").map(str::to_string),
        name: Some("OpenRouter".to_string()),
        base_url: Some("https://openrouter.ai/api/v1".to_string()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth(
                "OpenRouter API key",
                &["OPENROUTER_API_KEY"],
            )),
            oauth: Some(load_open_router_oauth()),
        },
        models: models_for_provider("openrouter"),
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::Single(openai_completions_api()),
    })
}

/// Port of `xaiProvider`.
pub fn xai_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "xai".to_string(),
        organization_id: organization_for_provider("xai").map(str::to_string),
        name: Some("xAI".to_string()),
        base_url: Some("https://api.x.ai/v1".to_string()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth("xAI API key", &["XAI_API_KEY"])),
            oauth: Some(load_xai_oauth()),
        },
        models: models_for_provider("xai"),
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::Single(openai_responses_api()),
    })
}

/// Port of `kimiCodingProvider`.
pub fn kimi_coding_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "kimi-coding".to_string(),
        organization_id: organization_for_provider("kimi-coding").map(str::to_string),
        name: Some("Kimi For Coding".to_string()),
        base_url: Some("https://api.kimi.com/coding".to_string()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth("Kimi API key", &["KIMI_API_KEY"])),
            oauth: Some(load_kimi_coding_oauth()),
        },
        models: models_for_provider("kimi-coding"),
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::Single(anthropic_messages_api()),
    })
}

/// Port of `openaiCodexProvider` (OAuth-only auth).
pub fn openai_codex_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "openai-codex".to_string(),
        organization_id: organization_for_provider("openai-codex").map(str::to_string),
        name: Some("OpenAI Codex".to_string()),
        base_url: Some("https://chatgpt.com/backend-api".to_string()),
        headers: None,
        auth: ProviderAuth::oauth(load_openai_codex_oauth()),
        models: models_for_provider("openai-codex"),
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::Single(openai_codex_responses_api()),
    })
}

/// The remaining single-API uniform providers that do not target
/// openai-completions.
type UniformApiFactory = (UniformSpec, fn() -> Arc<dyn ProviderStreams>);

const UNIFORM_OTHER: &[UniformApiFactory] = &[
    (
        spec(
            "openai",
            "OpenAI",
            Some("https://api.openai.com/v1"),
            "OpenAI API key",
            "OPENAI_API_KEY",
        ),
        openai_responses_api,
    ),
    (
        spec(
            "mistral",
            "Mistral",
            Some("https://api.mistral.ai"),
            "Mistral API key",
            "MISTRAL_API_KEY",
        ),
        mistral_conversations_api,
    ),
    (
        spec(
            "minimax",
            "MiniMax",
            Some("https://api.minimax.io/anthropic"),
            "MiniMax API key",
            "MINIMAX_API_KEY",
        ),
        anthropic_messages_api,
    ),
    (
        spec(
            "minimax-cn",
            "MiniMax CN",
            Some("https://api.minimaxi.com/anthropic"),
            "MiniMax CN API key",
            "MINIMAX_CN_API_KEY",
        ),
        anthropic_messages_api,
    ),
    (
        spec(
            "vercel-ai-gateway",
            "Vercel AI Gateway",
            Some("https://ai-gateway.vercel.sh"),
            "Vercel AI Gateway API key",
            "AI_GATEWAY_API_KEY",
        ),
        anthropic_messages_api,
    ),
    (
        spec(
            "google",
            "Google",
            Some("https://generativelanguage.googleapis.com/v1beta"),
            "Gemini API key",
            "GEMINI_API_KEY",
        ),
        google_generative_ai_api,
    ),
    (
        spec(
            "azure-openai-responses",
            "Azure OpenAI",
            None,
            "Azure OpenAI API key",
            "AZURE_OPENAI_API_KEY",
        ),
        azure_openai_responses_api,
    ),
];

/// All built-in providers, freshly constructed — port of `builtinProviders`
/// in the all.ts order (minus `amazonBedrockProvider` and `radiusProvider`,
/// which land with their modules).
pub fn builtin_providers() -> Vec<Arc<dyn Provider>> {
    let mut providers: Vec<Arc<dyn Provider>> =
        vec![crate::ai::providers::amazon_bedrock::amazon_bedrock_provider()];
    providers.extend(
        UNIFORM_OPENAI_COMPLETIONS
            .iter()
            .map(|spec| uniform_provider(spec, openai_completions_api))
            .chain(
                UNIFORM_OTHER
                    .iter()
                    .map(|(spec, api)| uniform_provider(spec, *api)),
            )
            .collect::<Vec<_>>(),
    );
    providers.push(fireworks_provider());
    providers.push(github_copilot_provider());
    providers.push(opencode_provider());
    providers.push(opencode_go_provider());
    providers.push(openrouter_provider());
    providers.push(xai_provider());
    providers.push(kimi_coding_provider());
    providers.push(openai_codex_provider());
    providers.push(anthropic_provider());
    providers.push(google_vertex_provider());
    providers.push(cloudflare_ai_gateway_provider());
    providers.push(cloudflare_workers_ai_provider());
    providers.push(crate::ai::providers::radius::radius_provider(
        crate::ai::providers::radius::RadiusProviderOptions::default(),
    ));
    // all.ts lists the providers in id order.
    providers.sort_by(|a, b| a.id().cmp(b.id()));
    providers
}

/// A `Models` collection with every built-in provider registered — port of
/// `builtinModels`.
pub fn builtin_models(options: CreateModelsOptions) -> Arc<Models> {
    let models = Arc::new(Models::new(options));
    for provider in builtin_providers() {
        models.set_provider(provider);
    }
    models
}

/// Typed read of the generated built-in catalog — port of `getBuiltinModel`.
pub fn get_builtin_model(provider: &str, model_id: &str) -> Option<Model> {
    models().get(provider)?.get(model_id).cloned()
}

/// Providers present in the generated catalog — port of `getBuiltinProviders`.
pub fn get_builtin_providers() -> Vec<String> {
    provider_ids()
}

/// The generated model list for one provider — port of `getBuiltinModels`.
pub fn get_builtin_models(provider: &str) -> Vec<Model> {
    models_for_provider(provider)
}

/// Generation timestamp shared by all built-in catalogs — port of
/// `getBuiltinModelDataGeneratedAt`.
pub fn get_builtin_model_data_generated_at() -> Option<i64> {
    generated_at()
}

/// The openrouter-images API adapter behind the images provider.
struct OpenRouterImagesApi;

impl ProviderImages for OpenRouterImagesApi {
    fn generate_images<'a>(
        &'a self,
        model: &'a ImagesModel,
        context: &'a ImagesContext,
        options: Option<&'a ImagesOptions>,
    ) -> futures::future::BoxFuture<'a, crate::ai::types::AssistantImages> {
        Box::pin(crate::ai::api::openrouter_images::generate_images(
            model, context, options,
        ))
    }
}

/// Port of `openrouterImagesProvider`.
pub fn openrouter_images_provider() -> Arc<dyn ImagesProvider> {
    create_images_provider(CreateImagesProviderOptions {
        id: "openrouter".to_string(),
        name: Some("OpenRouter".to_string()),
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth(
                "OpenRouter API key",
                &["OPENROUTER_API_KEY"],
            )),
            oauth: Some(load_open_router_oauth()),
        },
        models: image_models_for_provider("openrouter"),
        refresh_models: None,
        api: Arc::new(OpenRouterImagesApi),
    })
}

/// All built-in image-generation providers — port of `builtinImagesProviders`.
pub fn builtin_images_providers() -> Vec<Arc<dyn ImagesProvider>> {
    vec![openrouter_images_provider()]
}

/// An `ImagesModels` collection with every built-in image-generation
/// provider registered — port of `builtinImagesModels`.
pub fn builtin_images_models(options: CreateModelsOptions) -> ImagesModels {
    let models = create_images_models(options);
    for provider in builtin_images_providers() {
        models.set_provider(provider);
    }
    models
}
