---
title: "feat: implement v0.5.0 on-demand official docs fetch"
type: feat
status: implemented
date: 2026-04-18
---

# v0.5.0 On-Demand Official Docs Fetch Plan

## 0. Implementation Result

Status: implemented on 2026-04-18.

Validated:

- `cargo test`
- `cargo build`
- MCP smoke: `initialize`, `tools/list`, `shopify_status`
- MCP disabled fetch smoke: `shopify_fetch(url)` returns `-32007` without network access
- MCP enabled real-network smoke: `shopify_fetch(url=https://shopify.dev/docs/apps/build/accessibility)` fetches official raw markdown, indexes it as `source="on_demand"`, and subsequent `search accessibility` finds the recovered doc
- CLI help smoke: `refresh --url` and `coverage repair`

Implementation source:

- CLI shape: `src/main.rs` defines `refresh [PATH]`, `refresh --url`, and `coverage repair`.
- On-demand pipeline: `src/main.rs` validates policy/config, fetches raw source, stores raw cache, upserts `docs`, records coverage when requested, and updates Tantivy.
- Map candidate behavior: `src/main.rs` returns conservative `on_demand_candidate` metadata only when local map results are empty.
- Test coverage: `src/main.rs` contains focused tests for disabled policy, out-of-scope policy, enabled fetch, unindexed path recovery, source precedence, Tantivy delta replacement, map candidates, and coverage repair.

## 1. Goal Model

### Purpose

v0.5.0 makes `shopify-rextant` recover missing official Shopify documentation locally when an agent already knows the exact URL or canonical docs path. This closes the gap where a relevant Shopify page exists, but it was absent from `llms.txt`, sitemap discovery, or the local index.

### Non-goals

- Do not build a general-purpose arbitrary URL fetcher.
- Do not fetch non-Shopify hosts.
- Do not fetch Shopify app/store Admin API endpoints.
- Do not mutate live Shopify stores.
- Do not add server-side LLM summarization or answer synthesis.
- Do not automatically web-search for unknown docs.
- Do not make `shopify_map` perform network fetches.

### Constraints

- On-demand fetch is limited to `https://shopify.dev/docs/**` and `https://shopify.dev/changelog/**`.
- Network use is gated by explicit local configuration. Default is disabled.
- MCP stdio behavior stays newline-delimited JSON responses on stdout, with logs on stderr.
- Raw source text remains the source of truth. Fetch returns source-backed text, not generated answers.
- A successful on-demand fetch must keep raw cache, SQLite `docs`, coverage metadata, and Tantivy index consistent.
- On-demand documents must be marked with `source = "on_demand"` until a full build later discovers the same canonical path from an official index source.

### Minimum Done

- Add an on-demand fetch policy/config gate.
- Validate and normalize allowed Shopify docs/changelog URLs and unindexed canonical paths.
- Implement `shopify_fetch(url)` for allowed, enabled URLs.
- Implement `refresh --url <URL>` for allowed, enabled URLs.
- Save fetched raw source, upsert `docs`, and update Tantivy without full rebuild as the normal path.
- Return a `shopify_map` zero-result on-demand candidate without fetching.
- Add `shopify-rextant coverage repair` to retry failed allowed URL coverage rows.
- Cover disabled, disallowed, success, duplicate/upsert, search, and repair behavior with tests.

### Verification Criteria

- WHEN `shopify_fetch` receives an allowed `shopify.dev/docs/**` URL and on-demand fetch is enabled, THEN it fetches raw source, saves it, upserts the doc as `source = "on_demand"`, indexes it, and returns fetch output.
- WHEN `shopify_fetch` receives a URL while on-demand fetch is disabled, THEN it does no network request and returns the SPEC-defined `-32007` error contract with candidate URL data and `enable_on_demand_fetch=false`.
- WHEN `shopify_fetch` receives a disallowed host or path, THEN it does no network request and returns the SPEC-defined `-32008` policy error.
- WHEN `shopify_fetch` receives an unindexed canonical `/docs/**` or `/changelog/**` path and on-demand fetch is enabled, THEN it derives the official `https://shopify.dev` URL and follows the same on-demand pipeline.
- WHEN `refresh --url <URL>` succeeds, THEN subsequent `shopify_search` and `shopify_fetch(path)` can find the document locally.
- WHEN `shopify_map` has zero results for a doc-like query, THEN it includes an estimated on-demand candidate and the current `enable_on_demand_fetch` status without fetching.
- WHEN `coverage repair` sees a failed allowed URL, THEN it retries through the same on-demand pipeline and updates the coverage row status.

