//! Port of `pi-core/ai/test/supports-xhigh.test.ts` plus the catalog cases
//! from `pi-core/ai/test/max-thinking.test.ts` ("exposes xhigh and max for
//! openai-codex/%s"). Every assertion is a pure built-in catalog lookup —
//! the TS `getModel(provider, id)` reads become
//! `pi_core::ai::providers::builtin::get_builtin_model`.

use pi_core::ai::models::get_supported_thinking_levels;
use pi_core::ai::providers::builtin::get_builtin_model;
use pi_core::ai::types::{Model, ModelThinkingLevel};

/// `getModel(...)` + `expect(model).toBeDefined()`.
fn catalog_model(provider: &str, model_id: &str) -> Model {
    get_builtin_model(provider, model_id)
        .unwrap_or_else(|| panic!("model should be defined: {provider}/{model_id}"))
}

fn levels(provider: &str, model_id: &str) -> Vec<ModelThinkingLevel> {
    get_supported_thinking_levels(&catalog_model(provider, model_id))
}

#[test]
fn includes_max_but_not_xhigh_for_anthropic_opus_4_6_on_anthropic_messages_api() {
    let levels = levels("anthropic", "claude-opus-4-6");
    assert!(levels.contains(&ModelThinkingLevel::Max));
    assert!(!levels.contains(&ModelThinkingLevel::Xhigh));
}

#[test]
fn includes_xhigh_and_max_for_anthropic_opus_4_8_on_anthropic_messages_api() {
    let levels = levels("anthropic", "claude-opus-4-8");
    assert!(levels.contains(&ModelThinkingLevel::Xhigh));
    assert!(levels.contains(&ModelThinkingLevel::Max));
}

#[test]
fn includes_xhigh_and_max_for_anthropic_opus_5_on_anthropic_messages_api() {
    let levels = levels("anthropic", "claude-opus-5");
    assert!(levels.contains(&ModelThinkingLevel::Xhigh));
    assert!(levels.contains(&ModelThinkingLevel::Max));
}

#[test]
fn includes_max_but_not_xhigh_for_anthropic_sonnet_4_6_on_anthropic_messages_api() {
    let levels = levels("anthropic", "claude-sonnet-4-6");
    assert!(levels.contains(&ModelThinkingLevel::Max));
    assert!(!levels.contains(&ModelThinkingLevel::Xhigh));
}

#[test]
fn includes_xhigh_and_max_for_anthropic_sonnet_5_on_anthropic_messages_api() {
    let levels = levels("anthropic", "claude-sonnet-5");
    assert!(levels.contains(&ModelThinkingLevel::Xhigh));
    assert!(levels.contains(&ModelThinkingLevel::Max));
}

#[test]
fn includes_xhigh_and_max_but_not_off_for_anthropic_claude_fable_5_on_anthropic_messages_api() {
    let levels = levels("anthropic", "claude-fable-5");
    assert!(levels.contains(&ModelThinkingLevel::Xhigh));
    assert!(levels.contains(&ModelThinkingLevel::Max));
    assert!(!levels.contains(&ModelThinkingLevel::Off));
}

#[test]
fn does_not_include_xhigh_or_max_for_claude_sonnet_4_5() {
    let levels = levels("anthropic", "claude-sonnet-4-5");
    assert!(!levels.contains(&ModelThinkingLevel::Xhigh));
    assert!(!levels.contains(&ModelThinkingLevel::Max));
}

#[test]
fn includes_xhigh_for_openai_codex_models() {
    // it.each(["gpt-5.4", "gpt-5.5", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"])
    for model_id in [
        "gpt-5.4",
        "gpt-5.5",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
    ] {
        let levels = levels("openai-codex", model_id);
        assert!(
            levels.contains(&ModelThinkingLevel::Xhigh),
            "{model_id} should include xhigh"
        );
    }
}

#[test]
fn includes_xhigh_and_max_for_openai_models() {
    // it.each(["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"])
    for model_id in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
        assert_eq!(
            levels("openai", model_id),
            vec![
                ModelThinkingLevel::Off,
                ModelThinkingLevel::Low,
                ModelThinkingLevel::Medium,
                ModelThinkingLevel::High,
                ModelThinkingLevel::Xhigh,
                ModelThinkingLevel::Max,
            ],
            "unexpected levels for openai/{model_id}"
        );
    }
}

#[test]
fn includes_only_medium_high_xhigh_for_openai_gpt_5_5_pro() {
    assert_eq!(
        levels("openai", "gpt-5.5-pro"),
        vec![
            ModelThinkingLevel::Medium,
            ModelThinkingLevel::High,
            ModelThinkingLevel::Xhigh,
        ]
    );
}

#[test]
fn includes_only_medium_high_xhigh_for_openrouter_gpt_5_5_pro() {
    assert_eq!(
        levels("openrouter", "openai/gpt-5.5-pro"),
        vec![
            ModelThinkingLevel::Medium,
            ModelThinkingLevel::High,
            ModelThinkingLevel::Xhigh,
        ]
    );
}

#[test]
fn includes_low_high_max_plus_off_for_deepseek_v4_flash_on_the_deepseek_provider() {
    assert_eq!(
        levels("deepseek", "deepseek-v4-flash"),
        vec![
            ModelThinkingLevel::Off,
            ModelThinkingLevel::Low,
            ModelThinkingLevel::High,
            ModelThinkingLevel::Max,
        ]
    );
}

