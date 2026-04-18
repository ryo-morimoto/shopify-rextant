---
title: feat: Implement v0.3.0 freshness and changelog impact
type: feat
status: active
date: 2026-04-17
---

# feat: Implement v0.3.0 freshness and changelog impact

## Overview

Implement `SPEC.md` v0.3.0 by connecting Shopify changelog entries and index freshness to `shopify_map`, `shopify_fetch`, `shopify_status`, and `refresh`. The core design is source-first: RSS text can only produce candidates; the authoritative impact set is resolved through local `concepts`, `docs`, and `edges`.

The current implementation is still a single-file Rust binary in `src/main.rs`. For v0.3.0, keep the public CLI/MCP surface small, but introduce internal module boundaries inside `src/main.rs` first so the change remains reviewable and can be split into files later without changing behavior.

## Goal Model

- Purpose: prevent agents from trusting stale, deprecated, or soon-to-break Shopify docs when using local maps.
- Non-goals: no changelog-prose-only impact truth, no schema diff engine, no query-log repair, no on-demand URL fetch, no GraphQL/Liquid validation, no store mutation, no LLM synthesis.
- Constraints: `concepts` / `docs` / `edges` are the impact SSoT; unresolved changelog candidates are recorded but never mark docs deprecated; `refresh` without a path must not run a full index rebuild.
- Minimum: ingest changelog feed fixtures, resolve impacts, persist changelog/scheduled changes, mark affected docs, hydrate node staleness, expose status, and sweep stale docs without rebuilding the whole index.
- Verification criteria: carry forward all six v0.3.0 WHEN/THEN cases from `SPEC.md`.

## Requirements Trace

- R1. Changelog RSS entries are fetched and parsed into durable `changelog_entries`.
- R2. Changelog title/body/link/categories produce candidates, but only candidates resolved through `concepts`, `docs`, or `edges` are used as `affected_types` or `scheduled_changes.type_name`.
- R3. Unresolved candidates are stored in `unresolved_affected_refs` and do not affect `references_deprecated` or `upcoming_changes`.
- R4. Resolved concept/doc impacts expand through `edges` so linked docs and concepts can be marked.
- R5. `staleness` includes `references_deprecated`, `deprecated_refs`, and `upcoming_changes`.
- R6. `refresh PATH` updates one doc; `refresh` without a path sweeps aging/stale docs only and does not call full `build_index`.
- R7. `shopify_status` includes freshness distribution, worker timestamps, and changelog polling warnings.
- R8. The implementation preserves local-first, zero telemetry, no synthesis, and MCP stdout protocol safety.

## Scope Boundaries

- Do not introduce server-side summaries or generated answers.
- Do not use unresolved changelog text as a source of truth for deprecation or upcoming changes.
- Do not add on-demand fetch for missing docs URLs; that remains v0.5.
- Do not implement full version-to-version schema diffing.
- Do not split the repository into a new crate structure in this iteration unless implementation pressure makes the single file unsafe to review.

### Deferred to Separate Tasks

- Background worker scheduling on `serve`: v0.3.0 can implement callable worker functions and CLI-triggered flows first. Automatic periodic `tokio` tasks can follow once the data contract is stable.
- Conditional GET with persisted ETag/Last-Modified: useful for network efficiency, but not required for the fixture-backed v0.3.0 contract.
- Storefront GraphQL, Liquid, Functions, and Polaris concept-level changelog impact: keep existing doc/FTS behavior until their concept extraction lands.

## Context & Research

### Relevant Code and Patterns

- `src/main.rs` currently owns CLI parsing, MCP handling, indexing, graph expansion, persistence, and tests.
- `TextSource` already abstracts external text fetches for tests and build-time source injection.
- `build_index_from_sources` already performs source fetching, raw storage, SQLite upserts, tantivy indexing, graph build, and status metadata updates in one workflow.
- `refresh` currently handles `PATH` by refetching one doc, but falls back to `build_index(paths, false, None)` when no path is provided; this conflicts with v0.3.0.
- `Staleness` has the target fields but `staleness(doc)` currently derives deprecated fields as empty values.
- `init_db` already creates `docs`, `coverage_reports`, `concepts`, and `edges`, and uses `ensure_column` for compatible schema evolution.
- Current tests are fixture-heavy and network-independent; v0.3.0 should follow that pattern.

### Institutional Learnings

- Obsidian search found no project-specific prior note for `shopify-rextant` v0.3/changelog impact. The relevant local knowledge is in `AGENTS.md` and `SPEC.md`.
- `AGENTS.md` records the settled preference that v0.3 changelog impact must resolve against `concepts` / `docs` / `edges`, and unresolved candidates must not mark docs deprecated.