## 2. Source Findings

- `SPEC.md` now records v0.5.0 as implemented and keeps the product boundary: local-first, explicit config gate, official docs/changelog URLs only, no arbitrary URL fetcher.
- `src/main.rs` implements `refresh [PATH]`, `refresh --url`, and `coverage repair` in the CLI command model.
- `src/main.rs` reads `[index].enable_on_demand_fetch` from the configured home directory and defaults it to disabled.
- `src/main.rs` validates allowed input before network access: `https://shopify.dev/docs/**` and `https://shopify.dev/changelog/**` only.
- `src/main.rs` preserves MCP machine-readable error codes `-32007` for disabled allowed fetches and `-32008` for out-of-scope URLs.
- `src/main.rs` uses a shared on-demand pipeline for MCP fetch, CLI refresh, and coverage repair: raw fetch, raw cache write, docs upsert, optional coverage event, and Tantivy single-doc update.
- `src/main.rs` preserves higher-precedence `llms` / `sitemap` sources when an already indexed doc is refreshed through the on-demand path.
- `src/main.rs` emits `shopify_map.meta.on_demand_candidate` only for zero-result allowed URL/path inputs and does not fetch from `shopify_map`.

## 3. Requirements Trace

| ID | Requirement | Source |
| --- | --- | --- |
| R1 | `shopify_fetch(url)` supports on-demand official docs fetch when enabled. | `SPEC.md:663`, `SPEC.md:1737` |
| R2 | `refresh --url <URL>` retries or inserts a single official docs URL. | `SPEC.md:1737` |
| R3 | Allowed URLs are limited to `shopify.dev/docs/**` and `shopify.dev/changelog/**`. | `SPEC.md:666`, `SPEC.md:1740` |
| R4 | Raw source, docs row, and Tantivy index are updated for missing URLs. | `SPEC.md:664`, `SPEC.md:1739` |
| R5 | `shopify_map` zero-result output reports a candidate URL and on-demand enabled status. | `SPEC.md:1741` |
| R6 | Failed coverage URLs can be retried through `coverage repair`. | `SPEC.md:1742` |
| R7 | On-demand docs use `source = "on_demand"`. | `SPEC.md:1743` |
| R8 | This is not an arbitrary URL fetcher. | `SPEC.md:1746`, `AGENTS.md` |
| R9 | Disabled and out-of-scope fetches use the specified MCP error contract. | `SPEC.md:1540`, `SPEC.md:1550`, `SPEC.md:1551`, `SPEC.md:1559` |
| R10 | Config uses the documented `[index].enable_on_demand_fetch` key. | `SPEC.md:1353`, `SPEC.md:1356`, `SPEC.md:1366` |

## 4. Design Decisions

### 4.1 Default network posture

On-demand fetch defaults to disabled. Enablement is read from the documented local config file under the configured home directory, for example:

```toml
[index]
enable_on_demand_fetch = true
```

MCP calls do not get a per-call escape hatch. This keeps network permission a local operator decision instead of something a model can enable through tool arguments.

### 4.2 URL policy model

Introduce a small internal policy type that accepts only:

- scheme: `https`
- host: `shopify.dev`
- path prefix: `/docs/` or `/changelog/`

Normalize by removing query strings, fragments, and trailing slashes before deriving the canonical docs path. Disallowed input fails before any network call.

### 4.3 MCP error contract

Do not leave on-demand failures as generic `-32000` tool errors. Introduce typed internal errors or an equivalent domain error adapter so MCP responses can preserve:

- `-32007` for on-demand fetch disabled, with candidate URL data and `enable_on_demand_fetch=false` when a safe candidate exists.
- `-32008` for URL outside allowed scope.

