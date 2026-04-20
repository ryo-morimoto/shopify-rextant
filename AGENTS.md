# AGENTS.md

Guidance for AI coding agents working inside this repository.

## Project Identity

- Official name: `shopify-rextant`.
- Goal: local-first MCP server for Shopify developer documentation. Agents search and fetch source docs without repeat remote fetches.
- Primary language: Rust (edition 2024). TypeScript only if frontend/API tooling is introduced later.

## Source-First Rules

Before making design or implementation decisions, read in this order:

1. Current repository code and [`docs/dev/architecture.md`](docs/dev/architecture.md).
2. Local Shopify docs exposed by the `shopify-rextant` MCP, if available.
3. Official Shopify docs.
4. Community sources only when official sources are insufficient.

Cite the source used: file path and line number for local files, or the official docs URL.

## Critical Constraints

These are behavioral constraints, not preferences. Do not weaken them without explicitly revisiting the product boundary.

- **MCP stdio must support newline-delimited JSON.** Codex and `rmcp` terminate messages with `\n`; waiting for `Content-Length` framing causes 10s+ startup hangs. Keep `Content-Length` parsing only as backward compatibility.
- **Stdout is MCP protocol only.** Responses are one JSON object plus `\n`. Logs go to stderr via `tracing`.
- **No server-side LLM synthesis.** Node summaries are raw markdown prefixes. No paraphrase, no answer generation.
- **`shopify_fetch` preserves source text by default.** `anchor` and `include_code_blocks=false` are filters, not rewrites.
- **On-demand fetch is opt-in server-side only.** `enable_on_demand_fetch` lives in `config.toml`. There is no per-call MCP argument that lets an agent enable network fetches.
- **On-demand scope is hard-limited.** Only `shopify.dev/docs/**` and `shopify.dev/changelog/**`. Any other host, scheme, or Shopify path is rejected before any network request. See `src/url_policy.rs`.
- **Changelog impact SSoT is the index.** Candidates extracted from RSS text never flip `references_deprecated`, `upcoming_changes`, or `scheduled_changes.type_name` unless they resolve against `concepts` or `docs`.
- **Coverage requires `llms.txt` ∪ `sitemap.xml`.** `llms.txt` alone loses important pages (for example optional access scope docs).
- **Do not commit the empty `.codex` file** if it appears as untracked local metadata.

## Product Boundary (Out of Scope)

Unless the boundary is explicitly revisited:

- Server-side LLM summarization or answer synthesis
- Uploading user code, prompts, or project files
- Live Shopify store mutation
- Arbitrary URL fetching
- GraphQL / Liquid code validation
- Remote sharing or telemetry

## Development Commands

```bash
cargo test
cargo check
cargo build
```

Manual MCP smoke (prefer newline-delimited over `Content-Length`):

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  | target/debug/shopify-rextant serve --direct
```

Use an isolated home when testing indexing behavior:

```bash
SHOPIFY_REXTANT_HOME=/tmp/shopify-rextant-e2e cargo run -- build --limit 20
```

## Validation Expectations

Before committing implementation changes, run `cargo test`.

When touching MCP transport, also verify:

- `initialize` returns without a multi-second hang.
- `tools/list` succeeds and lists `shopify_map`, `shopify_fetch`, `shopify_status`.
- `tools/call` for `shopify_status` succeeds.

When touching indexing or search coverage, verify at least one query that previously required web fallback (for example managed/optional access scope queries) now resolves against the local index.

Full validation and release workflow: [`docs/dev/testing.md`](docs/dev/testing.md), [`docs/dev/release.md`](docs/dev/release.md).

## Git

- Use Conventional Commits: `feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `chore:`.
- Keep `.codex` out of commits unless it becomes intentional project configuration.
- Do not revert unrelated user changes.
