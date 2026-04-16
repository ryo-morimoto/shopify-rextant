---
name: shopify-rextant-dev
description: Work on the shopify-rextant Rust MCP server and its SPEC.md. Use when implementing, debugging, planning, or reviewing shopify-rextant behavior, especially MCP stdio transport, Shopify docs indexing, search coverage, roadmap allocation, or local validation.
---

# shopify-rextant Development

## First Reads

1. Read `AGENTS.md`.
2. Read the relevant section of `SPEC.md`.
3. Read the code that implements the behavior, usually `src/main.rs` in the current v0.1 implementation.

Do not decide from memory when a local source can answer the question.

## Non-Obvious Facts

- Official name is `shopify-rextant`.
- MCP stdio must handle newline-delimited JSON. A Content-Length-only server can appear to hang for 10s+ under Codex.
- stdout is protocol-only. Put logs on stderr.
- v0.1 `shopify_map` is FTS-backed and must not pretend to be a graph. Expose `graph_available=false` until v0.2 graph work lands.
- `llms.txt` alone is insufficient. v0.1.1 work should use `sitemap.xml` plus coverage reporting.
- On-demand URL fetch belongs to v0.5 unless the user explicitly pulls it forward.
- Do not commit an empty untracked `.codex` file.

## Work Loop

1. Identify the SPEC version bucket for the requested change.
2. Read the code and local docs involved.
3. Make the smallest implementation or SPEC edit that matches the bucket.
4. Run `cargo test` before committing implementation changes.
5. For MCP transport changes, run a newline-delimited JSON initialize smoke test.
6. Cite local files and lines in the final answer.

## Roadmap Bucketing

- Same feature area: add to the same milestone.
- Lightweight uncovered work: `v0.1.1`.
- Heavy new boundary or network/index mutation: `v0.5`.
- Real store mutation, credentials, code validation, remote sharing, or LLM answer synthesis: out of scope unless explicitly redefined.
