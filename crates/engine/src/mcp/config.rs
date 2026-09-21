//! Device-level MCP server definitions (ADR-0034): the strict,
//! hand-editable `mcp.json` under the data directory — one `mcpServers`
//! map, credentials-pattern file (0600, repaired on load), and a malformed
//! file (bad JSON, unknown keys, an impossible transport mix) fails
//! startup loudly instead of silently dropping servers. Values store
//! unexpanded `${VAR}` forms; nothing here touches the environment.

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, RwLock},
};

use crate::EngineError;

pub(crate) const FILE_NAME: &str = "mcp.json";

/// Default server startup timeout (ADR-0034): a server that cannot
/// initialize within this window is skipped for the Turn.
pub(crate) const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 10_000;
/// Default per-call timeout (ADR-0034): one `tools/call`'s hard wall.
pub(crate) const DEFAULT_TOOL_TIMEOUT_MS: u64 = 60_000;

/// One server's transport: a stdio child or a Streamable HTTP endpoint.
/// The config file discriminates by field — `command` means stdio, `url`
/// means HTTP; defining both (or neither) is an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ServerTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<String>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        bearer_token_env_var: Option<String>,
    },
}

/// One `mcpServers` entry: its transport plus the shared per-server
/// fields. Parsed strictly — an unknown key anywhere in the entry is an
/// error, so a typo'd field can never silently disable a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpServer {
    pub(crate) transport: ServerTransport,
    pub(crate) enabled: bool,
    pub(crate) startup_timeout_ms: u64,
    pub(crate) tool_timeout_ms: u64,
    pub(crate) enabled_tools: Vec<String>,
    pub(crate) disabled_tools: Vec<String>,
}

fn default_enabled() -> bool {
    true
}

fn default_startup_timeout_ms() -> u64 {
    DEFAULT_STARTUP_TIMEOUT_MS
}

fn default_tool_timeout_ms() -> u64 {
    DEFAULT_TOOL_TIMEOUT_MS
}

/// The fields each transport accepts — the strictness set for the
/// entry-level parse (shared fields included).
fn transport_keys(transport: &ServerTransport) -> Vec<&'static str> {
    let shared = [
        "enabled",
        "startupTimeoutMs",
        "toolTimeoutMs",
        "enabledTools",
        "disabledTools",
    ];
    let owned: &[&str] = match transport {
        ServerTransport::Stdio { .. } => &["command", "args", "env", "cwd"],
        ServerTransport::Http { .. } => &["url", "headers", "bearerTokenEnvVar"],
    };
    let mut keys = owned.to_vec();
    keys.extend_from_slice(&shared);
    keys
}

/// The flat on-disk entry shape for one transport — its own fields plus
/// the shared ones, strict (`deny_unknown_fields` as the backstop behind
/// the manual key check, which owns the error wording).
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FlatStdio {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default = "default_startup_timeout_ms")]
    startup_timeout_ms: u64,
    #[serde(default = "default_tool_timeout_ms")]
    tool_timeout_ms: u64,
    #[serde(default)]
    enabled_tools: Vec<String>,
    #[serde(default)]
    disabled_tools: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FlatHttp {
    url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    bearer_token_env_var: Option<String>,
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default = "default_startup_timeout_ms")]
    startup_timeout_ms: u64,
    #[serde(default = "default_tool_timeout_ms")]
    tool_timeout_ms: u64,
    #[serde(default)]
    enabled_tools: Vec<String>,
    #[serde(default)]
    disabled_tools: Vec<String>,
}

