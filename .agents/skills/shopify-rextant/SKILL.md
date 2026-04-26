---
name: shopify-rextant
description: Use shopify-rextant, a local MCP server for Shopify developer documentation. Use when looking up Shopify Admin GraphQL types, Liquid references, Functions / Polaris guides, or changelog entries — so coding agents can get source-backed docs without remote round-trips.
---

# shopify-rextant

Local-first MCP server that maps Shopify developer documentation. Returns source-backed graphs and raw markdown — never synthesized answers.

## Tools

### `shopify_map`

First call for any investigation. Returns a map of related nodes.

| arg | type | notes |
|---|---|---|
| `from` | string, required | GraphQL type name (`Product`), doc path (`/docs/...`), task id, or free-text query |
| `radius` | 1 \| 2 \| 3, default 2 | BFS hop depth |
| `lens` | `concept` \| `doc` \| `task` \| `auto`, default `auto` | graph axis |
| `version` | string | API version pin, e.g. `2026-04` |
| `max_nodes` | integer 1–100, default 30 | cap on returned nodes |

Response contains: `center`, `nodes[]` (with `summary_from_source` and `staleness`), `edges[]`, `suggested_reading_order`, `query_plan`, and `meta`.

### `shopify_fetch`

Returns the raw markdown of one document.

| arg | type | notes |
|---|---|---|
| `path` | string | canonical docs path (from `shopify_map`) |
| `url` | string | on-demand: `shopify.dev/docs/**` or `shopify.dev/changelog/**` only |
| `anchor` | string | return only the matching section |
| `include_code_blocks` | boolean, default `true` | set `false` to strip fenced code |
| `max_chars` | integer, default 20000 | truncation cap |

Exactly one of `path` or `url` is required. `url` triggers on-demand recovery and requires `enable_on_demand_fetch = true` in `~/.shopify-rextant/config.toml`.

### `shopify_status`

No input. Returns doc counts, coverage buckets, freshness tallies, worker timestamps, and `warnings[]`.

## Tool Selection

- **`shopify_map`** — always call first. Pass the most specific `from` you have (type name > doc path > free text).
- **`shopify_fetch`** — follow-up to read the raw markdown for one path from the map. Use `anchor` to narrow to a section.
- **`shopify_status`** — call when results look sparse or stale.

Do not treat `summary_from_source` as an answer — it is a raw prefix excerpt. Read the full doc via `shopify_fetch` before concluding.

## Interpreting `shopify_map.meta`

- `graph_available: false` — the concept graph was not used. `edges[]` will be empty; fall back to `nodes[]` FTS candidates.
- `coverage_warning` — the index is stale or incomplete. Follow the hint (`run shopify_refresh`, rebuild, etc.).
- `on_demand_candidate` — the query returned zero nodes and the path is recoverable. `enabled: false` means the user has not opted in; tell them to set `enable_on_demand_fetch = true` in `~/.shopify-rextant/config.toml`.

## Work Loop

1. `shopify_status` to confirm the index is current enough for the task.
2. `shopify_map` with the most specific `from` you have.
3. Read the top 1–3 entries from `suggested_reading_order` via `shopify_fetch`.
4. Cite the concrete doc path in any follow-up answer.

## Error Handling

| JSON-RPC code | meaning | action |
|---|---|---|
| `-32007` | on-demand fetch disabled | Instruct the user to set `enable_on_demand_fetch = true` in config. Do not work around it. |
| `-32008` | URL outside allowed scope | Only `shopify.dev/docs/**` and `shopify.dev/changelog/**` are accepted. Pick a different URL. |

## Non-Obvious Facts

- Responses are deterministic. Same index + same query ⇒ same output.
- Stdout is protocol-only. Server logs go to stderr.
- Summaries are raw markdown prefixes — never synthesized. Produce your own summary from fetched content.
- `path` in `shopify_fetch` must already be indexed. Missing paths return `Path not found`; use `url` (with on-demand enabled) to recover.
- `graph_available: false` is expected when the query can't resolve to an indexed Admin GraphQL concept. Follow `query_plan[0].action` instead of treating empty `edges` as a bug.
- Search returns nothing → check `shopify_status`, suggest `shopify-rextant build --force` to the user.
