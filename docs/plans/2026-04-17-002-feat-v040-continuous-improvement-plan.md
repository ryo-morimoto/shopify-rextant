---
title: feat: Implement v0.4.0 continuous improvement and release readiness
type: feat
status: active
date: 2026-04-17
---

# feat: Implement v0.4.0 continuous improvement and release readiness

## Overview

Implement `SPEC.md` v0.4.0 by adding a local-only learning loop for search and graph quality, then making the project easier to install, benchmark, and contribute to. The key behavior is not remote telemetry: `query_log` is a private SQLite table inside the user's local `SHOPIFY_REXTANT_HOME`, used only to improve local search rules and graph edges.

v0.4 combines behavior changes and distribution work. Keep those tracks separated so query logging and repair behavior can be validated with fixtures before release packaging changes are layered on.

## Goal Model

- Purpose: let `shopify-rextant` improve missing graph edges and weak searches from real local usage patterns, while preparing the binary for repeatable release and installation.
- Non-goals: no remote telemetry, no LLM synthesis, no automatic skill/workflow rewriting, no on-demand URL fetch, no live Shopify store calls, no remote team server.
- Constraints: logs stay local; MCP stdout must remain protocol-only; missing-edge repair must be candidate-driven and evidence-checked against local raw docs; v0.5 owns on-demand fetching and coverage repair.
- Minimum: create and populate `query_log`, detect map-then-fetch missing-edge candidates, run an idempotent `edge_repairer`, generate low-hit query improvement candidates, integrate Japanese search tokenization, add benchmarks, add release packaging, and write README/CONTRIBUTING.
- Verification criteria: each v0.4 checklist item in `SPEC.md` has at least one fixture-backed or command-level test, and end-to-end MCP smoke tests still cover `initialize`, `tools/list`, `shopify_status`, `shopify_map`, and `shopify_fetch`.

## Requirements Trace

- R1. Add `query_log` persistence for `shopify_map` and `shopify_fetch`.
- R2. Detect missing-edge candidates from `query_log` where a map result is immediately followed by a fetch for a doc that was not returned.
- R3. Add `edge_repairer` on a 72h worker cadence, plus callable repair logic for tests and manual validation.
- R4. Only insert repaired edges when local raw docs contain evidence for the suspected target.
- R5. Extract low-hit query patterns from `query_log` and persist or report search-rule improvement candidates.
- R6. Integrate Japanese tokenizer support for Tantivy content search using the SPEC's English-plus-Japanese indexing shape.
- R7. Add benchmarks and tuning checks for search/index behavior.
- R8. Add GitHub Actions release, Homebrew tap guidance/artifact path, and Nix flake support.
- R9. Add README and CONTRIBUTING documentation for install, local MCP registration, development, validation, and release.
- R10. Preserve local-first, zero telemetry, no synthesis, and no live store mutation boundaries.

## Scope Boundaries

- Do not send query logs outside the local machine.
- Do not store prompts, full fetched content, user code, or generated answers in `query_log`.
- Do not automatically apply low-hit query rules if they are ambiguous; produce candidates first.
- Do not add URL fetch for missing docs. `shopify_fetch.url` remains disabled until v0.5.
- Do not make Homebrew or Nix the only install path. Cargo/local binary remains valid.
- Do not split `src/main.rs` into modules as part of v0.4 unless the implementing diff becomes unsafe to review. Preserve the current single-file shape with explicit internal boundaries first.

## Context & Research

### Local Sources Read

- `SPEC.md:321` defines `query_log` as local learning storage for `shopify_map` and `shopify_fetch` arguments, returned IDs, latency, and optional client info.
- `SPEC.md:350` defines the intended Tantivy schema shape, including `content_en` and `content_ja` fields.
- `SPEC.md:1102` defines `edge_repairer` as a 72h worker that detects missing edges from `query_log` and reparses local raw docs before inserting edges.
- `SPEC.md:1727` lists v0.4 checklist items: edge repair, `lindera`, Homebrew/GitHub release, Nix flake, benchmarks, docs, low-hit query extraction, and map-then-fetch missing-edge candidates.
- `SPEC.md:1767` explicitly assigns low-hit query extraction and map-then-fetch missing edge candidates to v0.4.
- `src/main.rs:905` and `src/main.rs:945` are the current `shopify_fetch` and `shopify_map` boundaries where query logging should be instrumented.
- `src/main.rs:81` shows the current CLI has `Serve`, `Build`, `Refresh`, `Status`, `Search`, `Show`, and `Version`; no v0.4 diagnostics or repair command exists yet.
- `src/main.rs:3640` creates `docs`, including `hit_count`, but `query_log` is not currently created in `init_db`.
- `src/main.rs:1913` orders SQLite fallback search by `hit_count`, so v0.4 can either increment `hit_count` from map returns or leave it as a later tuning item. Do not assume it is already maintained.