CLI commands can render these as human-readable messages, but MCP should keep the SPEC-defined code and machine-readable data.

### 4.4 Canonical path support

For `shopify_fetch(path)` when the path is not indexed:

- If it is an allowed canonical `/docs/**` or `/changelog/**` path and on-demand is enabled, derive `https://shopify.dev{path}` and run the on-demand pipeline.
- If it is not allowed, return the scope error.
- If on-demand is disabled, return a disabled error with the derived candidate when the path itself is allowed.

### 4.5 Index update strategy

The normal v0.5 path should update Tantivy for the single fetched document instead of rebuilding the full index. If Tantivy delete-by-path or schema compatibility fails during implementation, use a rebuild fallback for correctness, but keep the public behavior and tests written against the successful postcondition rather than the internal mechanism.

The delta path must remove stale indexed terms for the same canonical path before inserting the updated document. An append-only update is not sufficient.

### 4.6 Source transition and precedence

An on-demand doc is inserted with `source = "on_demand"` only when the canonical path is newly recovered. If the canonical path already exists from `llms_txt`, `sitemap`, or another non-on-demand source, a URL refresh must preserve the existing higher-precedence source unless a full build later updates it through its normal source-specific path.

Recommended source precedence:

1. `llms_txt`
2. `sitemap`
3. `on_demand`
4. `fixture`

A later full build may upsert the same canonical path from `sitemap` or `llms_txt` and replace `on_demand` with that official index source. This keeps on-demand recovery visible without permanently shadowing full-index provenance.

### 4.7 `shopify_map` candidate semantics

`shopify_map` only suggests on-demand candidates. It never fetches. Candidate confidence is conservative:

- URL-like allowed input: exact candidate.
- Canonical path-like allowed input: derived `https://shopify.dev{path}` candidate.
- Free text: no candidate unless an existing local normalization rule can produce a URL-safe Shopify docs path without ambiguity.

## 5. Implementation Units

### Unit 1 - Config and Fetch Policy Gate

Files:

- `src/main.rs`
- `SPEC.md`

Work:

- [x] Add config loading from the configured home directory.
- [x] Add `enable_on_demand_fetch` with default `false`.
- [x] Add an internal `OnDemandFetchPolicy` or equivalent helper.
- [x] Ensure disabled and policy failures happen before network calls.
- [x] Surface enabled/disabled status to code paths that build `shopify_map` metadata.

Tests:

- [x] Disabled `shopify_fetch(url)` returns a disabled error and does not call a network source.
- [x] Disallowed host, scheme, and path return policy errors.
- [x] Allowed URL passes validation when enabled.

### Unit 2 - MCP Error Contract

Files:

- `src/main.rs`
- `SPEC.md`

Work:

- [x] Add typed tool/domain errors or an adapter around existing `anyhow` paths.
- [x] Preserve SPEC codes for MCP responses:
  - `-32007` on-demand fetch disabled
  - `-32008` URL outside allowed scope
- [x] Include candidate URL data and `enable_on_demand_fetch=false` for disabled allowed candidates.
- [x] Keep CLI output readable without losing MCP machine-readable data.

Tests:

- [x] MCP disabled URL response uses `-32007` and includes candidate/enablement data.
- [x] MCP out-of-scope URL response uses `-32008`.
- [x] Unknown tool and unrelated internal failures continue to use the existing generic error path.

### Unit 3 - URL and Path Normalization

Files:

- `src/main.rs`

Work:

- [x] Reuse or extend `canonical_doc_path`, `raw_doc_candidates`, and source URL handling.
- [x] Normalize `https://shopify.dev/docs/...?...#...` to one canonical docs path.
- [x] Support unindexed `/docs/**` and `/changelog/**` paths by deriving the matching official URL.
- [x] Preserve existing indexed-path behavior for already local docs.

Tests:

- [x] Query and fragment are stripped.
- [x] Trailing slash canonicalization is stable.
- [x] `/docs/**` and `/changelog/**` paths are accepted.
- [x] Non-doc Shopify paths are rejected.

