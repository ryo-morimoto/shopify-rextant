# Refactor plan: split `src/main.rs` (7,251 LOC)

Status: proposal
Author: Claude (Opus 4.7)
Date: 2026-04-19
Principles: encapsulation / separation of concerns / design by contract / side-effect isolation

## 1. 現状診断

`src/main.rs` は CLI、MCP プロトコル、デーモン/ソケット、HTTP フェッチ、SQLite 永続化、Tantivy 検索、Markdown パース、GraphQL インジェスト、Changelog 解析、Map/Graph 算出、tests をすべて抱えている。既存の分離点は `mcp_framing.rs` と `on_demand.rs` のみ。

| 層 | 行範囲の目安 | 種別 |
|---|---|---|
| CLI 定義 (`Cli` / `Command` / `CoverageCommand` / `SearchArgs`) | 93–196, 448 | 純粋 (parse) |
| ドメイン型 (status / map / staleness / records) | 198–370, 830–844, 2913–3032, 3034–3108 | 純粋データ |
| 設定 + Paths + OnDemand policy | 372–697 | 純粋 + FS I/O |
| TextSource trait + Reqwest 実装 | 462–563 | 契約 + HTTP I/O |
| アプリケーション層 (build_index / refresh / coverage_repair) | 699–945 | オーケストレータ |
| Daemon + ソケット + ロック | 947–1300 | OS I/O |
| MCP ハンドラ (ServerState, handle_mcp_request, call_mcp_tool, tool_descriptors, ToolError) | 391–446, 1302–1672 | プロトコル |
| Fetch 系 (shopify_fetch, on_demand_fetch_*, fetch_local_doc, fetch_source_doc) | 1673–1772, 3110–3152 | オーケストレータ |
| Map ランタイム | 1774–2477 | オーケストレータ |
| ステータス/メタ DB クエリ | 2478–2730 | DB I/O |
| Search ランタイム + Tokenizer + スキーマ | 2743–2912, 5079–5192 | 検索 I/O |
| Changelog フィード/インパクト/DB | 3110–3717 | 純粋 + DB I/O |
| Admin GraphQL インジェスト | 3718–4236 | DB I/O + 純粋 |
| Markdown / Sitemap / URL 分類 | 4238–4568 | 純粋 |
| SQLite CRUD (docs/graph/coverage/changelog/meta) | 4570–5078 | DB I/O |
| ユーティリティ (JSON/hash/time) | 5193–5206 | 純粋 |
| tests | 5207–7251 | test harness |

中核的な問題:
- 純粋関数 (parser / classifier / slugify / extract_*) が I/O 関数と同一スコープにあり、依存関係の向きが混ざっている。
- `fn open_db` / `fn init_db` と各種 `upsert_*` / `insert_*` / `get_*` が並列に存在し、リポジトリ境界が不明瞭。
- `ServerState` / Daemon / Workers が MCP ハンドラと同居し、プロセス寿命と呼び出し規約が絡み合う。
- ドメイン型 (`DocRecord`, `ConceptRecord`, `ChangelogEntryInput`) に対するバリデーションが CRUD 呼び出し側に散らばる (=契約が明示されない)。
- tests が内部可視性に依存しており、モジュール化の制約を生んでいる。

## 2. 目標モジュール構成

最終形 (全 phase 完了後):

