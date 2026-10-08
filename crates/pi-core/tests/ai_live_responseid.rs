//! Rust entry point for the credential-gated live suite
//! `pi-core/ai/test/responseid.test.ts` ("responseId E2E Tests").
//!
//! 11 TS `it` cases share the `expectResponseId` body: a one-shot request
//! whose response must not error and must expose a truthy string
//! `responseId`. Gates translated verbatim (env vars, the azure helper,
//! `resolveApiKey` for Copilot/Codex, and the per-it Google Vertex
//! ADC/API-key gates). Without credentials each case prints
//! `SKIP: <case> requires <ENV>` and the test passes.
//!
//! Deviation: vitest `{ retry: 3, timeout: 30000 }` has no Rust equivalent.

mod common;

use common::live::{
    self, LiveOptions, as_openai_completions, get_model_or_panic, live_complete, live_env,
    now_millis, skip,
};
use pi_core::ai::types::{Context, Model, RoleUser, StopReason, UserContent, UserMessage};

fn user_message(content: &str) -> pi_core::ai::types::Message {
    pi_core::ai::types::Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(content.to_string()),
        timestamp: now_millis(),
    })
}

/// expectResponseId from responseid.test.ts.
async fn expect_response_id(model: &Model, case: &str, options: &LiveOptions) {
    let context = Context {
        system_prompt: Some("You are a helpful assistant. Be concise.".to_string()),
        messages: vec![user_message("Reply with exactly: response id test")],
        tools: None,
    };

    let response = live_complete(model, &context, options).await;

    assert_ne!(
        response.stop_reason,
        StopReason::Error,
        "{case}: stopReason, error: {:?}",
        response.error_message
    );
    assert!(response.response_id.is_some(), "{case}: responseId");
}

#[tokio::test]
async fn response_id_env_provider_matrix() {
    let suites = [
        (
            "Google Provider: should expose responseId",
            "google",
            "gemini-2.5-flash",
            "GEMINI_API_KEY",
        ),
        (
            "OpenAI Completions Provider: should expose responseId",
            "openai",
            "gpt-4o-mini",
            "OPENAI_API_KEY",
        ),
        (
            "OpenAI Responses Provider: should expose responseId",
            "openai",
            "gpt-5-mini",
            "OPENAI_API_KEY",
        ),
        (
            "Anthropic Provider: should expose responseId",
            "anthropic",
            "claude-sonnet-4-5",
            "ANTHROPIC_API_KEY",
        ),
        (
            "Azure OpenAI Responses Provider: should expose responseId",
            "azure-openai-responses",
            "gpt-4o-mini",
            "AZURE_OPENAI_API_KEY + AZURE_OPENAI_BASE_URL|AZURE_OPENAI_RESOURCE_NAME",
        ),
        (
            "Mistral Provider: should expose responseId",
            "mistral",
            "devstral-medium-latest",
            "MISTRAL_API_KEY",
        ),
    ];

    for (case, provider, model_id, env) in suites {
        let gated = if provider == "azure-openai-responses" {
            live::has_azure_openai_credentials()
        } else {
            live_env(env).is_some()
        };
        if !gated {
            skip(case, env);
            continue;
        }
        let model = get_model_or_panic(provider, model_id);
        let model = if provider == "openai" && model_id == "gpt-4o-mini" {
            as_openai_completions(&model)
        } else {
            model
        };
        let mut options = LiveOptions::default();
        if provider == "azure-openai-responses" {
            options.azure_deployment_name = live::resolve_azure_deployment_name(&model.id);
        }
        expect_response_id(&model, case, &options).await;
    }
}

#[tokio::test]
async fn response_id_google_vertex() {
    // TS: describe("Google Vertex Provider") with per-it skipIf gates.
    let llm = get_model_or_panic("google-vertex", "gemini-3-flash-preview");
    let vertex_project = live_env("GOOGLE_CLOUD_PROJECT").or_else(|| live_env("GCLOUD_PROJECT"));
    let vertex_location = live_env("GOOGLE_CLOUD_LOCATION");
    let vertex_api_key = live_env("GOOGLE_CLOUD_API_KEY");
    let is_vertex_configured = vertex_project.is_some() && vertex_location.is_some();

    if !is_vertex_configured {
        skip(
            "Google Vertex Provider: should expose responseId with ADC",
            "GOOGLE_CLOUD_PROJECT|GCLOUD_PROJECT + GOOGLE_CLOUD_LOCATION",
        );
    } else {
        let options = LiveOptions {
            project: vertex_project.clone(),
            location: vertex_location.clone(),
            ..Default::default()
        };
        expect_response_id(
            &llm,
            "Google Vertex Provider: should expose responseId with ADC",
            &options,
        )
        .await;
    }

    if vertex_api_key.is_none() {
        skip(
            "Google Vertex Provider: should expose responseId with API key",
            "GOOGLE_CLOUD_API_KEY",
        );
    } else {
        let options = LiveOptions::with_api_key(vertex_api_key.as_deref().unwrap_or_default());
        expect_response_id(
            &llm,
            "Google Vertex Provider: should expose responseId with API key",
            &options,
        )
        .await;
    }
}

#[tokio::test]
async fn response_id_oauth_providers() {
    // GitHub Copilot: OpenAI path and Anthropic path.
    let copilot_token = live::resolve_api_key("github-copilot").await;
    if copilot_token.is_none() {
        skip(
            "GitHub Copilot Provider: responseId (OpenAI + Anthropic paths)",
            "~/.pi/agent/auth.json github-copilot credentials",
        );
    } else {
        let options = LiveOptions::with_api_key(copilot_token.as_deref().unwrap_or_default());
        let llm = get_model_or_panic("github-copilot", "gpt-5.3-codex");
        expect_response_id(
            &llm,
            "GitHub Copilot Provider: OpenAI path should expose responseId",
            &options,
        )
        .await;
        let llm = get_model_or_panic("github-copilot", "claude-sonnet-4.6");
        expect_response_id(
            &llm,
            "GitHub Copilot Provider: Anthropic path should expose responseId",
            &options,
        )
        .await;
    }

    // OpenAI Codex.
    let codex_token = live::resolve_api_key("openai-codex").await;
    if codex_token.is_none() {
        skip(
            "OpenAI Codex Provider: should expose responseId",
            "~/.pi/agent/auth.json openai-codex credentials",
        );
    } else {
        let llm = get_model_or_panic("openai-codex", "gpt-5.5");
        let options = LiveOptions::with_api_key(codex_token.as_deref().unwrap_or_default());
        expect_response_id(
            &llm,
            "OpenAI Codex Provider: should expose responseId",
            &options,
        )
        .await;
    }
}