### External References

- Shopify Developer Changelog: `https://shopify.dev/changelog`
- Shopify Changelog RSS feed verified from the official RSS endpoint: `https://shopify.dev/changelog/feed.xml`
- `feed-rs` docs: `https://docs.rs/feed-rs` describes a unified parser for Atom, RSS, and JSON Feed over a `Read` input.

## Approach Landscape

| Approach | How it works | Strength | Cost |
|---|---|---|---|
| A. Encapsulated regions in `src/main.rs` | Add internal data types and functions for changelog, impact resolution, freshness, and sweep while keeping one file | Lowest friction against current code and tests | File remains large until a later refactor |
| B. Split modules now | Move changelog, storage, graph, MCP, and tests into files | Better long-term separation | Higher diff risk because current worktree has large v0.2 edits |
| C. Worker runtime first | Build periodic worker scheduler and app state before impact logic | Closer to final architecture | Delays the v0.3 contract and increases async/state complexity |

Chosen for Step 1: Approach A. It respects the current single-file implementation while enforcing internal contracts through types and tests. A later module split can be mechanical once behavior is covered.

## Key Technical Decisions

- Add a `ChangelogSource` shape through the existing `TextSource` rather than a separate HTTP client layer. This keeps network effects injectable and fixture-backed.
- Use a structured RSS parser (`feed-rs`) instead of regex-parsing feeds. Regex remains appropriate for extracting changelog impact candidates from already-parsed text.
- Introduce explicit domain records: `ChangelogEntryRecord`, `ScheduledChangeRecord`, `ImpactCandidate`, and `ResolvedImpact`. These isolate parsing, resolution, persistence, and staleness hydration.
- Store resolved doc IDs and concept IDs together in `affected_types` for compatibility with the current SPEC wording, but keep the resolver typed internally so doc impacts and concept impacts are not conflated.
- Compute staleness from DB state at hydration time, not during initial document indexing. Changelog processing can update `docs.references_deprecated` / `deprecated_refs`, while `upcoming_changes` is read from `scheduled_changes`.
- Keep `refresh` as two explicit paths: `refresh_doc` for `PATH`, `sweep_stale_docs` for no path. Avoid calling full `build_index` from refresh.
- Do not mark a doc deprecated from an unresolved candidate, even if the changelog wording looks obvious.

## Open Questions

### Resolved During Planning

- Should changelog text alone determine impact? No. `SPEC.md` and `AGENTS.md` both make the index graph the SSoT.
- Should v0.3 add URL on-demand fetch? No. `SPEC.md` assigns it to v0.5.
- Should feed parsing be regex-only? No. RSS is structured input; use a parser and keep regex for candidate extraction from entry fields.

### Deferred to Implementation

- Exact JSON representation for `affected_types`: implement typed internals first, then serialize stable strings. Preserve backward compatibility by using strings.
- Exact status field names for worker timestamps and freshness distribution: confirm before implementation because this extends MCP structured output.
- Whether to add `feed-rs` or a smaller parser after dependency resolution: plan assumes `feed-rs`; implementation can switch only if dependency validation fails.

## High-Level Technical Design

This illustrates the intended approach and is directional guidance for review, not implementation specification. The implementing agent should treat it as context, not code to reproduce.

```text
fetch changelog feed
  -> parse RSS items into ChangelogEntryInput
  -> extract candidates from title/body/link/categories
  -> resolve candidates against concepts/docs/edges
  -> persist changelog_entries with resolved + unresolved refs
  -> derive scheduled_changes only for resolved refs
  -> mark affected docs references_deprecated/deprecated_refs
  -> hydrate staleness for map/fetch from docs + scheduled_changes
  -> expose freshness/changelog status in shopify_status
```

Pure logic boundary:

```text
parse feed, extract candidates, classify changes, resolve impact, compute staleness
```

Side-effect boundary:

```text
HTTP fetch, SQLite writes, raw doc fetch, tantivy rebuild/update, CLI/MCP output
```

## Implementation Units

- [ ] **Unit 1: Schema and Domain Contracts**

**Goal:** Add v0.3 persistence and typed internal records without changing behavior.

**Requirements:** R1, R2, R3, R5, R7

**Dependencies:** None

**Files:**
- Modify: `src/main.rs`
- Test: `src/main.rs`