```
src/
  main.rs                 # エントリ (CLI dispatch のみ)
  lib.rs                  # クレートルート (pub use の集約)
  cli.rs                  # Cli / Command / CoverageCommand / SearchArgs (parsing-only)
  config.rs               # AppConfig / IndexConfig / load_config / OnDemandFetchPolicy
  paths.rs                # Paths (path algebra のみ)
  app/
    mod.rs                # 公開 API (build_index / refresh / coverage_repair)
    build.rs              # build_index
    refresh.rs            # refresh / refresh_doc / refresh_stale
    repair.rs             # coverage_repair
  source/
    mod.rs                # TextSource trait + SourceFetchError
    reqwest.rs            # ReqwestTextSource (HTTP 副作用)
  mcp/
    mod.rs
    framing.rs            # (既存 mcp_framing を移設)
    protocol.rs           # handle_mcp_request / call_mcp_tool / tool_descriptors / json_rpc_error
    error.rs              # ToolError
    server.rs             # ServerState / serve / serve_direct
    daemon.rs             # DaemonIdentity / DaemonPaths / DaemonLock / spawn / handle_client / run_daemon
    workers.rs            # spawn_background_workers
  db/
    mod.rs                # open_db / init_db / ensure_column
    docs.rs               # upsert_doc / get_doc / all_docs / stale_refresh_candidates
    graph.rs              # concept/edge CRUD / load_edges / get_concept / find_concept_by_name
    coverage.rs           # insert_coverage_event / failed_coverage_rows / update_coverage_* / clear_coverage_reports
    changelog.rs          # changelog_entry_exists / insert_changelog_entry / insert_scheduled_change / mark_docs_deprecated / scheduled_changes_for_*
    meta.rs               # get_meta / set_meta / refresh_indexed_versions / indexed_version_exists / enqueue_version_rebuild / count_docs / count_where
    staleness.rs          # staleness / staleness_for_doc / update_doc_freshness_states
    rows.rs               # doc_from_row / concept_from_row / parse_json_string_vec (純粋)
  domain/
    mod.rs
    doc.rs                # DocRecord / SourceDoc / MarkdownLink / SectionInfo
    concept.rs            # ConceptRecord / GraphEdgeRecord / GraphBuild / GraphNodeKey / GraphExpansion
    changelog.rs          # ChangelogEntryInput / ResolvedImpact / ScheduledChangeRecord / ChangelogPollReport / VersionCheckReport
    coverage.rs           # CoverageEvent / CoverageStatus / CoverageSources / CoverageRepairSummary / CoverageRepairRow
    map.rs                # MapMeta / MapIndexStatus / MapCenter / MapNode / OnDemandCandidate / QueryInterpretation / QueryPlanStep / Staleness
    status.rs             # GraphIndexStatus / FreshnessStatus / WorkerStatus / ChangelogStatus
  markdown.rs             # parse_markdown_links / extract_sections / parse_heading / slugify_heading / section_content / remove_fenced_code_blocks / title_from_markdown (純粋)
  sitemap.rs              # parse_sitemap_links / dedupe_links_by_path (純粋)
  url_policy.rs           # canonical_doc_path / raw_path_for / raw_doc_candidates / is_indexable_shopify_url / extract_version / classify_doc_type / classify_content_class / classify_api_surface (純粋)
  changelog/
    feed.rs               # parse_changelog_feed / version_candidates_desc / validate_admin_graphql_version
    impact.rs             # resolve_changelog_impact + ヘルパ群 (extract_impact_candidates / trim_candidate / is_api_version / candidate_to_doc_path / looks_like_reference_candidate / surface_from_category / collect_graph_neighbors / classify_change / extract_effective_date / extract_migration_hint / impact_affected_types / scheduled_changes_from_entry) ― 純粋
    poll.rs               # poll_changelog_from_source / check_new_versions_from_source (オーケストレータ)
  graphql/
    mod.rs
    build.rs              # build_admin_graphql_graph
    ingest.rs             # ingest_introspection_schema / ingest_graphql_field / add_doc_graph_edges
    schema_urls.rs        # admin_graphql_versions / admin_graphql_direct_proxy_url / graphql_reference_path / graphql_concept_kind
    resolve.rs            # concept_id / resolve_concept_id / extract_named_type / markdown_mentions_type / insert_unique_concept / insert_unique_edge
    snapshot.rs           # persist_schema_snapshot / persist_graph_snapshot
  map/
    mod.rs                # shopify_map_with_runtime (公開エントリ)
    plan.rs               # graph_query_plan / is_doc_like_query
    expand.rs             # expand_graph / center_for_key / collect の細部
    nodes.rs              # graph_map_node / doc_map_node / concept_map_node / node_kind_rank / doc_type_rank / dedupe_docs_by_path
    warnings.rs           # map_coverage_warning
  fetch.rs                # shopify_fetch_from_source / on_demand_fetch_from_input / on_demand_fetch_candidate / fetch_local_doc / fetch_source_doc / fetch_required_text / store_source_doc / ensure_on_demand_enabled
  search/
    mod.rs                # SearchFields / search_schema / create_or_reset_index
    runtime.rs            # SearchRuntime / search_docs_with_runtime / sqlite_like_search
    tokenizer.rs          # register_japanese_tokenizer / japanese_segmenter / query_needs_japanese_tokenizer
    index_io.rs           # rebuild_tantivy_from_db / upsert_tantivy_doc / add_tantivy_doc
  util/
    hash.rs               # hex_sha256
    json.rs               # to_json_value / print_json / merge_json_arrays / doc_json_field / escape_query
    time.rs               # now_iso
  tests/                  # 統合テスト (Phase 9 以降で `tests/` ディレクトリへ移送)
```

