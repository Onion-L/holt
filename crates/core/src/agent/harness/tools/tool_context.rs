//! Port of `pi-core/agent/src/harness/tools/tool-context.ts`.

use std::sync::Arc;

use crate::agent::harness::types::{AgentToolContext, ExecutionEnv};

/// Filesystem and shell context required by the built-in execution tools.
///
/// The TypeScript interface rides structurally on the harness tool
/// context; the Rust port stores it as the `AgentToolContext` payload and
/// offers [`as_execution_tool_context`] for tools to recover it.
pub struct ExecutionToolContext {
    pub env: Arc<dyn ExecutionEnv>,
}

impl ExecutionToolContext {
    /// Wraps the context as the opaque harness tool-context payload.
    pub fn into_tool_context(self) -> AgentToolContext {
        Arc::new(self)
    }
}

/// Downcasts a harness tool context to the execution context.
pub fn as_execution_tool_context(
    context: &AgentToolContext,
) -> Result<&ExecutionToolContext, String> {
    context
        .downcast_ref::<ExecutionToolContext>()
        .ok_or_else(|| "tool context is not an ExecutionToolContext".to_string())
}
