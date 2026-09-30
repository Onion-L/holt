//! User-defined search backends (ADR-0023): `search-backends.json` under
//! the data directory, hand-written, holding no secrets. Each definition
//! names a `type` — `http` (a JSON REST API: a request template plus JSON
//! Pointer paths into the reply) or `mcp` (one tool on an MCP server,
//! connected the way `mcp.json` connects one) — and becomes one more
//! option in the Settings picker; its key, when `needsKey`, lives in
//! `web-search.json` like a built-in's.
//!
//! ```json
//! { "backends": [
//!   { "id": "tavily", "name": "Tavily", "needsKey": true, "type": "http",
//!     "request": { "method": "POST", "url": "https://api.tavily.com/search",
//!                  "headers": { "Authorization": "Bearer {apiKey}" },
//!                  "body": { "query": "{query}", "max_results": "{count}" } },
//!     "response": { "results": "/results", "title": "/title",
//!                   "url": "/url", "snippet": "/content" } },
//!   { "id": "local", "name": "Local search", "type": "mcp",
//!     "command": "npx", "args": ["-y", "some-search-mcp"],
//!     "tool": "search", "arguments": { "q": "{query}" } }
//! ] }
//! ```
//!
//! `{query}`, `{count}`, and `{apiKey}` fill in any string value; a value
//! that is exactly `"{count}"` becomes a number. Put the query in
//! `request.query` (URL-encoded) rather than in `url` (filled verbatim).
//!
//! The file is read fresh on every settings read and Turn admission, so an
//! edit lands without a restart. It is parsed strictly as a whole: a typo
//! anywhere leaves every custom backend out and the reason reaches
//! Settings — startup never fails on it.

use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{BACKENDS, SearchBackend, SearchHit, SearchResults, transport};
use crate::mcp::{self, LiveServer, config::McpServer};

pub(crate) const FILE_NAME: &str = "search-backends.json";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How much of a failing reply body an HTTP error quotes.
const ERROR_BODY_CHARS: usize = 300;

/// One parsed definition.
#[derive(Debug, Clone)]
pub(crate) struct CustomBackend {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) needs_key: bool,
    source: Source,
}