**Approach:**
- Bump `SCHEMA_VERSION` for the new schema.
- Extend `init_db` with `changelog_entries` and `scheduled_changes` plus indexes.
- Ensure `docs.references_deprecated` and `docs.deprecated_refs` are selected/upserted consistently, not just declared in schema.
- Add internal structs for changelog entries, scheduled changes, candidates, and resolved impacts.
- Keep JSON payload columns as serialized arrays for now to match existing SPEC and SQLite style.

**Patterns to follow:**
- Existing `CoverageEvent`, `ConceptRecord`, `GraphEdgeRecord`, `insert_concept`, `insert_edge`, and `ensure_column` patterns.

**Test scenarios:**
- Happy path: `init_db` creates changelog and scheduled change tables and indexes on an empty DB.
- Edge case: existing DB without v0.3 columns is migrated without losing docs.
- Error path: malformed JSON in deprecated refs falls back safely during staleness hydration.
- Integration: inserting a doc with `references_deprecated=true` and reading it back preserves the flag and refs.

**Verification:**
- Schema initialization is idempotent, and no existing v0.1/v0.2 tests need contract changes except expected schema version.

- [ ] **Unit 2: Changelog Feed Parsing**

**Goal:** Fetch and parse Shopify changelog RSS into normalized entries using testable source injection.

**Requirements:** R1, R8

**Dependencies:** Unit 1

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/main.rs`
- Test: `src/main.rs`

**Approach:**
- Add an RSS parser dependency, preferably `feed-rs`, because it parses RSS/Atom/JSON Feed from a `Read` source.
- Add `SHOPIFY_CHANGELOG_FEED_URL` as `https://shopify.dev/changelog/feed.xml`.
- Extend source URL configuration or add a small `ChangelogSourceUrls` wrapper so tests can inject feed fixtures.
- Normalize feed entries to local inputs with id/link fallback, posted date, title, HTML/body text, and categories.
- Store parser warnings in status metadata instead of panicking on one malformed entry.

**Patterns to follow:**
- Existing `IndexSourceUrls`, `TextSource`, `MockTextSource`, and fixture test style.

**Test scenarios:**
- Happy path: RSS fixture with title, description, categories, link, and pubDate becomes one normalized entry.
- Edge case: entry missing GUID uses link as id.
- Edge case: duplicate feed item id is ignored on re-poll.
- Error path: malformed feed returns a changelog polling warning and does not mutate docs.

**Verification:**
- Changelog parsing tests run without network access.

- [ ] **Unit 3: Candidate Extraction and Impact Resolution**

**Goal:** Extract candidate refs from changelog entries and resolve only graph/index-backed impacts.

**Requirements:** R2, R3, R4

**Dependencies:** Unit 1, Unit 2

**Files:**
- Modify: `src/main.rs`
- Test: `src/main.rs`

**Approach:**
- Extract candidates from entry title/body/link/categories:
  - GraphQL field-like refs such as `DraftOrderLineItem.grams`
  - GraphQL type names such as `DraftOrderLineItem`
  - docs/changelog URLs and canonical paths
  - API versions such as `2026-07`
  - surface-like categories such as `Admin GraphQL API`
- Resolve candidates by querying `concepts.id`, `concepts.name`, and `docs.path`.
- For doc impacts, walk relevant `edges` to collect adjacent concepts/docs within a small fixed radius.
- Track unresolved candidates separately.
- Derive `affected_surfaces` from RSS categories and resolved doc/concept metadata; do not trust body text alone.

**Patterns to follow:**
- Existing `load_edges`, `expand_graph`, `get_concept`, `find_concepts_by_name`, `canonical_doc_path`, and `classify_api_surface`.

**Test scenarios:**
- Happy path: `DraftOrderLineItem.grams` resolves to an existing concept id and affected docs through graph edges.
- Happy path: a changelog link to an indexed docs path resolves as doc impact and expands through edges.
- Edge case: a type name present in multiple versions resolves according to explicit version candidate when present.
- Error path: unknown symbol is stored in unresolved refs and returns no affected docs.
- Integration: resolved surfaces include `admin_graphql` when the concept/doc metadata says so, even if body text does not.

**Verification:**
- No unresolved candidate can create a `scheduled_changes` row or flip `docs.references_deprecated`.

- [ ] **Unit 4: Scheduled Changes and Deprecation Marking**

**Goal:** Persist resolved changelog impact and mark affected docs for deprecated references.

**Requirements:** R1, R2, R3, R4, R5

**Dependencies:** Unit 3

**Files:**
- Modify: `src/main.rs`
- Test: `src/main.rs`