#[test]
fn includes_low_high_max_plus_off_for_deepseek_v4_flash_on_opencode_go() {
    assert_eq!(
        levels("opencode-go", "deepseek-v4-flash"),
        vec![
            ModelThinkingLevel::Off,
            ModelThinkingLevel::Low,
            ModelThinkingLevel::High,
            ModelThinkingLevel::Max,
        ]
    );
}

#[test]
fn includes_only_high_plus_off_for_opencode_go_kimi_k2_6() {
    assert_eq!(
        levels("opencode-go", "kimi-k2.6"),
        vec![ModelThinkingLevel::Off, ModelThinkingLevel::High]
    );
}

#[test]
fn excludes_thinking_off_for_moonshot_kimi_k2_7_code_models() {
    let cases = [
        catalog_model("moonshotai", "kimi-k2.7-code"),
        catalog_model("moonshotai-cn", "kimi-k2.7-code"),
    ];

    for model in &cases {
        assert_eq!(
            get_supported_thinking_levels(model),
            vec![
                ModelThinkingLevel::Minimal,
                ModelThinkingLevel::Low,
                ModelThinkingLevel::Medium,
                ModelThinkingLevel::High,
            ]
        );
    }
}

#[test]
fn uses_the_verified_effort_options_for_kimi_k3() {
    // it.each(["moonshotai", "moonshotai-cn"])
    for provider in ["moonshotai", "moonshotai-cn"] {
        assert_eq!(
            levels(provider, "kimi-k3"),
            vec![
                ModelThinkingLevel::Low,
                ModelThinkingLevel::High,
                ModelThinkingLevel::Max,
            ],
            "unexpected levels for {provider}/kimi-k3"
        );
    }
}

#[test]
fn includes_only_low_high_max_for_kimi_coding_k3() {
    assert_eq!(
        levels("kimi-coding", "k3"),
        vec![
            ModelThinkingLevel::Low,
            ModelThinkingLevel::High,
            ModelThinkingLevel::Max,
        ]
    );
}

#[test]
fn includes_only_high_for_opencode_grok_build() {
    assert_eq!(
        levels("opencode", "grok-build-0.1"),
        vec![ModelThinkingLevel::High]
    );
}

#[test]
fn includes_only_high_xhigh_plus_off_for_deepseek_v4_flash_on_openrouter() {
    assert_eq!(
        levels("openrouter", "deepseek/deepseek-v4-flash"),
        vec![
            ModelThinkingLevel::Off,
            ModelThinkingLevel::High,
            ModelThinkingLevel::Xhigh,
        ]
    );
}

#[test]
fn includes_max_but_not_xhigh_for_openrouter_opus_4_6_openai_completions_api() {
    let levels = levels("openrouter", "anthropic/claude-opus-4.6");
    assert!(levels.contains(&ModelThinkingLevel::Max));
    assert!(!levels.contains(&ModelThinkingLevel::Xhigh));
}

#[test]
fn includes_xhigh_and_max_for_bedrock_claude_opus_5() {
    let levels = levels("amazon-bedrock", "global.anthropic.claude-opus-5");
    assert!(levels.contains(&ModelThinkingLevel::Xhigh));
    assert!(levels.contains(&ModelThinkingLevel::Max));
}

#[test]
fn includes_xhigh_but_not_off_or_max_for_xai_grok_4_6() {
    assert_eq!(
        levels("xai", "grok-4.6"),
        vec![
            ModelThinkingLevel::Low,
            ModelThinkingLevel::Medium,
            ModelThinkingLevel::High,
            ModelThinkingLevel::Xhigh,
        ]
    );
}

#[test]
fn includes_xhigh_and_max_but_not_off_for_bedrock_claude_fable_5() {
    let levels = levels("amazon-bedrock", "global.anthropic.claude-fable-5");
    assert!(levels.contains(&ModelThinkingLevel::Xhigh));
    assert!(levels.contains(&ModelThinkingLevel::Max));
    assert!(!levels.contains(&ModelThinkingLevel::Off));
}

// ---------------------------------------------------------------------------
// From `pi-core/ai/test/max-thinking.test.ts`: the openai-codex catalog
// cases (the "sends max to the Codex Responses API" payload case lives in
// tests/ai_codex_stream.rs).

#[test]
fn exposes_xhigh_and_max_for_openai_codex_models() {
    // it.each(["gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.6-terra"])
    for model_id in ["gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.6-terra"] {
        let model = catalog_model("openai-codex", model_id);
        // toMatchObject({ xhigh: "xhigh", max: "max" })
        let map = model
            .thinking_level_map
            .as_ref()
            .unwrap_or_else(|| panic!("thinkingLevelMap should be defined for {model_id}"));
        assert_eq!(
            map.get(&ModelThinkingLevel::Xhigh),
            Some(&Some("xhigh".to_string())),
            "unexpected xhigh mapping for {model_id}"
        );
        assert_eq!(
            map.get(&ModelThinkingLevel::Max),
            Some(&Some("max".to_string())),
            "unexpected max mapping for {model_id}"
        );
        assert_eq!(
            get_supported_thinking_levels(&model),
            vec![
                ModelThinkingLevel::Off,
                ModelThinkingLevel::Minimal,
                ModelThinkingLevel::Low,
                ModelThinkingLevel::Medium,
                ModelThinkingLevel::High,
                ModelThinkingLevel::Xhigh,
                ModelThinkingLevel::Max,
            ],
            "unexpected levels for openai-codex/{model_id}"
        );
    }
}