## 3. 原則別の設計ガイドライン

### 3.1 カプセル化
- 各モジュールは `pub(crate)` を初期値とし、外部へは最小限のみ `pub` する。
- `Paths` にパス合成責務を集約する (`raw_file`, `config_file` に加え `schema_dir`, `graph_snapshot`, `socket`, `lockfile`, `tantivy_dir` を追加)。呼び出し側では `PathBuf` を手で組まない。
- `ServerState` は `mcp::server` に閉じ、`handle_mcp_request` などのグローバル関数は非公開に変更する。`Arc<ServerState>` が唯一の共有点。
- Tantivy スキーマは `search::SearchFields` / `search::search_schema` に閉じ、`Index` の作成・再構築も `search::index_io` 経由に限定する。

### 3.2 関心の分離
- **純粋 core** (`markdown`, `sitemap`, `url_policy`, `changelog::{feed,impact}`, `graphql::{schema_urls,resolve}` の非DB部, `util/*`, `domain/*`, `db::rows`) は `std::io` / DB / HTTP に依存させない。
- **I/O adapters** (`source::reqwest`, `db/*`, `search::index_io`, `mcp::{daemon,server,framing}`, `fetch`) は trait/関数越しに差し替え可能にする。
- **オーケストレータ** (`app/*`, `mcp::protocol`, `map`, `fetch`, `changelog::poll`, `graphql::build`) は core と adapter を合成するだけで、自ら I/O を直書きしない。
- tests は「純粋 core のプロパティテスト」と「`MockTextSource` 経由の結合テスト」に二分する。

### 3.3 契約による設計
導入するが、急がずフェーズ化する (初期はフリー関数、安定後に型/トレイト化):
- **Newtype**: `DocPath`, `RawDocPath`, `ApiVersion`, `ApiSurface`, `SectionAnchor`, `ConceptId`。`canonical_doc_path` / `raw_path_for` / `concept_id` を smart constructor に格上げし、"検証済み文字列" を型で運ぶ。
- **Trait 境界** (Phase 9 で導入):
  - `TextSource` (既存, 強化: `fetch_text` の事後条件に UTF-8 保証をドキュメント化)
  - `DocRepo` / `GraphRepo` / `ChangelogRepo` / `CoverageRepo` / `MetaRepo` — 抽象を細かく切り、`&Connection` を隠す。
  - `Clock` (`now_iso`) / `Hasher` (`hex_sha256`) — 決定論テスト用。
- **事前/事後条件のドキュメント**: 公開関数は `# Preconditions` / `# Errors` / `# Postconditions` セクションを docコメントに必ず付ける。`debug_assert!` で不変条件を裏取る。
- **Error 型の再設計**: `anyhow::Result` はアプリケーション境界のみ。各モジュールは固有の `thiserror` enum (例: `db::DbError`, `fetch::FetchError`, `mcp::ProtocolError`) を返し、`ToolError` で JSON-RPC エラーに写像する。

### 3.4 副作用の隔離
- HTTP → `source::TextSource` の背後に限定。ReqwestTextSource 以外に `CachingTextSource` や `RateLimitedTextSource` を後付け可能に。
- FS → `Paths` + `std::fs` ラッパー (`paths::atomic_write` など) に集約。
- SQLite → `db/*` 以外からは `rusqlite` 型を露出させない (`Connection` を戻り値にしない)。
- Time/Random → `Clock` / (必要なら) `Rng` trait。
- Process/OS → `mcp::daemon` に閉じ、アプリケーション層は `serve(paths)` / `serve_direct(paths)` のみ呼ぶ。

## 4. 移行フェーズ (すべて緑を保ちながら)

各 phase は独立コミット。`cargo test` / `cargo bench --no-run` / `cargo clippy -- -D warnings` を必須ゲートにする。