**Approach:**
- Persist changelog entries with resolved refs, unresolved refs, surfaces, and processed timestamp.
- Extract scheduled changes from resolved refs only using conservative patterns:
  - removal/deprecation wording
  - effective API version/date when present
  - optional migration hints from entry body
- Add helper queries for affected docs by concept/doc ref.
- Update `docs.references_deprecated` and merge `deprecated_refs` for affected docs.
- Make updates idempotent so reprocessing the same entry does not duplicate refs or scheduled changes.

**Patterns to follow:**
- Existing SQLite insert/upsert helpers and JSON-string storage style.

**Test scenarios:**
- Happy path: `DraftOrderLineItem.grams field removed in 2026-07` stores a removal scheduled change for the resolved concept.
- Happy path: docs connected to the resolved concept are marked `references_deprecated=true`.
- Edge case: processing the same feed twice does not duplicate scheduled changes or deprecated refs.
- Error path: unknown symbol creates `unresolved_affected_refs` only and leaves docs unchanged.
- Integration: linked docs path impact expands through edges and persists both changelog entry and affected docs.

**Verification:**
- The first three v0.3.0 SPEC WHEN/THEN cases pass with fixture data.

- [ ] **Unit 5: Staleness Hydration in Map and Fetch**

**Goal:** Populate `references_deprecated`, `deprecated_refs`, and `upcoming_changes` in returned nodes and fetch responses.

**Requirements:** R5

**Dependencies:** Unit 4

**Files:**
- Modify: `src/main.rs`
- Test: `src/main.rs`

**Approach:**
- Replace `staleness(doc)` with a DB-backed hydration path where possible.
- For doc nodes, read deprecated refs from `docs` and upcoming changes from `scheduled_changes`.
- For concept nodes, use concept id/name to find scheduled changes and use backing doc freshness for age.
- Keep a fallback pure function for tests or status paths that do not have a DB connection.
- Ensure `upcoming_changes` contains `effective_date`, `change`, and optional `migration_hint`.

**Patterns to follow:**
- Existing `doc_map_node`, `concept_map_node`, `graph_map_node`, `shopify_fetch`, and `shopify_map` hydration flow.

**Test scenarios:**
- Happy path: `shopify_map` returning an affected doc includes `staleness.references_deprecated=true`.
- Happy path: `shopify_map` returning an affected concept includes an upcoming change.
- Happy path: `shopify_fetch` for an affected doc includes the same staleness data as map.
- Edge case: past scheduled changes are excluded from `upcoming_changes` but deprecated refs remain visible.
- Error path: invalid scheduled change JSON does not break map/fetch.

**Verification:**
- The fourth v0.3.0 SPEC WHEN/THEN case passes.

- [ ] **Unit 6: Refresh Sweep and Freshness Distribution**

**Goal:** Separate single-doc refresh from aging/stale sweep and expose freshness status.

**Requirements:** R6, R7, R8

**Dependencies:** Unit 1

**Files:**
- Modify: `src/main.rs`
- Test: `src/main.rs`

**Approach:**
- Split `refresh` into `refresh_doc(paths, path)` and `refresh_stale_docs(paths)`.
- `refresh_stale_docs` queries only docs whose computed or stored freshness is `aging` or `stale`.
- Update stored freshness based on age thresholds before selecting refresh candidates.
- Refresh selected docs through existing doc URL fetch and `upsert_doc`.
- Rebuild tantivy from DB after the sweep unless a smaller safe update path is already available.
- Do not call `build_index` from no-path refresh.

**Patterns to follow:**
- Current `refresh(PATH)` flow, `store_source_doc`, `upsert_doc`, and `rebuild_tantivy_from_db`.

**Test scenarios:**
- Happy path: no-path refresh considers only aging/stale docs and leaves fresh docs untouched.
- Happy path: `refresh PATH` still refetches only that one doc.
- Edge case: no aging/stale docs returns cleanly and does not rebuild from remote sources.
- Error path: one failed stale doc records/returns a recoverable warning without deleting existing raw content.
- Integration: no-path refresh does not fetch `llms.txt`, `sitemap.xml`, or admin GraphQL schema.

**Verification:**
- The fifth v0.3.0 SPEC WHEN/THEN case passes.

- [ ] **Unit 7: Status Contract Extension**

**Goal:** Extend `shopify_status` and CLI `status` to report v0.3 worker/freshness/changelog state.

**Requirements:** R7

**Dependencies:** Unit 1, Unit 4, Unit 6

**Files:**
- Modify: `src/main.rs`
- Test: `src/main.rs`

