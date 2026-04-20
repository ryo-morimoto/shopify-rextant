---
name: shopify-rextant
description: Use shopify-rextant, a local MCP server for Shopify developer documentation. Use when looking up Shopify Admin GraphQL types, Liquid references, Functions / Polaris guides, or changelog entries — so coding agents can get source-backed docs without remote round-trips. Covers MCP client registration, the three MCP tools, and opting into on-demand recovery of missing docs.
---

# shopify-rextant

Local-first MCP server that maps Shopify developer documentation. Returns source-backed graphs and raw markdown — never synthesized answers.

## First Reads

1. [`README.md`](../../../README.md) — install + quickstart
2. [`docs/user/mcp.md`](../../../docs/user/mcp.md) — MCP registration and tool contracts
3. [`docs/user/troubleshooting.md`](../../../docs/user/troubleshooting.md) — error codes and stdio issues

Decide from these files, not memory.

## Setup Loop

1. Install: `cargo install --path .` from the repo (no release channel yet).
2. Build the index once: `shopify-rextant build`. Expect 2–5 minutes on the first run.
3. Register with your MCP client:
   - Claude Code: `claude mcp add --transport stdio shopify-rextant -- shopify-rextant serve`
   - Codex: add `[mcp_servers.shopify-rextant]` in `~/.codex/config.toml` with `command = "shopify-rextant"` and `args = ["serve"]`
4. Verify: ask the client to call `shopify_status`. `index_built` should be `true` and `doc_count` non-zero.

## Tool Selection

- **`shopify_map`** — first call for any investigation. Pass `from` as a GraphQL type (`Product`), doc path (`/docs/...`), task id, or free-text query. Returns `center` + `nodes[]` (with `summary_from_source` — raw prefix, not a summary) + `edges[]` + `suggested_reading_order` + `query_plan`.
- **`shopify_fetch`** — follow-up call to read the raw markdown for one path from the map. Use `anchor` to narrow to a section. Set `include_code_blocks=false` only when deliberately stripping fenced code.
- **`shopify_status`** — call when results look sparse or stale. Surfaces coverage gaps, freshness counts, and worker timestamps.

Do not treat `summary_from_source` as an answer — it is a raw excerpt. Read the full doc via `shopify_fetch` before concluding.

## Interpreting `shopify_map.meta`

- `graph_available: false` — the concept graph was not used. `edges[]` will be empty; fall back to `nodes[]` FTS candidates.
- `coverage_warning` — the index is stale or incomplete. Follow the hint (`run shopify_refresh`, rebuild, etc.).
- `on_demand_candidate` — the query returned zero nodes and the path is recoverable. `enabled: false` means the user has not opted in; see below.

## On-Demand Recovery (Opt-In)

When a doc is missing, the server can fetch it from `shopify.dev` — but only if the human operator turns it on. Tell the user to add this to `~/.shopify-rextant/config.toml`:

```toml
[index]
enable_on_demand_fetch = true
```

Then either:

- CLI: `shopify-rextant refresh --url https://shopify.dev/docs/...`
- MCP: call `shopify_fetch` with `url` set to a `shopify.dev/docs/**` or `shopify.dev/changelog/**` URL

Error handling:

- `-32007` — on-demand is disabled. Instruct the user to flip the flag; do not try to work around it.
- `-32008` — the URL is outside allowed scope (only `shopify.dev/docs/**` and `shopify.dev/changelog/**` are accepted). Pick a different URL.

There is no MCP argument to toggle the flag from the client side. That is deliberate.

## Non-Obvious Facts

- Responses are deterministic. Same index + same query ⇒ same output.
- Stdout is protocol-only. Server logs go to stderr.
- Summaries are raw markdown prefixes — never synthesized. If you want a summary, produce it yourself from the fetched content.
- No telemetry. The server never sends user code, prompts, project files, or MCP client metadata upstream.
- `path` in `shopify_fetch` must already be indexed. Missing paths return `Path not found`; use `url` (with on-demand enabled) to recover.

## Work Loop

1. `shopify_status` to confirm the index is current enough for the task.
2. `shopify_map` with the most specific `from` you have (type name > doc path > free text).
3. Read the top 1–3 entries from `suggested_reading_order` via `shopify_fetch`.
4. Cite the concrete doc path in any follow-up answer.

## Troubleshooting Pointers

- Stdio `initialize` hangs → see [`docs/user/troubleshooting.md`](../../../docs/user/troubleshooting.md).
- Search returns nothing → check `shopify_status`, consider `shopify-rextant build --force`.
- Coverage gaps → `shopify-rextant coverage repair` (retries failed-but-in-scope rows).
