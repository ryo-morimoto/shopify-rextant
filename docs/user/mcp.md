# MCP Integration

`shopify-rextant serve` speaks MCP over stdio (JSON-RPC). It supports newline-delimited JSON and `Content-Length` framing; clients should prefer newline-delimited.

Build the index once before registering the server:

```bash
shopify-rextant build
```

## Register

### Claude Code

```bash
claude mcp add --transport stdio shopify-rextant -- shopify-rextant serve
```

### Codex CLI

`~/.codex/config.toml`:

```toml
[mcp_servers.shopify-rextant]
command = "shopify-rextant"
args = ["serve"]
```

### Manual smoke test

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  | shopify-rextant serve --direct
```

The response is one JSON object followed by `\n` on stdout. Logs stay on stderr.

## Tools

### `shopify_map`

Returns a map of related nodes for a query — the agent's entry point. Use this first.

| arg | type | notes |
|---|---|---|
| `from` | string, required | GraphQL type name (`Product`), doc path (`/docs/...`), task id, or free-text query |
| `radius` | 1 \| 2 \| 3, default 2 | BFS hop depth |
| `lens` | `concept` \| `doc` \| `task` \| `auto`, default `auto` | graph axis |
| `version` | string | API version pin, e.g. `2026-04` |
| `max_nodes` | integer 1–100, default 30 | cap on returned nodes |

Response contains: `center`, `nodes[]` (with source-verbatim `summary_from_source` and `staleness`), `edges[]`, `suggested_reading_order`, `query_plan`, and `meta` (`graph_available`, `coverage_warning`, `on_demand_candidate`, …). No summarization — summaries are raw markdown prefixes.

When the concept graph is not available yet for a query, `meta.graph_available = false` and the response falls back to FTS candidates with honest warnings.

### `shopify_fetch`

Returns the raw markdown of one document. No summarization.

| arg | type | notes |
|---|---|---|
| `path` | string | canonical docs path (from `shopify_map`) |
| `url` | string | on-demand: `shopify.dev/docs/**` or `shopify.dev/changelog/**` only |
| `anchor` | string | return only the matching section |
| `include_code_blocks` | boolean, default `true` | set `false` to strip fenced code |
| `max_chars` | integer, default 20000 | truncation cap |

Exactly one of `path` or `url` is required. `url` triggers on-demand recovery and requires `enable_on_demand_fetch = true`; see [config.md](config.md).

### `shopify_status`

No input. Returns the same payload as the `status` CLI: doc counts, coverage buckets, freshness tallies, worker timestamps, and `warnings[]`.

## Error Codes

| JSON-RPC code | meaning |
|---|---|
| `-32007` | on-demand fetch disabled in config |
| `-32008` | URL outside allowed Shopify docs scope |
| `-32000` | internal error |

## Privacy Boundary

Outbound HTTP is limited to official Shopify docs / changelog / GraphQL direct proxy endpoints. The server does not send user code, prompts, project files, client metadata, or telemetry.
