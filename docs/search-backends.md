# Custom search backends

Holt ships Exa (keyless, the default), Zhipu, Bocha, and Brave. To search with
another service, define it in `search-backends.json` in Holt's data directory
(`~/.holt/search-backends.json`, or under `HOLT_DATA_DIR`). Settings → General
→ Web search shows the exact path.

Each definition joins the "Search service" dropdown. Pick it there like a
built-in; when `needsKey` is true Settings asks for the key and stores it in
`web-search.json`, so this file holds no secrets. Edits apply on the next
Settings read or chat Turn — no restart. If the file has a mistake, Settings
shows the reason and every custom service is left out until it is fixed.

```json
{
  "backends": [
    {
      "id": "tavily",
      "name": "Tavily",
      "needsKey": true,
      "type": "http",
      "request": {
        "method": "POST",
        "url": "https://api.tavily.com/search",
        "headers": { "Authorization": "Bearer {apiKey}" },
        "body": { "query": "{query}", "max_results": "{count}" }
      },
      "response": {
        "results": "/results",
        "title": "/title",
        "url": "/url",
        "snippet": "/content"
      }
    },
    {
      "id": "searxng",
      "name": "SearXNG (local)",
      "type": "http",
      "request": {
        "url": "http://localhost:8888/search",
        "query": { "q": "{query}", "format": "json" }
      },
      "response": { "results": "/results", "title": "/title", "url": "/url", "snippet": "/content" }
    },
    {
      "id": "my-mcp-search",
      "name": "My MCP search",
      "needsKey": true,
      "type": "mcp",
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer {apiKey}" },
      "tool": "search",
      "arguments": { "query": "{query}", "limit": "{count}" }
    }
  ]
}
```

## Common fields

| Field | |
|---|---|
| `id` | Unique; must not be a built-in id (`exa`, `zhipu`, `bocha`, `brave`). |
| `name` | Shown in Settings and to the model. |
| `needsKey` | Optional, default `false`. When true, Settings asks for a key before the service can be used. |
| `type` | `http` or `mcp`. |

Placeholders fill in any string value: `{query}` (the search), `{count}`
(results wanted, 1–10), `{apiKey}` (the stored key, empty when keyless). A value
that is exactly `"{count}"` becomes a JSON number.

## `type: "http"`

For a JSON REST API.

- `request.method`: `GET` (default) or `POST`.
- `request.url`: filled verbatim — put the query in `request.query`, which is
  URL-encoded.
- `request.headers`, `request.query`: string maps.
- `request.body`: any JSON, sent as the JSON body; `POST` only.
- `response.results`: [JSON Pointer](https://datatracker.ietf.org/doc/html/rfc6901)
  to the result array in the reply, e.g. `/web/results`.
- `response.title`, `response.url`, `response.snippet` (optional): JSON
  Pointers into each result. Results without a URL are skipped.

A non-2xx reply fails the search with the status and the start of the body.

## `type: "mcp"`

For a search tool on an MCP server. The connection fields are the same as an
`mcp.json` server entry: `url` plus optional `headers` / `bearerTokenEnvVar`
(Streamable HTTP), or `command` plus optional `args` / `env` / `cwd` (stdio),
and optional `startupTimeoutMs` / `toolTimeoutMs`. `{apiKey}` fills in
`url`, `headers`, `command`, `args`, and `env`; `${VAR}` expands from the
environment as in `mcp.json`.

- `tool`: the tool to call.
- `arguments`: the tool's arguments; placeholders fill in.

The tool's text content goes to the model as-is. The server is independent of
`mcp.json`: defining it here does not expose its other tools to the agent.