### External Sources To Read Before Implementation

These are intentionally not fixed in this plan. Before implementing release packaging details, read official docs and cite them in the implementation notes:

- GitHub Actions release artifacts and release creation docs from `docs.github.com`.
- Homebrew Formula Cookbook and tap guidance from `docs.brew.sh`.
- Nix flakes/package/devShell references from official Nix or nix.dev documentation.
- `lindera` / `lindera-tantivy` and Tantivy tokenizer API docs from docs.rs for the exact compatible crate versions.

## Approach Landscape

| Approach | How it works | Strength | Cost |
|---|---|---|---|
| A. One large v0.4 PR | Implement logging, repair, tokenizer, benchmarks, CI, Homebrew, Nix, and docs together | Single release branch | High review risk; hard to isolate regressions |
| B. Data-plane first, release-plane second | Implement `query_log`, analysis, repair, tokenizer, and benchmarks first; add packaging/docs after behavior is stable | Keeps behavioral contracts testable before distribution | More milestones inside v0.4 |
| C. Release-plane first | Add CI/Homebrew/Nix/docs first, then improve search behavior | Early installability | Ships packaging around unstable v0.4 behavior |

Chosen: Approach B. `query_log` and `edge_repairer` alter runtime behavior and require fixture-backed tests. Packaging should come after the binary behavior is stable.

## Key Technical Decisions

- Treat `query_log` as local product instrumentation, not telemetry. It stores normalized tool args, result IDs, latency, and generic client identity only.
- Instrument `shopify_map` and `shopify_fetch` at their public tool boundaries so CLI and MCP paths share the same logging behavior.
- Store returned IDs as canonical docs paths and concept IDs. For `shopify_fetch`, store the fetched doc path as the single returned ID when successful.
- Detect missing-edge candidates from adjacent `query_log` entries with a conservative time window. Without a session ID, candidates are only suggestions unless local raw docs confirm the relationship.
- Add an `edge_candidates` table before repair insertion. This is required for idempotency, accepted/rejected state, and avoiding state loss between worker runs.
- Insert an edge automatically when local source evidence exists, such as a markdown link, canonical path, title mention, or concept identifier in the source doc. If validation shows poor precision, tighten the evidence rules rather than weakening the feature to candidate-only output.
- Keep low-hit query analysis candidate-based. It can report zero-result queries, low-confidence map responses, and repeated map-then-fetch misses, but it must not rewrite ranking rules automatically.
- Expose detailed improvement reports through a new CLI `diagnostics` command. Keep `shopify_status` for lightweight health summaries and counts.
- Integrate Japanese search by adding a second content field or tokenizer path matching the SPEC. Preserve English/type-name search quality by querying English and Japanese fields together rather than replacing the existing tokenizer.
- Keep benchmarks local and deterministic. Avoid network in benchmark runs; use fixture indexes or generated local docs.

## Settled Design Decisions

- `edge_candidates` is part of the v0.4 design. It prevents direct writes from weak `query_log` signals to `edges`, preserves repair state across runs, and supports idempotent accepted/rejected handling.
- `diagnostics` is exposed as a CLI command for low-hit queries, edge candidates, and repair details. `shopify_status` should expose only lightweight counts, timestamps, and warnings.
- `edge_repairer` should perform repair, not just candidate generation. Evidence-backed candidates are inserted into `edges`; precision problems should be handled by improving evidence rules and tests.

## Open Questions

- Should `lindera` be always-on or feature-gated? Default implementation recommendation: follow `SPEC.md` default `enable_japanese = true`, but keep a build/config escape hatch if binary size or dependency compatibility becomes a problem.

### Deferred to Implementation Source Check

- Exact `lindera` crate set and Tantivy tokenizer registration API.
- Exact GitHub Actions release workflow structure and permissions.
- Whether the Homebrew formula lives in this repository as a template or a separate tap repository.
- Exact Nix flake inputs and supported systems.

## High-Level Technical Design

```text
shopify_map / shopify_fetch
  -> record_query_log(tool, args, returned_ids, latency_ms, client_info)
  -> analyze_query_log()
       -> low_hit_query_candidates
       -> edge_candidates
  -> repair_edges()
       -> verify candidate against local raw docs
       -> insert_edge_if_missing()
       -> mark candidate accepted/rejected
  -> shopify_map graph expansion benefits from repaired edges
```