**Approach:**
- Add a nested status object rather than flattening many new top-level fields:
  - `freshness.fresh_count`
  - `freshness.aging_count`
  - `freshness.stale_count`
  - `workers.last_changelog_at`
  - `workers.last_aging_sweep_at`
  - `workers.last_version_check_at`
  - `changelog.entry_count`
  - `changelog.scheduled_change_count`
  - `changelog.unresolved_ref_count`
  - warnings for last changelog parse/fetch error
- Preserve existing `coverage` and graph status fields.
- Before implementation, confirm this structured output shape because it is an MCP contract extension.

**Patterns to follow:**
- Existing `StatusResponse`, `CoverageStatus`, `GraphIndexStatus`, `status(paths)`, and MCP `structuredContent` output.

**Test scenarios:**
- Happy path: after processing fixture changelog, status includes changelog entry count and scheduled change count.
- Happy path: freshness distribution counts docs by state.
- Edge case: DB not built returns zeroed nested statuses and an index-not-built warning.
- Error path: last changelog polling warning appears in `warnings`.
- Integration: MCP `tools/call shopify_status` returns the same structured fields as CLI status.

**Verification:**
- The sixth v0.3.0 SPEC WHEN/THEN case passes.

## System-Wide Impact

- **Interaction graph:** build remains the full rebuild entry point; refresh becomes incremental; changelog processing reads graph/index state and writes changelog/scheduled/doc flags.
- **Error propagation:** changelog feed/parse errors should become warnings/status, not MCP panics. Single-doc refresh errors remain recoverable command errors.
- **State lifecycle risks:** duplicate feed processing, duplicate deprecated refs, and stale scheduled changes are the main risks. Use idempotent inserts and sorted/deduped JSON refs.
- **API surface parity:** CLI `status`, MCP `shopify_status`, `shopify_map`, and `shopify_fetch` must agree on staleness and freshness semantics.
- **Integration coverage:** fixture-driven full flow is required: feed -> resolve -> persist -> mark docs -> map/fetch/status.
- **Unchanged invariants:** stdout remains protocol-only; no LLM synthesis; no user code/query telemetry; URL on-demand fetch remains disabled.

## Risks & Mitigations

| Risk | Mitigation |
|---|---|
| Changelog wording produces false positives | Treat regex output as candidates only; require DB/graph resolution before impact |
| Unresolved candidates silently disappear | Persist `unresolved_affected_refs` and expose unresolved counts in status |
| Status response grows without a reviewed contract | Confirm nested status shape before implementation |
| Single-file implementation becomes hard to maintain | Add internal boundary comments and cohesive helper groups; split modules only after tests protect behavior |
| Refresh accidentally performs a full rebuild | Add a test source that fails if `llms.txt`, `sitemap.xml`, or schema URLs are fetched during no-path refresh |
| Feed parser dependency adds unnecessary surface | Keep HTTP fetching in existing `TextSource`; use parser only for parsing bytes |

## Documentation / Operational Notes

- Update `SPEC.md` checkboxes only after implementation and tests pass.
- If status contract field names differ from this plan, update the plan or implementation notes before coding.
- Manual validation after implementation should include `cargo test` and MCP smoke tests only if MCP transport/status output is touched.

## Sources & References

- `AGENTS.md`: project identity, roadmap allocation, validation expectations, and settled v0.3 changelog-impact preference.
- `SPEC.md:70`: staleness is part of the product contract.
- `SPEC.md:292`: target `changelog_entries` schema.
- `SPEC.md:307`: target `scheduled_changes` schema.
- `SPEC.md:1133`: v0.3 changelog impact resolution contract.
- `SPEC.md:1232`: target staleness computation.
- `SPEC.md:1327`: `refresh [PATH]` behavior.
- `SPEC.md:1688`: v0.3.0 roadmap section.
- `src/main.rs:321`: current `TextSource` abstraction.
- `src/main.rs:475`: current build entry point.
- `src/main.rs:573`: current refresh behavior requiring v0.3 separation.
- `src/main.rs:828`: current `shopify_map` entry point.
- `src/main.rs:1395`: current status entry point.
- `src/main.rs:2696`: current DB initialization.
- `src/main.rs:2973`: current staleness function with empty deprecated/upcoming fields.
- `Cargo.toml:8`: current dependency surface.
- Shopify Developer Changelog: `https://shopify.dev/changelog`
- Shopify Changelog RSS feed: `https://shopify.dev/changelog/feed.xml`
- `feed-rs` parser docs: `https://docs.rs/feed-rs`