/// Parse one server entry strictly. The wire shape is flat — transport
/// fields beside the shared ones — so the transport is detected first
/// (`command` vs `url`), the entry's keys checked against that transport's
/// strict set, and only then deserialized.
pub(crate) fn parse_server(name: &str, value: &serde_json::Value) -> Result<McpServer, String> {
    let map = value
        .as_object()
        .ok_or_else(|| format!("server {name:?} must be a JSON object"))?;
    let stdio = map.contains_key("command");
    let http = map.contains_key("url");
    let transport = match (stdio, http) {
        (true, false) => ServerTransport::Stdio {
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        },
        (false, true) => ServerTransport::Http {
            url: String::new(),
            headers: BTreeMap::new(),
            bearer_token_env_var: None,
        },
        (true, true) => {
            return Err(format!(
                "server {name:?} defines both `command` (stdio) and `url` (http); \
pick one transport"
            ));
        }
        (false, false) => {
            return Err(format!(
                "server {name:?} needs either `command` (stdio) or `url` (http)"
            ));
        }
    };
    let expected = transport_keys(&transport);
    if let Some(key) = map.keys().find(|key| !expected.contains(&key.as_str())) {
        return Err(format!(
            "unknown field {key:?} in server {name:?} (expected one of {})",
            expected
                .iter()
                .map(|key| format!("`{key}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let malformed = |error: serde_json::Error| format!("server {name:?} is malformed: {error}");
    if stdio {
        let flat: FlatStdio = serde_json::from_value(value.clone()).map_err(malformed)?;
        Ok(McpServer {
            transport: ServerTransport::Stdio {
                command: flat.command,
                args: flat.args,
                env: flat.env,
                cwd: flat.cwd,
            },
            enabled: flat.enabled,
            startup_timeout_ms: flat.startup_timeout_ms,
            tool_timeout_ms: flat.tool_timeout_ms,
            enabled_tools: flat.enabled_tools,
            disabled_tools: flat.disabled_tools,
        })
    } else {
        let flat: FlatHttp = serde_json::from_value(value.clone()).map_err(malformed)?;
        Ok(McpServer {
            transport: ServerTransport::Http {
                url: flat.url,
                headers: flat.headers,
                bearer_token_env_var: flat.bearer_token_env_var,
            },
            enabled: flat.enabled,
            startup_timeout_ms: flat.startup_timeout_ms,
            tool_timeout_ms: flat.tool_timeout_ms,
            enabled_tools: flat.enabled_tools,
            disabled_tools: flat.disabled_tools,
        })
    }
}

/// Parse the whole file: `{ "mcpServers": { ... } }`, unknown top-level
/// keys rejected, each entry strictly parsed with its name in the error.
pub(crate) fn parse_file(bytes: &[u8]) -> Result<BTreeMap<String, McpServer>, String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct FileShape {
        #[serde(rename = "mcpServers", default)]
        servers: HashMap<String, serde_json::Value>,
    }
    let file: FileShape =
        serde_json::from_slice(bytes).map_err(|error| format!("not a valid mcp.json: {error}"))?;
    let mut servers = BTreeMap::new();
    for (name, value) in file.servers {
        // A name outside `[A-Za-z0-9_-]` would make the two-level tool name
        // ambiguous — rejected here and at save time (ADR-0034).
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!(
                "server name {name:?} is invalid: names use [A-Za-z0-9_-] only"
            ));
        }
        servers.insert(name.clone(), parse_server(&name, &value)?);
    }
    Ok(servers)
}

/// The loaded device-level definition set. Hand-edited in this slice; the
/// Settings RPC quartet (ticket 07) writes through the same store.
#[derive(Clone, Debug)]
pub(crate) struct McpStore {
    servers: Arc<RwLock<BTreeMap<String, McpServer>>>,
}

impl McpStore {
    /// Loads the file. A missing file is the empty state; a present but
    /// malformed file is a startup error naming the path (the credentials
    /// pattern — this file steers child processes, silent fallback would
    /// hide a broken config).
    pub(crate) fn load(data_dir: &Path) -> Result<Self, EngineError> {
        let path = data_dir.join(FILE_NAME);
        let servers = match std::fs::metadata(&path) {
            Ok(metadata) => {
                ensure_private_permissions(&path, &metadata)?;
                let bytes = std::fs::read(&path)?;
                parse_file(&bytes).map_err(|error| {
                    EngineError::Other(format!(
                        "mcp config file {} is malformed; fix or remove it manually: {error}",
                        path.display()
                    ))
                })?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            servers: Arc::new(RwLock::new(servers)),
        })
    }

    /// The current definitions — the pool reads this fresh at every Turn
    /// start, so a hand edit lands from the next Turn.
    pub(crate) fn get(&self) -> BTreeMap<String, McpServer> {
        self.servers
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

#[cfg(unix)]
fn ensure_private_permissions(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), EngineError> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
            |error| {
                EngineError::Other(format!(
                    "could not secure mcp config file {}: {error}",
                    path.display()
                ))
            },
        )?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_permissions(
    _path: &Path,
    _metadata: &std::fs::Metadata,
) -> Result<(), EngineError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: serde_json::Value) -> Result<BTreeMap<String, McpServer>, String> {
        parse_file(serde_json::to_string(&json).unwrap().as_bytes())
    }

    #[test]
    fn a_missing_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = McpStore::load(dir.path()).unwrap();
        assert!(store.get().is_empty());
    }

    #[test]
    fn stdio_servers_parse_with_defaults() {
        let servers = parse(serde_json::json!({
            "mcpServers": {
                "example": {
                    "command": "npx",
                    "args": ["-y", "some-mcp-server"],
                    "env": { "KEY": "${VAR}" }
                }
            }
        }))
        .unwrap();
        let server = servers.get("example").unwrap();
        assert_eq!(
            server.transport,
            ServerTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "some-mcp-server".into()],
                env: BTreeMap::from([("KEY".into(), "${VAR}".into())]),
                cwd: None,
            }
        );
        // Defaults: enabled, 10 s startup, 60 s calls, no filters.
        assert!(server.enabled);
        assert_eq!(server.startup_timeout_ms, 10_000);
        assert_eq!(server.tool_timeout_ms, 60_000);
        assert!(server.enabled_tools.is_empty());
        assert!(server.disabled_tools.is_empty());
    }

    #[test]
    fn http_servers_parse_with_their_field_set() {
        let servers = parse(serde_json::json!({
            "mcpServers": {
                "remote": {
                    "url": "https://example.com/mcp",
                    "headers": { "X-Static": "1" },
                    "bearerTokenEnvVar": "REMOTE_TOKEN",
                    "enabled": false
                }
            }
        }))
        .unwrap();
        let server = servers.get("remote").unwrap();
        assert_eq!(
            server.transport,
            ServerTransport::Http {
                url: "https://example.com/mcp".into(),
                headers: BTreeMap::from([("X-Static".into(), "1".into())]),
                bearer_token_env_var: Some("REMOTE_TOKEN".into()),
            }
        );
        assert!(!server.enabled);
    }

    #[test]
    fn unknown_keys_are_rejected_with_the_field_named() {
        let error = parse(serde_json::json!({
            "mcpServers": { "example": { "command": "npx", "args": [], "envv": {} } }
        }))
        .unwrap_err();
        assert!(error.contains("unknown field"), "{error}");
        assert!(error.contains("envv"), "{error}");
        assert!(error.contains("example"), "{error}");
        // An http-only key on a stdio server is just as unknown.
        let error = parse(serde_json::json!({
            "mcpServers": { "example": { "command": "npx", "bearerTokenEnvVar": "T" } }
        }))
        .unwrap_err();
        assert!(error.contains("unknown field"), "{error}");
        // …and a stdio key on an http server.
        let error = parse(serde_json::json!({
            "mcpServers": { "example": { "url": "https://x/y", "args": [] } }
        }))
        .unwrap_err();
        assert!(error.contains("unknown field"), "{error}");
        // Top-level typos too.
        let error = parse(serde_json::json!({ "mcpServerz": {} })).unwrap_err();
        assert!(error.contains("unknown field"), "{error}");
    }

    #[test]
    fn transport_mixing_and_absence_are_errors() {
        let both = parse(serde_json::json!({
            "mcpServers": { "x": { "command": "npx", "url": "https://x/y" } }
        }))
        .unwrap_err();
        assert!(both.contains("both"), "{both}");
        let neither =
            parse(serde_json::json!({ "mcpServers": { "x": { "enabled": true } } })).unwrap_err();
        assert!(neither.contains("either"), "{neither}");
    }

    #[test]
    fn illegal_server_names_are_rejected() {
        let error = parse(serde_json::json!({
            "mcpServers": { "my server": { "command": "npx" } }
        }))
        .unwrap_err();
        assert!(error.contains("my server"), "{error}");
        assert!(error.contains("[A-Za-z0-9_-]"), "{error}");
    }

    #[test]
    fn malformed_files_fail_startup_loudly_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, b"{broken").unwrap();
        let error = McpStore::load(dir.path()).unwrap_err();
        assert!(error.to_string().contains("malformed"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), b"{broken");
    }
}