Pure logic boundary:

```text
normalize tool args, compute returned IDs, detect low-hit queries,
detect map-then-fetch misses, verify edge evidence in markdown/text,
tokenize/query fixture text, compute benchmark summaries
```

Side-effect boundary:

```text
SQLite writes, Tantivy index writes, worker scheduling, CLI output,
GitHub release jobs, Homebrew formula publishing, Nix build evaluation
```

## Implementation Units

- [ ] **Unit 1: Query Log Schema and Tool Boundary Logging**

**Goal:** Create local `query_log` storage and instrument `shopify_map` / `shopify_fetch`.

**Requirements:** R1, R10

**Dependencies:** None

**Files:**
- Modify: `src/main.rs`
- Test: `src/main.rs`

**Approach:**
- Bump schema version if the project uses it for compatibility checks.
- Extend `init_db` with `query_log` and indexes from `SPEC.md`.
- Add a small `ToolObservation` / `QueryLogEntry` internal record.
- Wrap map/fetch public functions or their MCP call sites with timing and returned-ID capture.
- Redact args to the minimum needed:
  - map: `from`, `lens`, `radius`, `max_nodes`, `version`
  - fetch: `path`, `anchor`, `include_code_blocks`, `max_chars`; never content
  - URL fetch remains disabled, so do not log arbitrary URL fetch attempts beyond the disabled intent if needed for diagnostics.
- Optionally increment `docs.hit_count` for doc paths returned by `shopify_map` and `shopify_fetch`, but keep this covered by tests if implemented.

**Test scenarios:**
- `shopify_map` writes one `query_log` row with returned docs/concepts.
- `shopify_fetch` writes one `query_log` row with the fetched path.
- Failed `shopify_fetch` records either no row or a row with empty `returned_ids`; choose one behavior and test it explicitly.
- Args redaction does not include fetched content.
- Logging failure does not break successful MCP tool response unless DB is unavailable for the tool itself.

- [ ] **Unit 2: Missing Edge Candidate Detection**

**Goal:** Convert map-then-fetch usage patterns into durable missing-edge candidates.

**Requirements:** R2, R5

**Dependencies:** Unit 1

**Files:**
- Modify: `src/main.rs`
- Test: `src/main.rs`

**Approach:**
- Add `edge_candidates` table with fields such as `from_id`, `to_path`, `source_query_log_ids`, `reason`, `evidence`, `status`, `created_at`, `updated_at`.
- Use `edge_candidates` as a durable state machine for idempotency and state-loss avoidance, not as the final product surface.
- Detect candidate when:
  - a `shopify_map` row is followed by a `shopify_fetch` row within a small window,
  - the fetched path was not in map `returned_ids`,
  - there is at least one plausible source doc or concept from the map result to connect from.
- Prefer candidates from the first map result or center node as `from_id`; if ambiguous, persist multiple candidates with lower confidence rather than guessing silently.
- Deduplicate candidates by `(from_id, to_path, reason)`.

**Test scenarios:**
- Map returns doc A, immediate fetch B: creates candidate A -> B.
- Map returns doc A, immediate fetch A: no candidate.
- Fetch outside the time window: no candidate.
- Repeated same pattern: one deduped candidate with updated count/timestamp.
- Separate client info, if available, does not cross-link sessions.

- [ ] **Unit 3: Edge Repairer Worker and Repair Logic**

**Goal:** Add idempotent repair that accepts candidates only with local source evidence.

**Requirements:** R3, R4

**Dependencies:** Unit 2

**Files:**
- Modify: `src/main.rs`
- Test: `src/main.rs`

**Approach:**
- Add pure `verify_edge_evidence(from_doc_text, to_doc)` logic.
- Accept evidence from markdown links, canonical path mentions, exact title mentions, or exact concept identifier mentions.
- Add `repair_edges(paths)` that loads pending candidates, verifies local raw docs, inserts missing `edges`, and marks candidates `accepted` or `rejected`.
- Repair is part of the feature. Do not stop at candidate generation when evidence is sufficient.
- Wire `edge_repairer` as a background worker in `serve`, using the `edge_repairer_interval_hours = 72` default from SPEC.
- Expose last run and candidate counts in `shopify_status`.
- Expose detailed repair and candidate information through the `diagnostics` CLI.

**Test scenarios:**
- Candidate with markdown link in raw doc inserts one `SeeAlso` edge.
- Candidate without evidence is rejected or kept pending based on chosen status model.
- Existing edge is not duplicated.
- Worker status records last run and warnings.
- MCP `shopify_status` remains protocol-safe and does not emit logs to stdout.

