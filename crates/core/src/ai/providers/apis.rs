//! Port of the `api/*.lazy.ts` `ProviderStreams` values: each adapter binds
//! a statically linked API implementation module. The TypeScript lazy-loading
//! layer collapses to direct dispatch in Rust.

use std::sync::Arc;

use crate::ai::api::{
    anthropic_messages, azure_openai_responses, google_generative_ai, google_vertex,
    mistral_conversations, openai_codex_responses, openai_completions, openai_responses,
    pi_messages,
};
use crate::ai::models::ProviderStreams;
use crate::ai::types::{Context, Model, SimpleStreamOptions, StreamOptions};
use crate::ai::utils::event_stream::AssistantMessageEventStream;

struct AnthropicMessagesApi;

impl ProviderStreams for AnthropicMessagesApi {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let typed = options.map(|options| anthropic_messages::AnthropicOptions {
            base: options.clone(),
            ..Default::default()
        });
        anthropic_messages::stream(model, context, typed.as_ref())
    }
    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        anthropic_messages::stream_simple(model, context, options)
    }
}

struct OpenAICompletionsApi;

impl ProviderStreams for OpenAICompletionsApi {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let typed = options.map(|options| openai_completions::OpenAICompletionsOptions {
            base: options.clone(),
            ..Default::default()
        });
        openai_completions::stream(model, context, typed.as_ref())
    }
    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        openai_completions::stream_simple(model, context, options)
    }
}

struct OpenAIResponsesApi;

impl ProviderStreams for OpenAIResponsesApi {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let typed = options.map(|options| openai_responses::OpenAIResponsesOptions {
            base: options.clone(),
            ..Default::default()
        });
        openai_responses::stream(model, context, typed.as_ref())
    }
    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        openai_responses::stream_simple(model, context, options)
    }
}

struct AzureOpenAIResponsesApi;

impl ProviderStreams for AzureOpenAIResponsesApi {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let typed = options.map(
            |options| azure_openai_responses::AzureOpenAIResponsesOptions {
                base: options.clone(),
                ..Default::default()
            },
        );
        azure_openai_responses::stream(model, context, typed.as_ref())
    }
    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        azure_openai_responses::stream_simple(model, context, options)
    }
}

struct GoogleGenerativeAIApi;

impl ProviderStreams for GoogleGenerativeAIApi {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let typed = options.map(|options| google_generative_ai::GoogleOptions {
            base: options.clone(),
            ..Default::default()
        });
        google_generative_ai::stream(model, context, typed.as_ref())
    }
    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        google_generative_ai::stream_simple(model, context, options)
    }
}

struct GoogleVertexApi;

impl ProviderStreams for GoogleVertexApi {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let typed = options.map(|options| google_vertex::GoogleVertexOptions {
            base: options.clone(),
            ..Default::default()
        });
        google_vertex::stream(model, context, typed.as_ref())
    }
    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        google_vertex::stream_simple(model, context, options)
    }
}

struct MistralConversationsApi;

impl ProviderStreams for MistralConversationsApi {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let typed = options.map(|options| mistral_conversations::MistralOptions {
            base: options.clone(),
            ..Default::default()
        });
        mistral_conversations::stream(model, context, typed.as_ref())
    }
    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        mistral_conversations::stream_simple(model, context, options)
    }
}

struct OpenAICodexResponsesApi;

impl ProviderStreams for OpenAICodexResponsesApi {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let typed = options.map(
            |options| openai_codex_responses::OpenAICodexResponsesOptions {
                base: options.clone(),
                ..Default::default()
            },
        );
        openai_codex_responses::stream(model, context, typed.as_ref())
    }
    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        openai_codex_responses::stream_simple(model, context, options)
    }
}

struct PiMessagesApi;

impl ProviderStreams for PiMessagesApi {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let typed = options.map(|options| pi_messages::PiMessagesOptions {
            base: options.clone(),
            ..Default::default()
        });
        pi_messages::stream(model, context, typed.as_ref())
    }
    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        pi_messages::stream_simple(model, context, options)
    }
}

pub fn anthropic_messages_api() -> Arc<dyn ProviderStreams> {
    Arc::new(AnthropicMessagesApi)
}
pub fn openai_completions_api() -> Arc<dyn ProviderStreams> {
    Arc::new(OpenAICompletionsApi)
}
pub fn openai_responses_api() -> Arc<dyn ProviderStreams> {
    Arc::new(OpenAIResponsesApi)
}
pub fn azure_openai_responses_api() -> Arc<dyn ProviderStreams> {
    Arc::new(AzureOpenAIResponsesApi)
}
pub fn google_generative_ai_api() -> Arc<dyn ProviderStreams> {
    Arc::new(GoogleGenerativeAIApi)
}
pub fn google_vertex_api() -> Arc<dyn ProviderStreams> {
    Arc::new(GoogleVertexApi)
}
pub fn mistral_conversations_api() -> Arc<dyn ProviderStreams> {
    Arc::new(MistralConversationsApi)
}
pub fn openai_codex_responses_api() -> Arc<dyn ProviderStreams> {
    Arc::new(OpenAICodexResponsesApi)
}
pub fn pi_messages_api() -> Arc<dyn ProviderStreams> {
    Arc::new(PiMessagesApi)
}

/// Port of `bedrockConverseStreamApi` (the lazy wrapper collapses to direct
/// dispatch).
pub fn bedrock_converse_stream_api() -> Arc<dyn ProviderStreams> {
    struct BedrockConverseStreamApi;

    impl ProviderStreams for BedrockConverseStreamApi {
        fn stream(
            &self,
            model: &Model,
            context: &Context,
            options: Option<&StreamOptions>,
        ) -> AssistantMessageEventStream {
            let options =
                options.map(
                    |options| crate::ai::api::bedrock_converse_stream::BedrockOptions {
                        base: options.clone(),
                        ..Default::default()
                    },
                );
            crate::ai::api::bedrock_converse_stream::stream(model, context, options.as_ref())
        }
        fn stream_simple(
            &self,
            model: &Model,
            context: &Context,
            options: Option<&SimpleStreamOptions>,
        ) -> AssistantMessageEventStream {
            crate::ai::api::bedrock_converse_stream::stream_simple(model, context, options)
        }
    }
    Arc::new(BedrockConverseStreamApi)
}
