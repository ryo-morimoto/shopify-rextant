# Architecture

This document describes the implementation contract for `shopify-rextant`: design principles, the graph model, MCP tool contracts, the URL policy, and the storage schemas. It is the source of truth for behavior; roadmap and version history live in git history, not here.

## 1. Product Boundary

Fixed boundary — out of scope unless the boundary itself is revisited:

- Server-side LLM summarization or answer synthesis
- Uploading user code, prompts, or project files
- Live Shopify store mutation
- Arbitrary URL fetching (only `shopify.dev/docs/**` and `shopify.dev/changelog/**` are allowed, and only when the user opts in)
- GraphQL or Liquid code validation
- Remote sharing or telemetry

Keep these negatives in mind when evaluating any new feature.

## 2. Design Principles

1. **Map as response, not answer.** The tools return related nodes + edges + a reading-order hint. The agent decides what to read next.
2. **No synthesis, ever.** Node summaries are raw markdown prefixes. No LLM call, no paraphrase, no temperature.
3. **Staleness is a feature.** `age_days`, `references_deprecated`, and `upcoming_changes` are reported; the agent judges when to act.
4. **Deterministic algorithms only.** BFS, topological sort, BM25. Same index + same query ⇒ same output.
5. **Coverage before cleverness.** A missing document is worse than a clumsy ranking. Discovery uses `llms.txt` + `sitemap.xml` together; gaps are logged in `coverage_reports` rather than hidden.
6. **Local-first, offline-capable.** After the initial `build`, normal MCP reads do not touch the network. Refresh and on-demand recovery are explicit user actions.

## 3. Three-Layer Graph

Three graphs overlay the same index and are selected by `lens`:

- **Document graph** — page hierarchy, ordering, and "see also" links.
- **Concept graph** — GraphQL types/fields/mutations, Liquid objects, Function APIs, Polaris components, webhook topics. Built from Shopify's public `admin-graphql-direct-proxy` introspection.
- **Task graph** — tutorials and guides as implementation tasks, each linking to the concepts it teaches.

Edges use the `EdgeKind` enum: `DefinedIn`, `UsedIn`, `SeeAlso`, `ParentOf`, `Next`, `Prev`, `Replaces`, `Teaches`, `Requires`, `ComposedOf`, `HasField`, `Returns`, `AcceptsInput`, `Implements`, `MemberOf`, `ReferencesType`.

## 4. Runtime

```
┌─────────────────────────────────────────────────────┐
│ MCP client (Claude Code / Codex / ...)             │
└────────────────────────┬───────────────────────────┘
                         │ MCP stdio (JSON-RPC)
┌────────────────────────▼───────────────────────────┐
│ shopify-rextant (single Rust binary)               │
│   rmcp handler → query engine                      │
│     tools: shopify_map / shopify_fetch /           │
│            shopify_status                           │
│   query engine                                      │
│     entry resolver · BFS on petgraph ·             │
│     topological sort · staleness hydration         │
│   storage                                           │
│     tantivy (FTS) · SQLite WAL (metadata) ·        │
│     in-memory graph via Arc<ArcSwap<..>>           │
│   background tokio workers                          │
│     version · changelog · aging · edge repair      │
└────────────────────────┬───────────────────────────┘
                         │ HTTPS (reqwest, writes only)
                         ▼
                    shopify.dev
     /llms.txt · /sitemap.xml · /**/*.md
     /admin-graphql-direct-proxy/YYYY-MM · /changelog
```

Stdout is MCP protocol only. Logs go to stderr via `tracing`.

## 5. Stack Choices

| Layer | Crate |
|---|---|
| MCP SDK | `rmcp` |
| FTS | `tantivy` + `lindera-tantivy` (IPADIC for Japanese) |
| Metadata DB | `rusqlite` (bundled), WAL mode |
| HTTP | `reqwest` with `rustls` |
| RSS | `feed-rs` |
| Async runtime | `tokio` |
| Logging | `tracing` (stderr only) |

