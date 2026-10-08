//! Port of `pi-core/ai/src/utils/deferred-tools.ts`.

use std::collections::BTreeSet;

use indexmap::IndexMap;

use crate::ai::types::{AssistantContent, Context, Message, Tool};

/// Port of `splitDeferredTools`: splits current tools into prefix and
/// transcript-loaded definitions.
pub fn split_deferred_tools(
    context: &Context,
    enabled: bool,
    normalize_name: impl Fn(&str) -> String,
) -> DeferredToolSplit {
    // TypeScript deduplicates into a `Map`, which keeps insertion order; the
    // immediate tool list and the deferred map both preserve the context's
    // declaration order.
    let mut unique_tools: IndexMap<String, Tool> = IndexMap::new();
    for tool in context.tools.iter().flatten() {
        unique_tools.insert(normalize_name(&tool.name), tool.clone());
    }
    if !enabled {
        return DeferredToolSplit {
            immediate: unique_tools.into_values().collect(),
            deferred: IndexMap::new(),
        };
    }

    let mut deferred_names = BTreeSet::new();
    let mut used_names = BTreeSet::new();
    for message in &context.messages {
        match message {
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    if let AssistantContent::ToolCall(tool_call) = block {
                        used_names.insert(normalize_name(&tool_call.name));
                    }
                }
            }
            Message::ToolResult(result) => {
                for name in result.added_tool_names.iter().flatten() {
                    let normalized_name = normalize_name(name);
                    if !used_names.contains(&normalized_name) {
                        deferred_names.insert(normalized_name);
                    }
                }
            }
            Message::User(_) => {}
        }
    }

    let mut immediate = Vec::new();
    let mut deferred = IndexMap::new();
    for (name, tool) in unique_tools {
        if deferred_names.contains(&name) {
            deferred.insert(name, tool);
        } else {
            immediate.push(tool);
        }
    }
    DeferredToolSplit {
        immediate,
        deferred,
    }
}

/// Port of the `{ immediate, deferred }` result.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeferredToolSplit {
    pub immediate: Vec<Tool>,
    /// Deferred tools keyed by normalized tool name, in declaration order
    /// (the TypeScript `Map` iteration order).
    pub deferred: IndexMap<String, Tool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::types::{
        AssistantContent, AssistantMessage, RoleAssistant, RoleToolResult, RoleUser, TextContent,
        ToolResultMessage, UserContent, UserMessage,
    };

    fn tool(name: &str) -> Tool {
        Tool {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
            constrained_sampling: None,
        }
    }

    fn marker_context(added: &[&str]) -> Context {
        Context {
            messages: vec![
                Message::Assistant(Box::new(AssistantMessage {
                    role: RoleAssistant,
                    content: vec![AssistantContent::Text(TextContent {
                        text: "working".to_string(),
                        ..Default::default()
                    })],
                    ..Default::default()
                })),
                Message::ToolResult(Box::new(ToolResultMessage {
                    role: RoleToolResult,
                    tool_call_id: "call_1".to_string(),
                    tool_name: "read".to_string(),
                    content: vec![],
                    added_tool_names: Some(added.iter().map(|s| s.to_string()).collect()),
                    ..Default::default()
                })),
                Message::User(UserMessage {
                    role: RoleUser,
                    content: UserContent::Text("next".to_string()),
                    timestamp: 0,
                }),
            ],
            tools: Some(vec![tool("read"), tool("search"), tool("write")]),
            ..Default::default()
        }
    }

    /// The TypeScript split preserves `Map` insertion order for both the
    /// immediate list and the deferred map (`[...uniqueTools]` iteration).
    #[test]
    fn preserves_declaration_order_for_immediate_and_deferred() {
        let split =
            split_deferred_tools(&marker_context(&["search"]), true, |name| name.to_string());
        assert_eq!(
            split
                .immediate
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["read", "write"]
        );
        assert_eq!(
            split
                .deferred
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["search"]
        );
    }

    /// Unknown marker names defer nothing and keep every tool immediate.
    #[test]
    fn ignores_markers_without_matching_tools() {
        let split = split_deferred_tools(&marker_context(&["nope"]), true, |name| name.to_string());
        assert_eq!(split.deferred.len(), 0);
        assert_eq!(split.immediate.len(), 3);
    }
}
