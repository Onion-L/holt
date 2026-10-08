//! Port of `pi-core/ai/src/api/simple-options.ts`: shared option building for
//! `streamSimple` adapters.

use crate::ai::types::{
    Context, Model, SimpleStreamOptions, StreamOptions, ThinkingBudgets, ThinkingLevel,
};
use crate::ai::utils::estimate::estimate_context_tokens;

const CONTEXT_SAFETY_TOKENS: u64 = 4096;
const MIN_MAX_TOKENS: u64 = 1;

/// Port of `clampMaxTokensToContext`.
pub fn clamp_max_tokens_to_context(model: &Model, context: &Context, max_tokens: u64) -> u64 {
    if model.context_window == 0 {
        return max_tokens.max(MIN_MAX_TOKENS);
    }
    let available = model
        .context_window
        .saturating_sub(estimate_context_tokens(context).tokens)
        .saturating_sub(CONTEXT_SAFETY_TOKENS);
    max_tokens.min(available.max(MIN_MAX_TOKENS))
}

/// Port of `buildBaseOptions`.
pub fn build_base_options(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
    api_key: Option<&str>,
) -> StreamOptions {
    let options = options.cloned().unwrap_or_default();
    let sampling_params = match (
        model.sampling_params.as_ref(),
        options.base.sampling_params.as_ref(),
    ) {
        (None, None) => None,
        (model_params, request_params) => {
            let merged: std::collections::BTreeMap<String, serde_json::Value> = model_params
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .chain(request_params.cloned().unwrap_or_default())
                .collect();
            Some(merged)
        }
    };
    StreamOptions {
        base: crate::ai::types::ProviderRequestOptions {
            signal: options.base.base.signal.clone(),
            telemetry_context: options.base.base.telemetry_context.clone(),
            api_key: api_key
                .map(str::to_string)
                .or_else(|| options.base.base.api_key.clone()),
            fetch: options.base.base.fetch.clone(),
            env: options.base.base.env.clone(),
            headers: options.base.base.headers.clone(),
            timeout_ms: options.base.base.timeout_ms,
            max_retries: options.base.base.max_retries,
            max_retry_delay_ms: options.base.base.max_retry_delay_ms,
            on_payload: options.base.base.on_payload.clone(),
            on_response: options.base.base.on_response.clone(),
            on_retry: options.base.base.on_retry.clone(),
            ..Default::default()
        },
        temperature: options.base.temperature,
        sampling_params,
        max_tokens: Some(clamp_max_tokens_to_context(
            model,
            context,
            options.base.max_tokens.unwrap_or(model.max_tokens),
        )),
        transport: options.base.transport,
        cache_retention: options.base.cache_retention,
        session_id: options.base.session_id,
        websocket_connect_timeout_ms: options.base.websocket_connect_timeout_ms,
        metadata: options.base.metadata,
        transform_headers: options.base.transform_headers.clone(),
    }
}

/// Port of `MIN_ANSWER_TOKENS`.
pub const MIN_ANSWER_TOKENS: u64 = 1024;

/// Port of `DEFAULT_THINKING_BUDGETS`.
pub fn default_thinking_budgets() -> ThinkingBudgets {
    ThinkingBudgets {
        minimal: Some(1024),
        low: Some(2048),
        medium: Some(8192),
        high: Some(16384),
    }
}

/// Port of `clampReasoning`.
pub fn clamp_reasoning(effort: Option<ThinkingLevel>) -> Option<ThinkingLevel> {
    match effort {
        Some(ThinkingLevel::Xhigh) | Some(ThinkingLevel::Max) => Some(ThinkingLevel::High),
        other => other,
    }
}

/// Port of `thinkingBudgetForLevel`.
pub fn thinking_budget_for_level(
    reasoning_level: ThinkingLevel,
    custom_budgets: Option<&ThinkingBudgets>,
) -> u64 {
    let budgets = match custom_budgets {
        Some(custom) => {
            let defaults = default_thinking_budgets();
            ThinkingBudgets {
                minimal: custom.minimal.or(defaults.minimal),
                low: custom.low.or(defaults.low),
                medium: custom.medium.or(defaults.medium),
                high: custom.high.or(defaults.high),
            }
        }
        None => default_thinking_budgets(),
    };
    let level = clamp_reasoning(Some(reasoning_level)).expect("reasoning level is present");
    match level {
        ThinkingLevel::Minimal => budgets.minimal,
        ThinkingLevel::Low => budgets.low,
        ThinkingLevel::Medium => budgets.medium,
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => budgets.high,
    }
    .unwrap_or(1024)
}