MCP stdio must support newline-delimited JSON — `Content-Length`-only parsing is retained for backward compatibility but is not the primary path. Codex and `rmcp` send `\n`-terminated messages; waiting for `Content-Length` causes 10s+ startup hangs.

## 6. MCP Tool Contracts

### 6.1 `shopify_map`

Input (JSON Schema, fields omitted for brevity):

- `from` (required, string): GraphQL type name, doc path, task id, or free-text query. The server decides.
- `radius` (1 | 2 | 3, default 2)
- `lens` (`concept` | `doc` | `task` | `auto`, default `auto`)
- `version` (API version pin, e.g. `2026-04`)
- `max_nodes` (1–100, default 30)

Output (shape abbreviated): `{ center, nodes[], edges[], suggested_reading_order, query_plan, meta }`.

Deterministic resolution order for `from`:

1. Prefix `/docs/` → `doc_path`.
2. Matches `tasks.id` → `task_name`.
3. Matches GraphQL identifier shape (`^[A-Z][A-Za-z0-9]*$`, or dotted form) and `concepts.name` → `concept_name`.
4. Otherwise → `free_text` (Tantivy top-k).

Contract invariants:

- `nodes` are dedup'd by `path`.
- `summary_from_source` is a raw markdown prefix (≤ 400 chars). No paraphrase.
- `edges` only reference nodes present in the response.
- `suggested_reading_order` lists doc paths, sorted by `{overview:0, tutorial:1, how-to:2, reference:3, migration:4}`, then topologically within each rank.
- `meta.graph_available = true` only when BFS actually ran over a populated concept graph for the resolved version.
- `meta.on_demand_candidate` appears only when the query returned zero nodes and the path is in allowed scope; its `enabled` field reflects the server config.

### 6.2 `shopify_fetch`

Input (exactly one of `path` or `url`):

- `path` (canonical docs path)
- `url` (on-demand; only `shopify.dev/docs/**` or `shopify.dev/changelog/**`)
- `anchor` (heading slug — return only the matching section)
- `include_code_blocks` (default `true` — source fidelity)
- `max_chars` (default 20000)

Output: `{ path, content, title, version?, staleness, sections?, truncated, source_url }`.

Contract invariants:

- `content` is verbatim markdown. `include_code_blocks=false` is a filter, not a rewrite.
- `path` must already be indexed — no implicit network fetch. Missing paths return `Path not found` with the current `doc_count`.
- `url` only fetches when `enable_on_demand_fetch = true`. Newly recovered docs are stored with `source = "on_demand"`; re-fetching an existing `llms`/`sitemap` doc does not downgrade its source.

### 6.3 `shopify_status`

No input. Returns `schema_version`, `doc_count`, `index_built`, per-source coverage counts, freshness tallies, worker timestamps, changelog counters, and `warnings[]`.

## 7. URL Policy

Implementation: `src/url_policy.rs` and `src/on_demand.rs`.

Accepted input for on-demand operations:

- `https://shopify.dev/docs/...`
- `https://shopify.dev/changelog/...`
- Canonical paths beginning with `/docs/` or `/changelog/`

Normalization strips `?query`, `#fragment`, trailing `/`, and `.md` / `.txt` suffixes. Anything outside the allowed prefixes returns `PolicyError::OutsideScope` before any network call is made, and the MCP layer maps this to JSON-RPC `-32008`.

`enable_on_demand_fetch` is a single server-side flag in `config.toml`. There is no per-call MCP argument that flips it. This is deliberate: the human operator opts in explicitly; the agent cannot.

## 8. Storage

### 8.1 SQLite (metadata, WAL)

Key tables — full DDL lives in `src/db/schema.rs`:

- **`docs`** — per-path metadata (`title`, `version`, `doc_type`, `api_surface`, `content_class`, `content_sha`, `etag`, `last_verified`, `freshness` ∈ `{fresh, aging, stale, rebuilding}`, `references_deprecated`, `deprecated_refs` JSON, `summary_raw`, `raw_path`, `source` ∈ `{llms, sitemap, on_demand, manual}`, `hit_count`).
- **`coverage_reports`** — discovered URLs that didn't make it into the index (status, reason, `retry_after`).
- **`concepts`** — graph nodes (`kind`, `name`, `version`, `defined_in_path`, `deprecated`, `kind_metadata` JSON).
- **`edges`** — typed edges between `concept`/`doc`/`task` nodes with `kind` and `weight`.
- **`tasks`** — tutorials/guides as task nodes with `related_paths`.
- **`changelog_entries`** — RSS-derived entries with `affected_types` (resolved) and `unresolved_affected_refs` (candidates).
- **`scheduled_changes`** — date-bound breaking changes extracted from changelog; `type_name` must resolve against `concepts` or `docs` before inclusion.
- **`indexed_versions`**, **`query_log`**, **`schema_meta`**.

`PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;` on open.

### 8.2 Tantivy (FTS)

Per-field tokenizers. Content is indexed into both `content_en` (UAX29 + lowercase + stemming) and `content_ja` (Lindera IPADIC); queries OR both fields so Japanese input does not suppress English type names. `title` and `related_concepts` use the default tokenizer. `path`/`version`/`doc_type`/`api_surface`/`content_class` are `STRING | STORED | INDEXED` for filtering. Raw document bodies live on disk under `data/raw/`, not in the FTS index — only the prefix (~2000 chars) is indexed.

### 8.3 In-memory graph

`petgraph::DiGraph<NodeData, EdgeData>` behind `Arc<ArcSwap<ConceptGraph>>`. MCP handlers never take a lock; background workers build a replacement graph and swap atomically. A `data/graph.msgpack` snapshot speeds cold starts (<10ms load vs. 100–300ms rebuild from SQLite).

## 9. Concurrency

- MCP handlers are read-only. All writes go through background workers.
- SQLite WAL lets multiple readers share one writer.
- Tantivy writers are single-threaded; readers refresh via `reload()`.
- Graph updates are wait-free on the read side via `ArcSwap`.

Failure modes:

- Upstream HTTP error → keep serving cached data; worker retries on next tick.
- SQLite / Tantivy corruption → user runs `shopify-rextant build --force`.
- Graph snapshot read failure → automatic fallback to SQLite rebuild.

## 10. Changelog Impact Resolution

Raw candidates extracted from RSS titles/bodies are *not* authoritative. The source of truth for impact is `concepts` / `docs` / `edges`:

- Only candidates that resolve to an existing `concepts.id` or `docs.path` enter `affected_types`, `scheduled_changes.type_name`, or flip `docs.references_deprecated`.
- Unresolved candidates are kept in `changelog_entries.unresolved_affected_refs` for auditing, but never leak into agent-visible staleness.
- `affected_surfaces` is derived from RSS categories plus resolved concepts/docs — never from free-form changelog prose.

## 11. Coverage Contract

- `llms.txt` alone is insufficient. The crawler always unions it with `sitemap.xml` (`/docs/**`, `/changelog/**`).
- Every discovered URL is normalized via `canonical_doc_path`; mismatches with `source_url.md` frontmatter become coverage warnings.
- URLs that fail to classify, fail to fetch, or lack a raw-markdown source remain in `coverage_reports` with a reason and `http_status`.
- `shopify_status` surfaces `coverage.skipped_count`, `coverage.failed_count`, `coverage.last_sitemap_at`, and per-source counts (`llms` / `sitemap` / `on_demand` / `manual`).

## 12. File Layout

```
<home>/
├── config.toml
├── data/
│   ├── index.db       # SQLite + WAL
│   ├── tantivy/       # FTS segments
│   ├── graph.msgpack  # petgraph snapshot (optional)
│   └── raw/           # verbatim markdown
└── logs/
```

Default `<home>` is `~/.shopify-rextant/`; override via `SHOPIFY_REXTANT_HOME` or `--home`.