### Unit 4 - On-Demand Upsert Pipeline

Files:

- `src/main.rs`

Work:

- [x] Build a single shared pipeline:
  1. validate request
  2. fetch source doc
  3. write raw cache
  4. upsert `docs` with source precedence
  5. insert coverage event
  6. update Tantivy
  7. return the fetched doc record
- [x] Make this pipeline usable from MCP, CLI refresh, and coverage repair.
- [x] Keep raw fetch fidelity: no summarization, no text rewriting.

Tests:

- [x] Successful on-demand fetch writes raw source and a `docs` row.
- [x] Newly recovered docs are recorded as `on_demand`.
- [x] Repeating the same URL is idempotent and updates the existing canonical row.
- [x] Fetching an already indexed non-on-demand doc preserves its higher-precedence source.
- [x] Search finds newly fetched content.
- [x] Re-fetching changed content removes stale indexed terms for that path.
- [x] `shopify_fetch(path)` works after a URL fetch.

### Unit 5 - MCP and CLI Wiring

Files:

- `src/main.rs`
- `SPEC.md`

Work:

- [x] Replace the pre-v0.5 `shopify_fetch(url)` disabled branch with the on-demand pipeline.
- [x] Add `refresh --url <URL>` while preserving existing positional `refresh [PATH]` behavior.
- [x] Update help text and MCP tool descriptions so agents understand that URL fetch depends on local configuration.
- [x] Preserve stdout/stderr separation for MCP.

Tests:

- [x] MCP `tools/call shopify_fetch` with `url` succeeds when enabled.
- [x] MCP `tools/call shopify_fetch` with `url` fails cleanly when disabled.
- [x] CLI `refresh --url` succeeds with enabled config.
- [x] CLI `refresh [PATH]` keeps existing behavior.

### Unit 6 - `shopify_map` Zero-Result Candidate

Files:

- `src/main.rs`

Work:

- [x] Populate existing `OnDemandCandidate` metadata when map results are empty and the input is URL-like or path-like.
- [x] Include `enable_on_demand_fetch` status and an explanation that `shopify_fetch(url)` can recover the doc only when enabled.
- [x] Keep candidate output warning-only; do not fetch from `shopify_map`.

Tests:

- [x] Zero-result map for allowed URL-like input returns candidate metadata.
- [x] Zero-result map for disallowed URL-like input does not suggest a fetch.
- [x] Non-zero map results do not add noisy candidates.

### Unit 7 - Coverage Repair CLI

Files:

- `src/main.rs`
- `SPEC.md`

Work:

- [x] Add a `coverage repair` CLI command.
- [x] Select failed allowed URL coverage rows.
- [x] Retry each through the shared on-demand pipeline.
- [x] Update status and error fields per URL.
- [x] Return a concise summary: attempted, repaired, still_failed, skipped_policy, skipped_disabled.

Tests:

- [x] Failed allowed URL row is repaired after source becomes available.
- [x] Disallowed failed URL is skipped without network access.
- [x] Disabled config skips repair with a clear message.
- [x] Partial failures do not prevent later rows from being attempted.

### Unit 8 - Documentation and Final Spec Pass

Files:

- `SPEC.md`
- `README.md` if present, otherwise a v0.5 docs note under `docs/`
- `AGENTS.md` only if a new durable preference is confirmed during implementation

Work:

- [x] Mark v0.5 checklist items as implemented only after tests pass.
- [x] Document how to enable on-demand fetch locally.
- [x] Document accepted URL scope.
- [x] Document `refresh --url` and `coverage repair` usage.
- [x] Record any non-obvious implementation decision in project docs.

Tests:

- [x] Documentation examples match actual CLI syntax.
- [x] `cargo test` passes.
- [x] Manual MCP smoke verifies `initialize`, `tools/list`, and `shopify_fetch(url)` with enabled config.

## 6. Suggested Execution Order

1. Unit 1 - Config and fetch policy gate.
2. Unit 2 - MCP error contract.
3. Unit 3 - URL and path normalization.
4. Unit 4 - Shared on-demand upsert pipeline.
5. Unit 5 - MCP and CLI wiring.
6. Unit 6 - Map candidate metadata.
7. Unit 7 - Coverage repair CLI.
8. Unit 8 - Documentation and final SPEC pass.