- [ ] **Unit 4: Low-Hit Query Improvement Candidates**

**Goal:** Use `query_log` to surface search-rule improvement candidates without automated rewriting.

**Requirements:** R5, R10

**Dependencies:** Unit 1

**Files:**
- Modify: `src/main.rs`
- Test: `src/main.rs`

**Approach:**
- Define low-hit signals:
  - `shopify_map` returned zero nodes,
  - `shopify_map.meta.query_interpretation.confidence = "low"`,
  - repeated map-then-fetch miss patterns,
  - repeated queries where only SQLite fallback found results if detectable.
- Add a report function that groups by normalized query and suggests a candidate reason.
- Keep candidate output source-backed and mechanical, for example `add_alias`, `add_token_rule`, `add_edge`, or `inspect_missing_doc`.
- Surface detailed reports through the `diagnostics` CLI. Keep `shopify_status` limited to summary counts and worker health.

**Test scenarios:**
- Repeated zero-result query appears in report.
- High-confidence exact doc path query does not appear.
- Map-then-fetch miss links the query to an edge candidate.
- Report is stable and sorted by count / latest timestamp.

- [x] **Unit 5: Japanese Tokenizer Integration**

**Goal:** Implement the SPEC's English-plus-Japanese search indexing path.

**Requirements:** R6

**Dependencies:** Unit 1 can run independently, but this unit should be isolated because dependency/API compatibility risk is higher.

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `src/main.rs`
- Test: `src/main.rs`

**Approach:**
- Read `lindera`, `lindera-tantivy`, and Tantivy tokenizer official docs before choosing exact dependencies.
- Add `content_en` and `content_ja` or equivalent fields without breaking existing index reads.
- Register Japanese tokenizer at index creation and query parsing time.
- Query both English and Japanese content fields and combine results without degrading exact type/path searches.
- Keep fixture tests independent of network and deterministic.

**Test scenarios:**
- Japanese query fixture such as `割引 クーポン` can match a fixture doc whose English docs contain relevant title/path/summary plus Japanese test text.
- Existing English type-name query still returns the expected doc.
- Rebuilding an old index after schema change succeeds or returns a clear rebuild-required error.
- Binary builds with the selected tokenizer dependency set.

- [ ] **Unit 6: Benchmarks and Tuning**

**Goal:** Add repeatable local performance checks for search, fetch, graph expansion, and repair analysis.

**Requirements:** R7

**Dependencies:** Units 1-5 for full coverage; a first benchmark harness can start earlier.

**Files:**
- Modify: `Cargo.toml`
- Add: `benches/search.rs` or equivalent benchmark harness
- Add: `docs/benchmarks.md` if benchmark interpretation needs docs

**Approach:**
- Prefer fixture/local generated data over network-built indexes.
- Benchmark:
  - `shopify_map` exact path,
  - `shopify_map` free-text search,
  - graph expansion with edges,
  - `shopify_fetch` raw read and section extraction,
  - query-log analysis over synthetic logs.
- Record baseline numbers in docs only if reproducible enough on developer machines.

**Test scenarios:**
- Benchmark harness compiles.
- Unit tests cover benchmark fixture generation.
- CI can run compile-only or a short smoke mode, not full benchmarks on every PR unless runtime is acceptable.

- [ ] **Unit 7: Release Packaging and Nix**

**Goal:** Make the binary releaseable and installable through GitHub release artifacts, Homebrew, and Nix.

**Requirements:** R8

**Dependencies:** Behavioral units should be green first.

**Files:**
- Add: `.github/workflows/ci.yml`
- Add: `.github/workflows/release.yml`
- Add: `flake.nix`
- Add or document: Homebrew formula/tap template path
- Modify: `Cargo.toml` if release metadata is missing

**Approach:**
- Read official GitHub Actions, Homebrew, and Nix docs before implementation.
- Add CI jobs for format/check/test/build on supported targets.
- Add release workflow triggered by tags, with built artifacts and checksums.
- Add `flake.nix` with package and dev shell.
- Decide whether Homebrew formula is generated into this repo or published to a separate tap. If separate tap is required, document exact manual/CI step rather than pretending it is fully automated here.

**Test scenarios:**
- `cargo test` passes in CI definition.
- Local `nix flake check` passes where Nix is available.
- Release workflow can be syntax-checked or dry-run validated.
- Homebrew formula passes `brew audit --strict` / `brew test-bot` only after official docs confirm the local validation command.

- [ ] **Unit 8: Documentation Site and Contributor Docs**