| Phase | 内容 | リスク | 既存テスト影響 |
|---|---|---|---|
| 0 | ベースライン: `lib.rs` を導入し `main.rs` を re-export する薄いラッパへ。`pub(crate) use crate::*;` で tests を維持 | 低 | なし |
| 1 | 純粋関数の抽出: `markdown.rs` / `sitemap.rs` / `url_policy.rs` / `util/*` / `changelog::{feed,impact}` の純粋部 / `graphql::{schema_urls,resolve}` の純粋部 / `map::{plan,nodes,warnings}` の純粋部 | 低 | path 変更のみ |
| 2 | ドメイン型を `domain/*` へ移設。impl は一緒に移し、re-export で後方互換 | 低 | なし |
| 3 | `db/` を新設: `open_db` / `init_db` / `ensure_column` → 以後 docs/graph/coverage/changelog/meta/staleness を順次移設。シグネチャは維持 | 中 (SQL の見落とし) | なし |
| 4 | `source/` を新設: `TextSource` / `SourceFetchError` / `ReqwestTextSource` を分離 | 低 | 既に `MockTextSource` 前提なので互換 |
| 5 | `search/` を新設: runtime / tokenizer / schema / index_io に分割 | 中 (tantivy スキーマ互換) | スキーマ変更なし、ベンチも緑 |
| 6 | `mcp/` を完全に分離: framing 移設、protocol/server/daemon/workers に分割。`ServerState` を `pub(crate)` 化 | 中 (daemon 配線) | `mcp_*` テストを更新 |
| 7 | オーケストレータを `app/*` / `fetch.rs` / `map/mod.rs` / `changelog::poll` / `graphql::build` に集約。`main.rs` はここを呼ぶだけに | 中 | 統合テストのみ要調整 |
| 8 | `cli.rs` へ CLI 分離。`main.rs` は `<= 80 LOC` を目標 | 低 | なし |
| 9 | 契約強化: Newtype 導入 (`DocPath` ほか) と repo trait 導入。`Clock` / `Hasher` を DI 化 | 高 (API 波及) | 段階的に更新 |
| 10 | 統合テストを `tests/` へ移送。`lib.rs` の公開 API を最小化 | 中 | 可視性調整 |

## 5. 受け入れ基準

- `main.rs` は **150 LOC 以下** (CLI dispatch と Tokio ランタイム起動のみ)。
- 各モジュールは **600 LOC 以下** を目標、越えた場合は再分割を検討。
- `cargo test --all-targets` / `cargo bench --no-run` / `cargo clippy -- -D warnings` がすべて緑。
- `MockTextSource` を差し替えるだけで CLI 経路の大半 (build / refresh / coverage_repair / changelog poll) がエンドツーエンドでテスト可能。
- `rusqlite::Connection` / `reqwest::Client` / `tantivy::Index` の型は対応する adapter モジュール外に漏れない。
- 公開関数 (`pub` / `pub(crate)`) には doc コメントに事前条件・事後条件・エラー条件が記載される。

## 6. 非目標 / 先送り

- ビジネスロジックの挙動変更 (API 応答・DB スキーマ・プロトコル互換) は行わない。
- 非同期ランタイム置換 (`tokio` → 他) はしない。
- `anyhow` → `thiserror` 完全移行は Phase 9 で部分的にとどめる。完全置換は別計画。
- tokio 対応の `async TextSource` のインターフェース刷新 (例: `bytes::Bytes` 戻り) は行わない。

## 7. 合意済み決定 (2026-04-19)

1. **tests/ 移送する** (Phase 10 を含む)。
2. **Newtype は最小**: `DocPath` と `RawDocPath` の 2 型のみ導入。`canonical_doc_path` / `raw_path_for` を smart constructor に格上げし、raw/canonical の取り違えを型で遮断。`ApiVersion` 等は `String` のまま。
3. **`lib.rs` 公開 API は最小**: tests/ と bench が touch する型・関数のみ `pub`、残りは `pub(crate)`。
4. **`thiserror` を追加**: `DbError` / `FetchError` / `McpError` (+ 既存 `ToolError` をマクロ化) をモジュール別 enum で定義。境界で `anyhow::Error` に合流。
5. **1 PR 完結**: すべての phase を 1 PR にまとめる。ただし commit は phase 単位で刻み、各 commit が `cargo test` / `cargo clippy -- -D warnings` / `cargo bench --no-run` を通すことを必須とする。
