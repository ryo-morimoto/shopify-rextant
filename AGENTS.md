# AGENTS.md

## Project Identity

- Official project name: `shopify-rextant`.
- Primary goal: provide a local-first MCP server for Shopify developer documentation so coding agents can search and fetch source docs without repeated remote fetches.
- Current implementation status: v0.1 is implemented; `SPEC.md` is now tracking v0.1.1 as the next target.
- Primary languages: Rust for the MCP server and indexing backend; TypeScript only if frontend/API tooling is introduced later.

## Source-First Rules

Before making design or implementation decisions, read the relevant source in this order:

1. Current repository code and `SPEC.md`
2. Local Shopify docs exposed by the `shopify-rextant` MCP, if available
3. Official Shopify docs
4. Community sources only when official sources are insufficient

When answering or changing behavior, cite the source used: file path and line number for local files, or official docs URL.

## Critical Project Learnings

- MCP stdio transport must support newline-delimited JSON. Codex and `rmcp` use JSON messages terminated by `\n`; waiting only for `Content-Length` framing causes apparent 10s+ startup hangs. Keep Content-Length parsing only as backward compatibility.
- The server must write MCP responses as one JSON object plus `\n` on stdout. Logs must stay on stderr.
- `llms.txt` is not enough for Shopify docs coverage. Important pages, including optional access scope related docs, can be absent from `llms.txt`. v0.1.1 must add `sitemap.xml` discovery and coverage reporting.
- v0.1 `shopify_map` is FTS-backed, not a real graph map. It must expose that honestly with `graph_available=false`, query interpretation, and coverage warnings. Real concept/doc/task graph behavior belongs to v0.2.
- `shopify_fetch` should preserve source text by default. Section extraction via `anchor` and `include_code_blocks=false` are convenience filters, not summarization.
- Never add server-side LLM summarization or answer synthesis. The tool returns source-backed maps and raw docs, not generated answers.
- On-demand URL fetch is heavier than a patch fix because it touches network policy, DB upsert, raw cache, and tantivy delta indexing. Keep it in v0.5 unless explicitly reprioritized.
- Do not commit the empty `.codex` file if it appears as untracked local metadata.

## Roadmap Allocation Rule

When new requirements are found but not yet explicitly represented in `SPEC.md`:

- If they belong to an existing feature area, add them to that feature's version.
- If they do not belong to an existing feature area and are lightweight, schedule them for `v0.1.1`.
- If they are heavy or introduce new boundaries, schedule them for a new `v0.5`-style milestone.
- If they require credentials, mutate a real Shopify store, validate user code, or provide remote sharing, treat them as out of scope unless the user explicitly changes the product boundary.

## Development Commands

Use these from the repository root:

```bash
cargo test
cargo check
cargo build
```

For manual MCP smoke tests, prefer newline-delimited JSON over Content-Length framing:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  | target/debug/shopify-rextant serve
```

For Codex MCP registration during local testing:

```bash
codex mcp add shopify-rextant -- /absolute/path/to/target/debug/shopify-rextant --home /tmp/shopify-rextant-e2e serve
```

## Validation Expectations

Before committing implementation changes, run:

```bash
cargo test
```

When touching MCP transport, also verify:

- `initialize` returns without a multi-second hang.
- `tools/list` succeeds.
- `tools/call` for `shopify_status` succeeds.

When touching indexing or search coverage, verify at least one query that previously required web fallback, such as optional access scopes / managed access scopes related wording.

## Git

- Use Conventional Commits, for example `feat:`, `fix:`, `docs:`, `test:`, `chore:`.
- Keep `.codex` out of commits unless it becomes intentional project configuration.
- Do not revert unrelated user changes.

## 選好ログ（L）

- Product naming: `shopify-rextant` is the official name.
- Scope allocation: unrepresented work should be grouped with the same feature timing; otherwise lightweight work goes to `v0.1.1`, heavy new work goes to `v0.5`.
- Product boundary: local-first, zero telemetry, no LLM synthesis, no live Shopify store mutations.
- v0.2 graph scope: build the first real graph map from Admin GraphQL concepts/docs; keep Storefront GraphQL, Liquid, Functions, and Polaris concept extraction as follow-up work on the same graph foundation.
- Schema source: use Shopify's public `shopify.dev/admin-graphql-direct-proxy/{version}` introspection for index builds; do not hit authenticated store Admin API endpoints.
- v0.3 changelog impact: extract candidates from changelog text, but use `concepts` / `docs` / `edges` as the SSoT; unresolved candidates must not mark docs deprecated.
- v0.3 version watcher: run as a `serve` background worker; use the official versioning page only for candidates, then validate availability with Shopify's public Admin GraphQL direct proxy before queueing a rebuild.
- v0.3 implementation shape: first preserve the current single-file implementation with explicit internal boundaries and fixture-backed contracts; split modules only after behavior is covered.
- v0.4 tokenizer: keep `lindera` / IPADIC always-on; use separate `content_en` and `content_ja` Tantivy fields and query both so Japanese queries do not replace English type/path search. Pin Tantivy to the version compatible with the selected `lindera-tantivy` release.
- v0.4 edge repair: keep an `edge_candidates` table for idempotency and accepted/rejected state, then automatically insert evidence-backed repaired edges; if precision is poor, tighten evidence rules rather than making repair candidate-only.
- v0.4 diagnostics: expose detailed low-hit query, edge candidate, and repair reports through a CLI `diagnostics` command; keep `shopify_status` limited to lightweight counts, timestamps, and warnings.
- v0.5 on-demand fetch: default network use to disabled, allow only explicit local configuration, and do not expose a per-call MCP argument that lets an agent enable network fetches.

## 未確定ドメイン（U）

- None currently recorded.