**Goal:** Add enough docs for users and future agents to install, register, validate, and contribute.

**Requirements:** R9, R10

**Dependencies:** Unit 7 for install commands; can draft earlier with placeholders.

**Files:**
- Add: `README.md`
- Add: `CONTRIBUTING.md`
- Add: `docs/architecture.md` if the README becomes too long
- Modify: `AGENTS.md` only if new settled implementation preferences emerge

**Approach:**
- README should cover project purpose, install paths, local MCP registration, build/index lifecycle, tool list, privacy/local-first boundary, and troubleshooting.
- CONTRIBUTING should cover setup, validation commands, test fixture expectations, release process, and source-first expectations.
- Keep user-facing docs honest about what v0.4 does not do: no LLM synthesis, no store mutation, no on-demand URL fetch until v0.5.

**Test scenarios:**
- Commands in README are copy-paste valid after implementation.
- `cargo test`, `cargo build`, and MCP smoke test are documented.
- Docs mention that logs and query learning stay local.

## Suggested Sequence

1. Unit 1: Query log schema and tool boundary logging.
2. Unit 2: Missing edge candidate detection.
3. Unit 3: Edge repairer worker and status visibility.
4. Unit 4: Low-hit query report.
5. Unit 5: Japanese tokenizer integration.
6. Unit 6: Benchmarks and tuning.
7. Unit 7: Release packaging and Nix.
8. Unit 8: README and CONTRIBUTING.

If implementation needs smaller PRs, split after Unit 4:

- PR A: local learning loop (`query_log`, candidates, edge repair, low-hit report)
- PR B: tokenizer and benchmarks
- PR C: release packaging and docs

## Validation Plan

Run these before considering v0.4 complete:

```bash
cargo fmt
cargo test
cargo build
```

When tokenizer dependencies land:

```bash
cargo test tokenizer
cargo test search
```

When packaging lands:

```bash
nix flake check
```

E2E smoke test:

```bash
tmp_home="$(mktemp -d)"
cargo build
target/debug/shopify-rextant --home "$tmp_home" build --limit 3
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"v040-smoke","version":"0"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"shopify_status","arguments":{}}}' \
  '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"shopify_map","arguments":{"from":"discount","max_nodes":5}}}' \
  | target/debug/shopify-rextant --home "$tmp_home" serve
```

Add one additional E2E after Unit 3:

- Run `shopify_map` for a fixture query.
- Run `shopify_fetch` for a doc not returned by the map.
- Run diagnostics or repair.
- Confirm an edge candidate is created, and a high-confidence candidate becomes an edge only with source evidence.

## Risks and Mitigations

- Risk: `query_log` accidentally becomes telemetry. Mitigation: no network path, no upload command, no prompt/content storage, and docs explicitly state local-only behavior.
- Risk: map-then-fetch creates false-positive edges. Mitigation: candidate table first, deterministic raw-doc evidence required before insertion.
- Risk: `lindera` dependency/API mismatch with current Tantivy. Mitigation: isolate tokenizer unit, read docs.rs before implementation, and keep English search behavior covered by tests.
- Risk: Homebrew tap automation needs a separate repository. Mitigation: document the boundary and generate formula/checksums without claiming full tap publication unless the repo exists.
- Risk: Nix flake support can fight current local environment. Mitigation: make `cargo` path authoritative and keep flake as an additional reproducibility layer.
- Risk: Benchmarks become flaky. Mitigation: benchmark fixtures are local and CI runs compile/smoke unless full benchmark runtime is explicitly accepted.

## Completion Checklist

- [ ] `SPEC.md` v0.4 checklist items are implemented or explicitly deferred with rationale.
- [ ] `query_log` rows are created for map/fetch and contain no fetched content or prompts.
- [ ] Edge candidates are generated from map-then-fetch misses.
- [ ] `edge_repairer` inserts only evidence-backed edges and is idempotent.
- [ ] Low-hit query candidates are reportable.
- [x] Japanese search path is covered by tests and does not regress English/type searches.
- [ ] Benchmarks compile and have documented usage.
- [ ] GitHub Actions release workflow, Homebrew guidance, and Nix flake are validated against official docs.
- [ ] README and CONTRIBUTING exist and match actual commands.
- [ ] `cargo test`, `cargo build`, and MCP E2E smoke tests pass.

## Session Preference Updates

At the end of implementation, update `AGENTS.md` if any of these become settled preferences:

- diagnostics public surface (`status`, CLI, or both)
- edge repair auto-insert threshold
- tokenizer default / feature gate decision
- Homebrew tap ownership and release automation shape
- Nix flake support policy
