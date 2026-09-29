//! The MCP search kind (ADR-0023): a search tool on a server already
//! defined in `mcp.json`, called through the engine's MCP pool. The query
//! lands in the tool's required string parameter and the hit count in a
//! count-like integer parameter when it has one — both read off the
//! tool's input schema at call time. The tool's text goes to the model
//! as-is; its shape is the server's, not a [`super::SearchHit`] list.
//!
//! The call rides the server's own connection, auth and call timeout, and
//! skips the MCP approval gate: the user chose this tool as their search
//! backend, and `web_search` is ungated.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::{SearchBackend, SearchResults, transport};
use crate::mcp::McpPool;

/// Property names read as the hit count, in preference order.
const COUNT_NAMES: [&str; 9] = [
    "count",
    "num_results",
    "numResults",
    "max_results",
    "maxResults",
    "limit",
    "top_k",
    "topK",
    "size",
];

/// Property names read as the query when no string parameter is required.
const QUERY_NAMES: [&str; 4] = ["query", "q", "search_query", "searchQuery"];

pub(crate) struct McpSearchBackend {
    pool: Arc<McpPool>,
    server: String,
    tool: String,
    /// `server / tool` — the output header and every error name it.
    name: String,
}

impl McpSearchBackend {
    pub(crate) fn new(pool: Arc<McpPool>, server: String, tool: String) -> Self {
        let name = format!("{server} / {tool}");
        Self {
            pool,
            server,
            tool,
            name,
        }
    }
}

impl SearchBackend for McpSearchBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        max_results: usize,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<SearchResults, String>> {
        Box::pin(async move {
            let call = self.pool.call_tool(&self.server, &self.tool, |schema| {
                arguments(schema, query, max_results)
                    .ok_or_else(|| format!("{} takes no string query parameter", self.name))
            });
            transport::race_cancel(call, cancel)
                .await
                .map(SearchResults::Text)
                .map_err(|error| format!("{} search failed: {error}", self.name))
        })
    }
}

/// The call arguments for one query, off the tool's input schema: the
/// query in the first required string property (else a query-named or the
/// first string property), the count in a count-like integer property when
/// there is one. `None` when the schema has no string property at all.
fn arguments(schema: &Map<String, Value>, query: &str, max_results: usize) -> Option<Value> {
    let properties = schema.get("properties").and_then(Value::as_object);
    let empty = Map::new();
    let properties = properties.unwrap_or(&empty);
    let is_type = |name: &str, types: &[&str]| {
        properties
            .get(name)
            .and_then(|property| property.get("type"))
            .is_some_and(|kind| match kind {
                Value::String(kind) => types.contains(&kind.as_str()),
                Value::Array(kinds) => kinds
                    .iter()
                    .any(|kind| kind.as_str().is_some_and(|kind| types.contains(&kind))),
                _ => false,
            })
    };
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    let query_name = required
        .clone()
        .find(|name| is_type(name, &["string"]))
        .or_else(|| {
            QUERY_NAMES
                .into_iter()
                .find(|name| is_type(name, &["string"]))
        })
        .or_else(|| {
            properties
                .keys()
                .map(String::as_str)
                .find(|name| is_type(name, &["string"]))
        })?;
    let mut arguments = Map::new();
    arguments.insert(query_name.to_string(), Value::from(query));
    if let Some(count) = COUNT_NAMES
        .into_iter()
        .find(|name| is_type(name, &["integer", "number"]))
    {
        arguments.insert(count.to_string(), Value::from(max_results));
    }
    Some(Value::Object(arguments))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn the_required_string_takes_the_query_and_a_count_name_takes_the_cap() {
        let schema = schema(json!({
            "type": "object",
            "properties": {
                "lang": { "type": "string" },
                "q": { "type": "string" },
                "numResults": { "type": "integer" }
            },
            "required": ["q"]
        }));
        assert_eq!(
            arguments(&schema, "rust", 3),
            Some(json!({ "q": "rust", "numResults": 3 }))
        );
    }

    #[test]
    fn without_a_required_string_a_query_name_wins_and_the_count_is_optional() {
        let schema = schema(json!({
            "type": "object",
            "properties": {
                "freshness": { "type": "string" },
                "query": { "type": "string" }
            }
        }));
        assert_eq!(
            arguments(&schema, "rust", 5),
            Some(json!({ "query": "rust" }))
        );
    }

    #[test]
    fn nullable_type_arrays_count_and_a_stringless_schema_has_no_query() {
        let nullable = schema(json!({
            "properties": {
                "search": { "type": "string" },
                "limit": { "type": ["integer", "null"] }
            }
        }));
        assert_eq!(
            arguments(&nullable, "q", 2),
            Some(json!({ "search": "q", "limit": 2 }))
        );
        let stringless = schema(json!({ "properties": { "n": { "type": "integer" } } }));
        assert_eq!(arguments(&stringless, "q", 2), None);
    }
}