Units 1-4 should land together or behind tests because they define the core safety boundary. Units 6 and 7 can be implemented after the shared pipeline exists.

## 7. End-to-End Flow

```text
shopify_fetch(url or unindexed path)
  -> load config
  -> validate on-demand policy
  -> normalize URL/path
  -> fetch raw .md/.txt source candidate
  -> write raw cache
  -> upsert docs row with source = "on_demand"
  -> record coverage event
  -> update Tantivy for this doc
  -> return source-backed fetch response
```

```text
coverage repair
  -> load failed coverage rows
  -> filter to allowed official docs/changelog URLs
  -> run shared on-demand pipeline per URL
  -> update coverage status
  -> print repair summary
```

## 8. Risk Register

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Accidentally creating arbitrary URL fetch behavior | Security and product-boundary regression | Strict scheme, host, and path policy before network access |
| Model-triggered network access without operator intent | Local-first boundary regression | Default disabled config, no per-call MCP enable flag |
| Partial writes across raw cache, DB, and index | Inconsistent local search/fetch results | Shared pipeline, DB transaction for row writes, index fallback rebuild for correctness |
| Duplicate docs under slightly different URL forms | Search noise and stale rows | Canonicalize query, fragment, and trailing slash before upsert |
| Tantivy single-doc update is incompatible with current schema | Failed search after fetch | Test postcondition and keep full rebuild fallback |
| `shopify_map` suggests bad URLs from free text | Agent confusion | Only suggest candidates for exact URL/path-like inputs initially |
| Coverage repair loops on permanently unavailable URLs | Slow or noisy CLI | Retry only explicit failed rows, print summary, retain last error |

## 9. Validation Matrix

| Area | Command or Test |
| --- | --- |
| Unit tests | `cargo test` |
| Build sanity | `cargo build` |
| MCP startup | newline-delimited `initialize` smoke |
| MCP tool list | newline-delimited `tools/list` smoke |
| On-demand fetch | `tools/call shopify_fetch` with enabled config and a fixture-backed allowed URL |
| Disabled policy | `tools/call shopify_fetch` with disabled config |
| MCP error codes | disabled fetch returns `-32007`; out-of-scope fetch returns `-32008` |
| CLI refresh | `shopify-rextant refresh --url <allowed-url>` and existing `shopify-rextant refresh [PATH]` |
| Coverage repair | `shopify-rextant coverage repair` against fixture failed rows |

## 10. Acceptance Checklist

- [x] On-demand fetch config exists and defaults to disabled.
- [x] Config uses `[index].enable_on_demand_fetch`.
- [x] URL scope validation blocks all non-`https://shopify.dev/docs/**` and non-`https://shopify.dev/changelog/**` URLs.
- [x] MCP disabled fetch errors use `-32007` with candidate/enablement data.
- [x] MCP out-of-scope fetch errors use `-32008`.
- [x] `shopify_fetch(url)` works when enabled.
- [x] `shopify_fetch(path)` can recover unindexed allowed paths when enabled.
- [x] `refresh --url` works and shares the same safety checks.
- [x] Existing positional `refresh [PATH]` still works.
- [x] Fetched docs are stored with `source = "on_demand"`.
- [x] Fetching an already indexed non-on-demand doc does not downgrade its source to `on_demand`.
- [x] Raw source file is saved under the configured cache root.
- [x] Tantivy search finds newly fetched docs.
- [x] Tantivy delta update removes stale terms for the same canonical path.
- [x] Re-fetching the same URL is idempotent.
- [x] `shopify_map` zero-result output includes conservative on-demand candidate metadata.
- [x] `coverage repair` retries failed allowed URLs and reports skipped/failed rows.
- [x] MCP stdio behavior remains newline-delimited stdout responses only.
- [x] `cargo test` passes.
- [x] `cargo build` passes.
- [x] `SPEC.md` v0.5 checklist is updated after implementation.