/// Port of `clampThinkingBudgetToAnswerRoom`.
pub fn clamp_thinking_budget_to_answer_room(thinking_budget: u64, ceiling: u64) -> u64 {
    thinking_budget.min(ceiling.saturating_sub(MIN_ANSWER_TOKENS))
}

/// Port of `adjustMaxTokensForThinking`.
pub fn adjust_max_tokens_for_thinking(
    base_max_tokens: Option<u64>,
    model_max_tokens: u64,
    reasoning_level: ThinkingLevel,
    custom_budgets: Option<&ThinkingBudgets>,
) -> (u64, u64) {
    let mut thinking_budget = thinking_budget_for_level(reasoning_level, custom_budgets);
    let max_tokens = match base_max_tokens {
        None => model_max_tokens,
        Some(base) => (base + thinking_budget).min(model_max_tokens),
    };

    if max_tokens <= thinking_budget {
        thinking_budget = clamp_thinking_budget_to_answer_room(thinking_budget, max_tokens);
    }

    (max_tokens, thinking_budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::types::{Message, RoleUser, UserContent, UserMessage};

    fn model() -> Model {
        Model {
            id: "test".to_string(),
            name: "Test".to_string(),
            api: "anthropic-messages".to_string(),
            provider: "test".to_string(),
            base_url: "https://example.com".to_string(),
            reasoning: false,
            input: vec![crate::ai::types::ModelInput::Text],
            cost: crate::ai::types::ModelCost::default(),
            context_window: 100_000,
            max_tokens: 8_192,
            ..Default::default()
        }
    }

    fn context() -> Context {
        Context {
            messages: vec![Message::User(UserMessage {
                role: RoleUser,
                content: UserContent::Text("hi".to_string()),
                timestamp: 0,
            })],
            ..Default::default()
        }
    }

    #[test]
    fn clamps_max_tokens_to_context() {
        let m = model();
        assert_eq!(clamp_max_tokens_to_context(&m, &context(), 8_192), 8_192);
        // contextWindow - estimate - 4096 caps the value.
        assert!(
            clamp_max_tokens_to_context(&m, &context(), 200_000) < 200_000,
            "large request is clamped"
        );
        let mut tiny = m.clone();
        tiny.context_window = 0;
        assert_eq!(clamp_max_tokens_to_context(&tiny, &context(), 0), 1);
    }

    #[test]
    fn thinking_budget_adjustment() {
        // High budget (16384) overflows the shared ceiling: max is capped at
        // the model limit and the budget clamps so MIN_ANSWER_TOKENS remain.
        let (max_tokens, budget) =
            adjust_max_tokens_for_thinking(Some(4_096), 8_192, ThinkingLevel::High, None);
        assert_eq!(max_tokens, 8_192);
        assert_eq!(budget, 8_192 - 1_024);

        let (max_tokens, budget) =
            adjust_max_tokens_for_thinking(None, 8_192, ThinkingLevel::Low, None);
        assert_eq!(max_tokens, 8_192);
        assert_eq!(budget, 2_048);

        // Budget is clamped so MIN_ANSWER_TOKENS remain: max =
        // min(1500 + 8192, 8192) = 8192 <= budget 8192, so the budget clamps
        // to 8192 - 1024.
        let (_, budget) =
            adjust_max_tokens_for_thinking(Some(1_500), 8_192, ThinkingLevel::Medium, None);
        assert_eq!(budget, 8_192 - 1_024);

        let (max_tokens, budget) =
            adjust_max_tokens_for_thinking(Some(0), 0, ThinkingLevel::Minimal, None);
        assert_eq!(max_tokens, 0);
        assert_eq!(budget, 0);
    }
}