#[derive(Debug, Clone)]
enum Source {
    Http {
        request: HttpRequest,
        response: HttpResponse,
    },
    Mcp {
        server: McpServer,
        tool: String,
        arguments: Value,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HttpRequest {
    #[serde(default)]
    method: Method,
    url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    query: BTreeMap<String, String>,
    /// Sent as JSON; only a POST carries one.
    #[serde(default)]
    body: Option<Value>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum Method {
    #[default]
    Get,
    Post,
}

/// JSON Pointers: `results` into the reply, the rest into each result.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HttpResponse {
    results: String,
    title: String,
    url: String,
    #[serde(default)]
    snippet: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileShape {
    backends: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HttpShape {
    id: String,
    name: String,
    #[serde(default)]
    needs_key: bool,
    #[serde(rename = "type")]
    _type: String,
    request: HttpRequest,
    response: HttpResponse,
}

/// The `mcp` definition fields besides the connection ones, which go to
/// the `mcp.json` entry parser.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpShape {
    id: String,
    name: String,
    #[serde(default)]
    needs_key: bool,
    tool: String,
    #[serde(default = "empty_object")]
    arguments: Value,
}

const MCP_OWN_KEYS: [&str; 6] = ["id", "name", "needsKey", "type", "tool", "arguments"];
/// The `mcp.json` entry fields a search backend has no use for.
const MCP_REFUSED_KEYS: [&str; 3] = ["enabled", "enabledTools", "disabledTools"];

fn empty_object() -> Value {
    Value::Object(Default::default())
}

/// Read the definitions. A missing file is none; anything else wrong is
/// the error naming the file.
pub(crate) fn load(data_dir: &Path) -> Result<Vec<CustomBackend>, String> {
    let path = data_dir.join(FILE_NAME);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    parse(&bytes).map_err(|error| format!("{}: {error}", path.display()))
}

fn parse(bytes: &[u8]) -> Result<Vec<CustomBackend>, String> {
    let file: FileShape = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    let mut backends: Vec<CustomBackend> = Vec::new();
    for (index, value) in file.backends.iter().enumerate() {
        let backend = parse_backend(value).map_err(|error| {
            let id = value.get("id").and_then(Value::as_str);
            match id {
                Some(id) => format!("backend {id:?}: {error}"),
                None => format!("backend #{}: {error}", index + 1),
            }
        })?;
        if BACKENDS.iter().any(|builtin| builtin.id == backend.id) {
            return Err(format!("backend {:?}: the id is a built-in's", backend.id));
        }
        if backends.iter().any(|other| other.id == backend.id) {
            return Err(format!("backend {:?}: the id is defined twice", backend.id));
        }
        backends.push(backend);
    }
    Ok(backends)
}

fn parse_backend(value: &Value) -> Result<CustomBackend, String> {
    let map = value.as_object().ok_or("must be a JSON object")?;
    let backend = match map.get("type").and_then(Value::as_str) {
        Some("http") => {
            let shape: HttpShape =
                serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
            let response = &shape.response;
            for pointer in [&response.results, &response.title, &response.url]
                .into_iter()
                .chain(&response.snippet)
            {
                if !pointer.is_empty() && !pointer.starts_with('/') {
                    return Err(format!(
                        "response path {pointer:?} is not a JSON Pointer (start it with `/`)"
                    ));
                }
            }
            if shape.request.body.is_some() && matches!(shape.request.method, Method::Get) {
                return Err("`request.body` needs `\"method\": \"POST\"`".into());
            }
            CustomBackend {
                id: shape.id,
                name: shape.name,
                needs_key: shape.needs_key,
                source: Source::Http {
                    request: shape.request,
                    response: shape.response,
                },
            }
        }
        Some("mcp") => {
            if let Some(key) = MCP_REFUSED_KEYS.iter().find(|key| map.contains_key(**key)) {
                return Err(format!("unknown field `{key}`"));
            }
            let own = map
                .iter()
                .filter(|(key, _)| MCP_OWN_KEYS.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<serde_json::Map<_, _>>();
            let shape: McpShape =
                serde_json::from_value(Value::Object(own)).map_err(|error| error.to_string())?;
            let connection = map
                .iter()
                .filter(|(key, _)| !MCP_OWN_KEYS.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<serde_json::Map<_, _>>();
            let server = mcp::config::parse_server(&shape.id, &Value::Object(connection))?;
            if !shape.arguments.is_object() {
                return Err("`arguments` must be a JSON object".into());
            }
            CustomBackend {
                id: shape.id,
                name: shape.name,
                needs_key: shape.needs_key,
                source: Source::Mcp {
                    server,
                    tool: shape.tool,
                    arguments: shape.arguments,
                },
            }
        }
        Some(other) => return Err(format!("unknown type {other:?} (expected `http` or `mcp`)")),
        None => return Err("needs a `type`: `http` or `mcp`".into()),
    };
    if backend.id.trim().is_empty() || backend.name.trim().is_empty() {
        return Err("`id` and `name` must not be empty".into());
    }
    Ok(backend)
}

impl CustomBackend {
    /// The adapter for one Turn, carrying the entry's key.
    pub(crate) fn adapter(&self, api_key: String) -> Arc<dyn SearchBackend> {
        match &self.source {
            Source::Http { request, response } => Arc::new(HttpBackend {
                name: self.name.clone(),
                api_key,
                request: request.clone(),
                response: response.clone(),
            }),
            Source::Mcp {
                server,
                tool,
                arguments,
            } => Arc::new(McpBackend {
                name: self.name.clone(),
                server: with_key(server, &api_key),
                tool: tool.clone(),
                arguments: arguments.clone(),
                connection: tokio::sync::Mutex::new(None),
            }),
        }
    }
}

/// The placeholder values for one query.
struct Vars<'a> {
    query: &'a str,
    count: usize,
    api_key: &'a str,
}

impl Vars<'_> {
    fn fill_str(&self, text: &str) -> String {
        text.replace("{query}", self.query)
            .replace("{count}", &self.count.to_string())
            .replace("{apiKey}", self.api_key)
    }

    fn fill(&self, value: &Value) -> Value {
        match value {
            Value::String(text) if text == "{count}" => Value::from(self.count),
            Value::String(text) => Value::String(self.fill_str(text)),
            Value::Array(items) => Value::Array(items.iter().map(|item| self.fill(item)).collect()),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, value)| (key.clone(), self.fill(value)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

struct HttpBackend {
    name: String,
    api_key: String,
    request: HttpRequest,
    response: HttpResponse,
}

impl HttpBackend {
    async fn request(&self, query: &str, count: usize) -> Result<Vec<SearchHit>, String> {
        let vars = Vars {
            query,
            count,
            api_key: &self.api_key,
        };
        let url = vars.fill_str(&self.request.url);
        let (status, body) = transport::send_bounded(&self.name, REQUEST_TIMEOUT, |client| {
            let mut builder = match self.request.method {
                Method::Get => client.get(&url),
                Method::Post => client.post(&url),
            };
            for (name, value) in &self.request.headers {
                builder = builder.header(name, vars.fill_str(value));
            }
            let query = self
                .request
                .query
                .iter()
                .map(|(name, value)| (name.clone(), vars.fill_str(value)))
                .collect::<Vec<_>>();
            if !query.is_empty() {
                builder = builder.query(&query);
            }
            if let Some(body) = &self.request.body {
                builder = builder.json(&vars.fill(body));
            }
            builder
        })
        .await?;
        let name = &self.name;
        if !status.is_success() {
            let text = String::from_utf8_lossy(&body);
            let text = text.trim();
            let detail = if text.is_empty() {
                String::new()
            } else {
                format!(
                    ": {}",
                    text.chars().take(ERROR_BODY_CHARS).collect::<String>()
                )
            };
            return Err(format!("{name} search failed: HTTP {status}{detail}"));
        }
        let reply: Value = serde_json::from_slice(&body)
            .map_err(|error| format!("{name} search returned invalid JSON: {error}"))?;
        let results = reply
            .pointer(&self.response.results)
            .and_then(Value::as_array)
            .ok_or_else(|| {
                format!(
                    "{name} search reply has no array at {:?}",
                    self.response.results
                )
            })?;
        Ok(results
            .iter()
            .map(|item| SearchHit {
                title: field(item, &self.response.title),
                url: field(item, &self.response.url),
                snippet: self
                    .response
                    .snippet
                    .as_deref()
                    .map(|pointer| field(item, pointer))
                    .unwrap_or_default(),
            })
            .filter(|hit| !hit.url.is_empty())
            .collect())
    }
}

/// A result field as text: a string as-is, a number or bool printed,
/// anything else (or nothing) empty.
fn field(item: &Value, pointer: &str) -> String {
    match item.pointer(pointer) {
        Some(Value::String(text)) => text.clone(),
        Some(value @ (Value::Number(_) | Value::Bool(_))) => value.to_string(),
        _ => String::new(),
    }
}

impl SearchBackend for HttpBackend {
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
            transport::race_cancel(self.request(query, max_results), cancel)
                .await
                .map(SearchResults::Hits)
        })
    }
}

/// The server definition with `{apiKey}` filled into its connection
/// strings (`${VAR}` expansion still happens at connect).
fn with_key(server: &McpServer, api_key: &str) -> McpServer {
    use mcp::config::ServerTransport;
    let fill = |text: &String| text.replace("{apiKey}", api_key);
    let fill_map = |map: &BTreeMap<String, String>| {
        map.iter()
            .map(|(name, value)| (name.clone(), fill(value)))
            .collect()
    };
    let transport = match &server.transport {
        ServerTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } => ServerTransport::Stdio {
            command: fill(command),
            args: args.iter().map(fill).collect(),
            env: fill_map(env),
            cwd: cwd.clone(),
        },
        ServerTransport::Http {
            url,
            headers,
            bearer_token_env_var,
        } => ServerTransport::Http {
            url: fill(url),
            headers: fill_map(headers),
            bearer_token_env_var: bearer_token_env_var.clone(),
        },
    };
    McpServer {
        transport,
        ..server.clone()
    }
}

/// One tool on an MCP server. The connection opens on the Turn's first
/// search and serves the rest of it; a failed call drops it, so the next
/// search reconnects.
struct McpBackend {
    name: String,
    server: McpServer,
    tool: String,
    arguments: Value,
    connection: tokio::sync::Mutex<Option<Arc<LiveServer>>>,
}

impl McpBackend {
    async fn request(&self, query: &str, count: usize) -> Result<String, String> {
        let name = &self.name;
        let connection = {
            let mut slot = self.connection.lock().await;
            match slot.as_ref() {
                Some(connection) => Arc::clone(connection),
                None => {
                    let connection =
                        Arc::new(mcp::connect(&self.server).await.map_err(|error| {
                            format!("{name} search could not connect: {error}")
                        })?);
                    *slot = Some(Arc::clone(&connection));
                    connection
                }
            }
        };
        let arguments = Vars {
            query,
            count,
            api_key: "",
        }
        .fill(&self.arguments);
        match connection.call_text(&self.tool, &arguments).await {
            Ok(text) if text.trim().is_empty() => Ok("No results.".into()),
            Ok(text) => Ok(text),
            Err(error) => {
                self.connection.lock().await.take();
                Err(format!("{name} search failed: {error}"))
            }
        }
    }
}

impl SearchBackend for McpBackend {
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
            transport::race_cancel(self.request(query, max_results), cancel)
                .await
                .map(SearchResults::Text)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_http::{response, serve};
    use serde_json::json;

    fn parse_one(definition: Value) -> Result<CustomBackend, String> {
        parse(json!({ "backends": [definition] }).to_string().as_bytes())
            .map(|mut backends| backends.remove(0))
    }

    fn http_definition(base: &str) -> Value {
        json!({
            "id": "tavily",
            "name": "Tavily",
            "needsKey": true,
            "type": "http",
            "request": {
                "method": "POST",
                "url": format!("{base}/search"),
                "headers": { "Authorization": "Bearer {apiKey}" },
                "query": { "lang": "en" },
                "body": { "query": "{query}", "max_results": "{count}", "note": "n={count}" }
            },
            "response": { "results": "/data/items", "title": "/title", "url": "/link", "snippet": "/text" }
        })
    }

    #[test]
    fn a_missing_file_is_no_backends() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn both_types_parse() {
        let backends = parse(
            json!({ "backends": [
                http_definition("http://x"),
                {
                    "id": "local", "name": "Local", "type": "mcp",
                    "command": "search-mcp", "args": ["--key", "{apiKey}"],
                    "tool": "search", "arguments": { "q": "{query}" }
                },
                {
                    "id": "remote", "name": "Remote", "type": "mcp", "needsKey": true,
                    "url": "https://example.com/mcp",
                    "headers": { "Authorization": "Bearer {apiKey}" },
                    "tool": "search"
                }
            ]})
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        let summary = backends
            .iter()
            .map(|backend| {
                (
                    backend.id.as_str(),
                    backend.name.as_str(),
                    backend.needs_key,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            vec![
                ("tavily", "Tavily", true),
                ("local", "Local", false),
                ("remote", "Remote", true),
            ]
        );
    }

    #[test]
    fn mistakes_name_the_backend_and_the_problem() {
        let cases = [
            (
                json!({ "id": "a", "name": "A" }),
                "backend \"a\": needs a `type`",
            ),
            (
                json!({ "id": "a", "name": "A", "type": "grpc" }),
                "unknown type \"grpc\"",
            ),
            (
                json!({ "name": "A", "type": "http" }),
                "backend #1: missing field `id`",
            ),
            (
                {
                    let mut definition = http_definition("http://x");
                    definition["request"]["mehtod"] = json!("GET");
                    definition
                },
                "unknown field `mehtod`",
            ),
            (
                {
                    let mut definition = http_definition("http://x");
                    definition["response"]["title"] = json!("title");
                    definition
                },
                "not a JSON Pointer",
            ),
            (
                {
                    let mut definition = http_definition("http://x");
                    definition["request"]["method"] = json!("GET");
                    definition
                },
                "needs `\"method\": \"POST\"`",
            ),
            (
                json!({ "id": "a", "name": "A", "type": "mcp", "tool": "t" }),
                "needs either `command` (stdio) or `url` (http)",
            ),
            (
                json!({ "id": "a", "name": "A", "type": "mcp", "url": "http://x", "tool": "t", "enabled": true }),
                "unknown field `enabled`",
            ),
            (
                json!({ "id": "a", "name": "A", "type": "mcp", "url": "http://x" }),
                "missing field `tool`",
            ),
            (
                json!({ "id": "exa", "name": "Mine", "type": "mcp", "url": "http://x", "tool": "t" }),
                "the id is a built-in's",
            ),
        ];
        for (definition, expected) in cases {
            let error = parse_one(definition).unwrap_err();
            assert!(error.contains(expected), "{expected:?} not in {error:?}");
        }

        let twice = json!({ "backends": [http_definition("a"), http_definition("b")] });
        assert!(
            parse(twice.to_string().as_bytes())
                .unwrap_err()
                .contains("defined twice")
        );
        assert!(
            parse(b"{\"backend\": []}")
                .unwrap_err()
                .contains("unknown field")
        );
    }

    #[test]
    fn a_bad_file_names_its_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), b"{oops").unwrap();
        let error = load(dir.path()).unwrap_err();
        assert!(error.contains(FILE_NAME), "unexpected: {error}");
    }

    #[test]
    fn placeholders_fill_strings_and_a_lone_count_becomes_a_number() {
        let vars = Vars {
            query: "rust",
            count: 3,
            api_key: "sk",
        };
        assert_eq!(
            vars.fill(&json!({ "q": "{query}", "n": "{count}", "k": ["Bearer {apiKey}", 7], "s": "top {count}" })),
            json!({ "q": "rust", "n": 3, "k": ["Bearer sk", 7], "s": "top 3" })
        );
    }

    #[tokio::test]
    async fn an_http_backend_sends_the_template_and_maps_the_reply() {
        let server = serve(|_, _| {
            response(
                "200 OK",
                "application/json",
                json!({ "data": { "items": [
                    { "title": "Tokio", "link": "https://tokio.rs", "text": "Runtime" },
                    { "title": "No link" },
                    { "title": 42, "link": "https://n" }
                ]}})
                .to_string()
                .as_bytes(),
            )
        })
        .await;
        let backend = parse_one(http_definition(&server.base))
            .unwrap()
            .adapter("sk-1".into());
        let results = backend
            .search("rust async", 3, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            results,
            SearchResults::Hits(vec![
                SearchHit {
                    title: "Tokio".into(),
                    url: "https://tokio.rs".into(),
                    snippet: "Runtime".into(),
                },
                SearchHit {
                    title: "42".into(),
                    url: "https://n".into(),
                    snippet: String::new(),
                },
            ])
        );
        assert_eq!(backend.name(), "Tavily");

        let head = &server.requests()[0];
        assert!(
            head.starts_with("POST /search?lang=en HTTP/1.1"),
            "unexpected: {head}"
        );
        assert!(
            head.contains("authorization: Bearer sk-1\r\n"),
            "missing auth header: {head}"
        );
        let body: Value = serde_json::from_slice(&server.bodies()[0]).unwrap();
        assert_eq!(
            body,
            json!({ "query": "rust async", "max_results": 3, "note": "n=3" })
        );
    }

    #[tokio::test]
    async fn a_get_backend_url_encodes_the_query() {
        let server =
            serve(|_, _| response("200 OK", "application/json", br#"{"results": []}"#)).await;
        let backend = parse_one(json!({
            "id": "searxng", "name": "SearXNG", "type": "http",
            "request": { "url": format!("{}/search", server.base), "query": { "q": "{query}", "format": "json" } },
            "response": { "results": "/results", "title": "/title", "url": "/url" }
        }))
        .unwrap()
        .adapter(String::new());
        assert_eq!(
            backend
                .search("a b&c", 5, CancellationToken::new())
                .await
                .unwrap(),
            SearchResults::Hits(Vec::new())
        );
        let head = &server.requests()[0];
        assert!(
            head.starts_with("GET /search?format=json&q=a+b%26c HTTP/1.1"),
            "unexpected: {head}"
        );
    }

    #[tokio::test]
    async fn http_failures_name_the_backend() {
        let server = serve(|_, _| {
            response(
                "401 Unauthorized",
                "application/json",
                br#"{"error":"bad key"}"#,
            )
        })
        .await;
        let backend = parse_one(http_definition(&server.base))
            .unwrap()
            .adapter("sk".into());
        assert_eq!(
            backend
                .search("q", 5, CancellationToken::new())
                .await
                .unwrap_err(),
            r#"Tavily search failed: HTTP 401 Unauthorized: {"error":"bad key"}"#
        );

        let server = serve(|_, _| response("200 OK", "application/json", br#"{"data": {}}"#)).await;
        let backend = parse_one(http_definition(&server.base))
            .unwrap()
            .adapter("sk".into());
        assert_eq!(
            backend
                .search("q", 5, CancellationToken::new())
                .await
                .unwrap_err(),
            "Tavily search reply has no array at \"/data/items\""
        );
    }

    #[test]
    fn the_key_fills_the_mcp_connection() {
        let backend = parse_one(json!({
            "id": "remote", "name": "Remote", "type": "mcp", "needsKey": true,
            "url": "https://example.com/mcp?key={apiKey}",
            "headers": { "Authorization": "Bearer {apiKey}" },
            "tool": "search"
        }))
        .unwrap();
        let Source::Mcp { server, .. } = &backend.source else {
            panic!("expected an mcp source");
        };
        assert_eq!(
            with_key(server, "sk").transport,
            mcp::config::ServerTransport::Http {
                url: "https://example.com/mcp?key=sk".into(),
                headers: BTreeMap::from([("Authorization".into(), "Bearer sk".into())]),
                bearer_token_env_var: None,
            }
        );
    }

    #[tokio::test]
    async fn an_unreachable_mcp_server_fails_the_search_by_name() {
        let backend = parse_one(json!({
            "id": "local", "name": "Local", "type": "mcp",
            "command": "/nonexistent/holt-search-mcp", "tool": "search"
        }))
        .unwrap()
        .adapter(String::new());
        let error = backend
            .search("q", 5, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            error.starts_with("Local search could not connect:"),
            "unexpected: {error}"
        );
    }
}
