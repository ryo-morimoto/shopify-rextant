# shopify-rextant — Design Document

**Status**: v0.5.0 implementation complete / release-prep pending
**Author**: Maintainers
**Last updated**: 2026-04-18
**Version**: 0.5.0 release candidate (SPEC)

---

## 0. TL;DR

Shopifyアプリ開発中のcoding agentが、shopify.devへの毎回のHTTP fetchで遅くなる問題を解決する**ローカルMCPサーバ**。

「答えを合成する」のではなく、**調査計画を立てるための地図(concept/doc/task graph)を返す**ことで、情報の歪みゼロ・エージェント自律性最大化を両立する。

**実装言語**: Rust。Node.jsでは並列性・検索エンジン(tantivy)・メモリフットプリントで限界があるため。

**配布**: 現時点はsource checkoutからのlocal install/buildを正とする。crates.io / Homebrew / GitHub Releases / NixOS flakeはv1.0公開準備項目。

**現在の実装スナップショット**:

- v0.1: local MCP server、newline-delimited JSON stdio、`shopify_map` / `shopify_fetch` / `shopify_status`
- v0.1.1-v0.1.2: sitemap discovery、coverage reporting、fetch section extraction、transport fixtures
- v0.2: Admin GraphQL concept/doc graph foundation
- v0.3: changelog freshness and scheduled change hydration
- v0.4: Japanese tokenizer integration; edge repair and diagnostics remain roadmap work
- v0.5: on-demand official docs fetch, `refresh --url`, and `coverage repair`

**リリース準備メモ**:

- このSPECはv0.5.0相当の実装契約を記録する。公開リリースは、package metadata、README/CONTRIBUTING、CI/release workflow、配布チャネルが揃った時点で切る
- `Cargo.toml` の package version はリリースタグ直前に意図した公開バージョンへ上げる。`USER_AGENT` とCLI `--version` は `CARGO_PKG_VERSION` に従う
- それまでは `cargo install --path .` / `cargo build --release` と `cargo test` をrelease candidate検証の正とする

---

## 1. 背景と問題

### 1.1 問題領域

Shopifyアプリ/テーマ開発で、coding agent (Claude Code, Codex等) がShopify APIの仕様を確認するたびに以下の遅さが発生する:

- **公式 `@shopify/dev-mcp` の `search_docs_chunks` / `fetch_full_docs` は shopify.dev に都度HTTP fetch**: ネットRTT + サーバ処理で1-3秒/回
- **エージェントの探索ループ回数 × 往復時間**: 1タスクで5-10回fetchすると実時間の大半がwait
- **バージョンとdeprecationの罠**: エージェントが古いフィールドを使うコードを書きがち

### 1.2 非機能要件

1. **Local-first**: ユーザのマシンでstdio経由で動作。リモートサーバ不要
2. **Zero telemetry**: 非公開プロジェクトのコード断片を外部送信しない(公式AI Toolkitの`validate.mjs`問題を回避)
3. **No information distortion**: LLMによる要約・合成を挟まない。原文そのままを返す
4. **Staleness visible**: キャッシュの古さを隠さず構造データで提示
5. **Fast**: 典型クエリで20ms以下、原文取得で5ms以下
6. **Offline-capable**: インデックス構築後はネット切断でも動作(changelog検出だけオンライン必要)

### 1.3 スコープ外

- GraphQL/Liquidの**コード検証**: schemaがあれば型チェックは可能だが、このツールでは扱わない(別ツール `shopify-validate` に分離可能)
- **実店舗に対する操作**: Shopify CLI / Admin APIの実行系機能は担当しない
- **リモート共有**: ローカル環境で完結。チーム共有は別途`cargo publish`やGitHub Releasesで配布する形

---

## 2. 設計の核心原則

### 2.1 Map as response (not answer, not search results)

エージェントが**自律的に航海できる地図**を返す。以下のいずれもしない:

- ❌ 合成された「答え」を返す (情報が歪む、サーバ側LLM必要)
- ❌ 検索結果のフラットなリストだけ返す (エージェントの探索が肥大化)
- ✅ 関連ノード群とエッジ構造を返す (エージェントが読むべき順を自分で決められる)

「地図」とは具体的に: **中心ノード + 近傍ノード群 + エッジ + 各ノードの原文先頭N文字(加工なし) + 読む順の提案**。

### 2.2 No synthesis, ever

サーバ側でLLMを呼ばない。ノードのsummaryは常に**原文の先頭N文字を切り出したもの**。加工ゼロ。これにより:

- 情報の歪みが構造的に起こりえない
- デバッグ可能性が完全 (原文と1:1対応)
- ローカル完結 (LLM API不要)
- 再現性あり (同じ入力→同じ出力、温度パラメータなし)

### 2.3 Staleness is a feature, not a bug

古さを隠すのではなく、レスポンスに数値化して埋め込む。`age_days`、`references_deprecated`、`upcoming_changes`の3軸でエージェントに伝える。エージェントは自身の判断で`shopify_refresh`を呼ぶか、別のソースを当たるか決められる。

### 2.4 Deterministic algorithms only

BFS、トポロジカルソート、BM25スコアリング、すべて決定的。同じインデックスに対して同じクエリ→同じ結果。embeddingやLLMによる非決定性を排除。

### 2.5 Three-layer graph

情報の性質によって3つのグラフを重ねる:

- **Document graph**: ページの階層・順序・"see also"リンク
- **Concept graph**: GraphQL型/mutation/Liquidオブジェクト/Function APIの関係
- **Task graph**: 典型的な実装タスクとそれに必要なconceptの集合

各グラフは独立に構築・更新され、クエリ時にlensで切り替える。

### 2.6 Coverage before cleverness

検索エンジンやgraphが賢くても、対象ドキュメントがindexに入っていなければ失敗する。実用上の優先順位は以下:

1. **Coverage**: shopify.devの該当ドキュメントが漏れなく入っている
2. **Freshness**: 古さとdeprecationが見える
3. **Retrievability**: クエリから該当pathへ到達できる
4. **Graph richness**: 関連型・関連guide・taskの構造が見える

`shopify_map` が検索で外した時にweb searchへ逃げる必要がある状態は、速度以前に地図として失敗。v0.1.1でsitemap取り込みと分類修正を優先し、v0.5で未収録docのオンデマンド回収を追加する。

---

## 3. アーキテクチャ

### 3.1 全体像

```
┌─────────────────────────────────────────────────────────────┐
│  Coding agent (Claude Code / Codex / Cursor)                │
└────────────────────────┬────────────────────────────────────┘
                         │ MCP stdio (JSON-RPC)
                         ▼
┌─────────────────────────────────────────────────────────────┐
│  shopify-rextant (Rust binary)                              │
│                                                              │
│  ┌───────────────────────────────────────────────────────┐  │
│  │ MCP handler (rmcp crate)                              │  │
│  │  tools: shopify_map / shopify_fetch / shopify_status  │  │
│  └────────┬──────────────────────────────────────────────┘  │
│           │                                                  │
│  ┌────────▼──────────────────────────────────────────────┐  │
│  │ Query engine                                           │  │
│  │  - Entry point resolver (type name / path / FTS)      │  │
│  │  - BFS on in-memory ConceptGraph                      │  │
│  │  - Topological sort for reading order                 │  │
│  │  - Staleness hydration                                │  │
│  └────────┬──────────────────────────────────────────────┘  │
│           │                                                  │
│  ┌────────▼──────┐  ┌──────────────┐  ┌──────────────────┐  │
│  │ tantivy       │  │ SQLite (WAL) │  │ In-memory graph  │  │
│  │ (FTS index)   │  │ (metadata)   │  │ (petgraph)       │  │
│  └───────────────┘  └──────────────┘  └──────────────────┘  │
│           ▲                 ▲                                │
│  ┌────────┴─────────────────┴────────────────────────────┐  │
│  │ Background workers (tokio tasks)                      │  │
│  │  - version_watcher (24h)                              │  │
│  │  - changelog_watcher (30min)                          │  │
│  │  - aging_sweeper (6h)                                 │  │
│  │  - edge_repairer (72h)                                │  │
│  └──────────────────────┬────────────────────────────────┘  │
└─────────────────────────┼───────────────────────────────────┘
                          │ HTTPS (reqwest) — writesのみ
                          ▼
┌─────────────────────────────────────────────────────────────┐
│  shopify.dev                                                 │
│  ├─ /llms.txt                                                │
│  ├─ /sitemap.xml                                             │
│  ├─ /**/*.md (.md suffix for raw markdown)                   │
│  ├─ /admin-graphql-direct-proxy/YYYY-MM                      │
│  └─ /changelog (RSS)                                         │
└─────────────────────────────────────────────────────────────┘
```

### 3.2 技術スタック

| レイヤ | 採用 | 理由 |
|---|---|---|
| 言語 | Rust (stable, edition 2024) | 並列性、メモリ効率、起動速度、単一バイナリ配布 |
| 非同期ランタイム | tokio | background worker、reqwest、rmcp全て統合 |
| MCP SDK | `rmcp` (公式Anthropic Rust SDK) | stdio対応、JSON-RPC自動処理 |
| 全文検索 | `tantivy` + `lindera-tantivy` (IPADIC) | <10ms起動、BM25、日本語対応 |
| メタデータDB | `rusqlite` + `r2d2` (pool) + WAL mode | トランザクショナル更新、lock-free read |
| HTTPクライアント | `reqwest` + `rustls` | ETag/If-Modified-Since対応 |
| RSS解析 | `feed-rs` | Atom/RSS両対応 |
| GraphQL解析 | `async-graphql-parser` または `graphql-parser` | SDLをAST化 |
| グラフ | `petgraph` | BFS/Dijkstra/トポソート、in-memory |
| シリアライズ | `serde` + `serde_json` + `rmp-serde` | JSON応答 + graphスナップショット |
| 日時 | `jiff` または `chrono` | RFC3339パース、age計算 |
| ログ | `tracing` + `tracing-subscriber` | stderr only (stdout はMCPプロトコル専用) |

---

## 4. データモデル

### 4.1 ファイルシステムレイアウト

```
~/.shopify-rextant/
├── config.toml                   # 設定ファイル(pin versions等)
├── data/
│   ├── index.db                  # SQLite (WAL)
│   ├── index.db-wal              # WALファイル
│   ├── index.db-shm              # Shared memory
│   ├── tantivy/                  # tantivy index directory
│   │   ├── meta.json
│   │   └── <segment files>
│   ├── graph.msgpack             # petgraphスナップショット(起動高速化用)
│   └── raw/                      # 原文マークダウン
│       ├── docs/
│       │   └── api/
│       │       └── admin-graphql/
│       │           └── 2026-04/
│       │               └── objects/
│       │                   └── Product.md
│       └── ...
└── logs/
    └── worker.log                # tracingログ
```

### 4.2 SQLite schema

```sql
-- バージョン管理
CREATE TABLE schema_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
-- 例: ('schema_version', '1'), ('last_full_build', '2026-04-15T03:02:00Z')

-- ドキュメントメタデータ
CREATE TABLE docs (
  path              TEXT PRIMARY KEY,           -- "/docs/api/admin-graphql/2026-04/objects/Product"
  title             TEXT NOT NULL,
  version           TEXT,                       -- "2026-04" | "2026-07" | "evergreen"
  doc_type          TEXT NOT NULL,              -- "reference" | "tutorial" | "how-to" | "explanation" | "migration" | "changelog"
  api_surface       TEXT,                       -- "admin_graphql" | "storefront" | "hydrogen" | "liquid" | "functions" | "polaris" | "flow" | ...
  content_class     TEXT NOT NULL,              -- "schema_ref" | "api_ref" | "guide" | "tutorial" | "changelog" | "liquid_ref" | "polaris"
  content_sha       TEXT NOT NULL,              -- SHA256 of raw markdown
  etag              TEXT,                       -- Upstream ETag for conditional GET
  upstream_last_modified TEXT,
  last_verified     TEXT NOT NULL,              -- ISO8601
  last_changed      TEXT NOT NULL,              -- content_shaが最後に変わった時刻
  freshness         TEXT NOT NULL,              -- "fresh" | "aging" | "stale" | "rebuilding"
  references_deprecated  INTEGER NOT NULL DEFAULT 0,  -- 0/1
  deprecated_refs   TEXT,                       -- JSON: ["DraftOrderLineItem.grams", ...]
  summary_raw       TEXT NOT NULL,              -- 原文先頭N文字 (加工なし)
  reading_time_min  INTEGER,                    -- 推定読了時間(分)
  raw_path          TEXT NOT NULL,              -- data/raw/ 以下の相対パス
  source            TEXT NOT NULL DEFAULT 'sitemap', -- "llms" | "sitemap" | "on_demand" | "manual"
  hit_count         INTEGER NOT NULL DEFAULT 0  -- クエリで返された回数
);
CREATE INDEX idx_docs_version ON docs(version);
CREATE INDEX idx_docs_surface ON docs(api_surface);
CREATE INDEX idx_docs_class ON docs(content_class, api_surface);
CREATE INDEX idx_docs_freshness ON docs(freshness);
CREATE INDEX idx_docs_source ON docs(source);

-- Coverage report (sitemap/llms から発見したが取り込めなかったURL)
CREATE TABLE coverage_reports (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  source            TEXT NOT NULL,              -- "llms" | "sitemap" | "on_demand"
  canonical_path    TEXT,
  source_url        TEXT NOT NULL,
  status            TEXT NOT NULL,              -- "indexed" | "skipped" | "failed" | "classified_unknown"
  reason            TEXT,                       -- "markdown_not_found" | "network_error" | "outside_scope" | ...
  http_status       INTEGER,
  checked_at        TEXT NOT NULL,
  retry_after       TEXT
);
CREATE INDEX idx_coverage_status ON coverage_reports(status, checked_at);
CREATE INDEX idx_coverage_path ON coverage_reports(canonical_path);

-- Concept グラフ(型・API・機能)
CREATE TABLE concepts (
  id                TEXT PRIMARY KEY,           -- "admin_graphql.2026-04.Product" 等の一意ID
  kind              TEXT NOT NULL,              -- "graphql_type" | "graphql_field" | "graphql_input_object" | "graphql_enum" | "graphql_interface" | "graphql_union" | "graphql_scalar" | "graphql_mutation" | "graphql_query" | "liquid_object" | "function_api" | "polaris_component" | "webhook_topic" | ...
  name              TEXT NOT NULL,              -- "Product", "productCreate", "cart.line_items"
  version           TEXT,
  defined_in_path   TEXT,                       -- docs.pathへの参照
  deprecated        INTEGER NOT NULL DEFAULT 0,
  deprecated_since  TEXT,                       -- "2026-07" 等
  deprecation_reason TEXT,
  replaced_by       TEXT,                       -- 別conceptのid
  kind_metadata     TEXT                        -- JSON: kind固有の詳細(フィールド一覧、型引数等)
);
CREATE INDEX idx_concepts_name ON concepts(name);
CREATE INDEX idx_concepts_kind_version ON concepts(kind, version);

-- エッジ(conceptとdocを横断)
CREATE TABLE edges (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  from_type         TEXT NOT NULL,              -- "concept" | "doc" | "task"
  from_id           TEXT NOT NULL,
  to_type           TEXT NOT NULL,
  to_id             TEXT NOT NULL,
  kind              TEXT NOT NULL,              -- "defined_in" | "used_in" | "see_also" | "parent_of" | "next" | "prev" | "replaces" | "teaches" | "requires" | "composed_of" | "has_field" | "returns" | "accepts_input" | "implements" | "member_of" | "references_type"
  weight            REAL NOT NULL DEFAULT 1.0,  -- BFS時の重み
  source_path       TEXT,                       -- このエッジを抽出した元ドキュメント
  extracted_at      TEXT NOT NULL
);
CREATE INDEX idx_edges_from ON edges(from_type, from_id);
CREATE INDEX idx_edges_to ON edges(to_type, to_id);
CREATE INDEX idx_edges_kind ON edges(kind);

-- Taskグラフ(実装タスクの抽出)
CREATE TABLE tasks (
  id                TEXT PRIMARY KEY,           -- "build_discount_function"
  title             TEXT NOT NULL,
  description       TEXT,
  root_path         TEXT,                       -- 主要tutorialページ
  related_paths    TEXT NOT NULL                -- JSON配列
);

-- Changelog (RSS由来)
CREATE TABLE changelog_entries (
  id                TEXT PRIMARY KEY,           -- RSS guidまたはURL
  posted_at         TEXT NOT NULL,
  title             TEXT NOT NULL,
  body              TEXT,
  url               TEXT NOT NULL,
  is_breaking       INTEGER NOT NULL DEFAULT 0,
  affected_types    TEXT,                       -- JSON: concepts/docsで解決済みの参照 ["admin_graphql.2026-04.DraftOrderLineItem.grams", ...]
  unresolved_affected_refs TEXT,                -- JSON: changelogから抽出したがindex上で未解決の候補 ["UnknownType.foo", ...]
  affected_surfaces TEXT,                       -- JSON: ["admin_graphql", ...]
  processed_at      TEXT
);
CREATE INDEX idx_changelog_posted_at ON changelog_entries(posted_at);

-- スケジュール済み破壊的変更
CREATE TABLE scheduled_changes (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  effective_date    TEXT NOT NULL,              -- "2026-07-01"
  type_name         TEXT NOT NULL,              -- "DraftOrderLineItem.grams"
  change_kind       TEXT NOT NULL,              -- "removal" | "deprecation" | "breaking_signature_change"
  description       TEXT NOT NULL,
  migration_hint    TEXT,
  source_changelog_id TEXT,
  FOREIGN KEY (source_changelog_id) REFERENCES changelog_entries(id)
);
CREATE INDEX idx_scheduled_effective ON scheduled_changes(effective_date);
CREATE INDEX idx_scheduled_type ON scheduled_changes(type_name);

-- クエリログ(学習用、LLM不使用)
CREATE TABLE query_log (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  ts                TEXT NOT NULL,
  tool              TEXT NOT NULL,              -- "shopify_map" | "shopify_fetch"
  args              TEXT NOT NULL,              -- JSON
  returned_ids      TEXT,                       -- JSON配列
  latency_ms        INTEGER,
  client_info       TEXT                        -- 可能ならMCPクライアント識別
);
CREATE INDEX idx_querylog_ts ON query_log(ts);

-- バージョンインデックス(どのバージョンが取り込み済みか)
CREATE TABLE indexed_versions (
  version           TEXT PRIMARY KEY,           -- "2026-04"
  api_surface       TEXT NOT NULL,              -- "admin_graphql" 等
  indexed_at        TEXT NOT NULL,
  doc_count         INTEGER NOT NULL,
  status            TEXT NOT NULL               -- "active" | "archived" | "failed"
);
```

**WAL mode設定**: 起動時に `PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;` を発行。

### 4.3 Tantivy schema

```rust
let mut schema_builder = Schema::builder();

// 識別子・フィルタ用(STORED, 検索しない)
schema_builder.add_text_field("path", STRING | STORED);
schema_builder.add_text_field("version", STRING | STORED | INDEXED);
schema_builder.add_text_field("doc_type", STRING | STORED | INDEXED);
schema_builder.add_text_field("api_surface", STRING | STORED | INDEXED);
schema_builder.add_text_field("content_class", STRING | STORED | INDEXED);

// 検索対象(TextOptions::IndexedText)
// titleは英数 tokenizer (UAX29 + lowercase)
let title_options = TextOptions::default()
    .set_indexing_options(TextFieldIndexing::default()
        .set_tokenizer("default")
        .set_index_option(IndexRecordOption::WithFreqsAndPositions))
    .set_stored();
schema_builder.add_text_field("title", title_options);

// contentは英数 + 日本語(lindera)を両方試すmulti-tokenizer
// tantivy 0.25+ は `per-field tokenizer` で対応
let content_en = TextOptions::default()
    .set_indexing_options(TextFieldIndexing::default()
        .set_tokenizer("en_stem")
        .set_index_option(IndexRecordOption::WithFreqsAndPositions));
schema_builder.add_text_field("content_en", content_en);

let content_ja = TextOptions::default()
    .set_indexing_options(TextFieldIndexing::default()
        .set_tokenizer("lindera_ipadic")
        .set_index_option(IndexRecordOption::WithFreqsAndPositions));
schema_builder.add_text_field("content_ja", content_ja);

// Conceptの関連名(型名で一発ヒットさせる用)
schema_builder.add_text_field("related_concepts", TEXT | STORED);

let schema = schema_builder.build();
```

**なぜcontentをen/ja両方indexするか**: shopify.devは英語中心だが、エージェントが日本語クエリを投げる場合がある(例: "割引 クーポン 重ね掛け")。日本語tokenizerだと`admin.combinesWith`のような英語型名を切りすぎるので、英語tokenizerと両方持つ。クエリ時に両フィールドにOR検索、スコア合算。

**原文本体はtantivyに入れない**: `raw_path`で`data/raw/`のファイルを指すだけ。tantivyに全文を入れるとインデックスサイズが肥大化する。検索用には`content_en`/`content_ja`フィールドに本文の2000文字程度のprefixを入れる(冒頭がもっとも意味密度が高い前提)。

### 4.4 In-memory graph (petgraph)

```rust
pub struct ConceptGraph {
    graph: petgraph::graph::DiGraph<NodeData, EdgeData>,
    by_concept_id: HashMap<String, NodeIndex>,
    by_doc_path: HashMap<String, NodeIndex>,
    by_task_id: HashMap<String, NodeIndex>,
    by_concept_name_version: HashMap<(String, String), NodeIndex>,  // ("Product", "2026-04")
}

#[derive(Clone, Debug)]
pub enum NodeData {
    Concept(ConceptNode),
    Doc(DocNode),
    Task(TaskNode),
}

#[derive(Clone, Debug)]
pub struct EdgeData {
    kind: EdgeKind,
    weight: f32,
    source_path: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    DefinedIn, UsedIn, SeeAlso, ParentOf, Next, Prev,
    Replaces, Teaches, Requires, ComposedOf,
    HasField, Returns, AcceptsInput, Implements, MemberOf, ReferencesType,
}
```

**起動時の読み込み**: `data/graph.msgpack` があればそれを `rmp_serde::from_slice` で一発ロード(<10ms)。なければSQLiteから再構築(100-300ms)。

**更新時**: background workerが新しいグラフを裏で構築し、`Arc<ArcSwap<ConceptGraph>>` で原子的に差し替え。既存readerは前バージョンを使い続ける。

---

## 5. MCP ツール仕様

### 5.1 `shopify_map`

調査計画を立てるための地図を返す。最も頻繁に使われるツール。

**v0.1系の正直な制約:** v0.1はconcept graphを持たないため、`shopify_map` は「graph map」ではなく **FTS候補にstalenessと読む順のヒントを付けた調査入口** として返す。レスポンスの `query_plan` / `meta.query_interpretation` には、`resolved_as="free_text"`、`graph_available=false`、`coverage_warning` などを明示し、エージェントが「構造化された関連graph」だと誤解しないようにする。v0.2でconcept/doc/task graphが入った時点で本来のmapになる。

**Input schema (JSON Schema):**

```json
{
  "type": "object",
  "required": ["from"],
  "properties": {
    "from": {
      "type": "string",
      "description": "起点。GraphQL型名(Product, DraftOrderLineItem)、ドキュメントパス(/docs/api/...)、タスク名(build-discount-function)、または自由テキストクエリ(discount function cart level)のいずれか。サーバが自動判別する。"
    },
    "radius": {
      "type": "integer",
      "enum": [1, 2, 3],
      "default": 2,
      "description": "起点から何ホップまで展開するか"
    },
    "lens": {
      "type": "string",
      "enum": ["concept", "doc", "task", "auto"],
      "default": "auto",
      "description": "どのグラフを主軸に展開するか。autoは起点の種類から推定"
    },
    "version": {
      "type": "string",
      "description": "pinするAPIバージョン。例: '2026-04'。省略時はconfig.tomlのpinned_version、それもなければlatest"
    },
    "max_nodes": {
      "type": "integer",
      "default": 30,
      "minimum": 1,
      "maximum": 100
    }
  }
}
```

**Output schema:**

```typescript
interface MapResponse {
  center: {
    id: string;                   // ノードID
    kind: "concept" | "doc" | "task";
    path?: string;                // docの場合
    title: string;
  };

  nodes: Array<{
    id: string;
    kind: "concept" | "doc" | "task";
    subkind?: string;             // "graphql_type" | "tutorial" | "reference" | ...
    path?: string;
    title: string;
    summary_from_source: string;  // 原文先頭N文字、加工なし、最大400文字
    version?: string;
    api_surface?: string;
    doc_type?: string;
    reading_time_min?: number;
    staleness: {
      age_days: number;
      freshness: "fresh" | "aging" | "stale";
      content_verified_at: string;        // ISO8601
      schema_version?: string;
      references_deprecated: boolean;
      deprecated_refs: string[];          // 例: ["DraftOrderLineItem.grams"]
      upcoming_changes: Array<{
        effective_date: string;
        change: string;
        migration_hint?: string;
      }>;
    };
    distance_from_center: number;  // BFSのホップ数
  }>;

  edges: Array<{
    from: string;                 // ノードID
    to: string;
    kind: string;                 // EdgeKind
    weight: number;
  }>;

  suggested_reading_order: string[];  // paths, ordered

  query_plan: Array<{
    step: number;
    action: "fetch" | "inspect_status" | "refresh" | "fallback_to_official_docs";
    path?: string;
    reason: string;                 // 原文根拠を読むための短い理由。回答合成はしない
  }>;

  meta: {
    generated_at: string;
    index_age_days: number;
    versions_available: string[];   // ["2026-01", "2026-04"]
    version_used: string;
    coverage_warning?: string;       // "Index is >14 days old; run shopify_refresh"
    graph_available: boolean;        // v0.1=false, v0.2以降true
    index_status?: {
      doc_count: number;
      skipped_count?: number;
      failed_count?: number;
    };
    on_demand_candidate?: {           // v0.5.0。0件時の回収候補
      url: string;
      enabled: boolean;
      reason: string;
    };
    query_interpretation: {          // 入力をどう解釈したか(デバッグ用)
      resolved_as: "concept_name" | "doc_path" | "task_name" | "free_text";
      entry_points: string[];
      confidence: "exact" | "high" | "medium" | "low";
    };
  };
}
```

**処理ロジック:**

1. `from`の解釈(決定的ルール順)
   - (a) `/docs/...` で始まる → `doc_path`として扱う
   - (b) タスクID形式(`[a-z][a-z0-9-]*`かつ`tasks`テーブルにマッチ) → `task_name`
   - (c) GraphQL型名形式(`^[A-Z][A-Za-z0-9]*$` かつ `concepts.name`にマッチ) → `concept_name`
   - (d) それ以外 → `free_text` (tantivyで検索)
2. 起点候補の特定
   - (a)(b)(c)の場合はDBから直接ノードID取得
   - (d)の場合はtantivyでtop-3、複数起点としてmulti-source BFS
   - path重複は必ずdedupeする。`nodes[].path` が同じものを複数返さない
3. `lens`の解決(autoの場合)
   - doc_path起点 → `lens=doc`
   - concept_name起点 → `lens=concept`
   - task_name起点 → `lens=task`
   - free_text起点 → `lens=concept` (conceptが最も情報密度高い)
4. BFS展開
   - petgraphの`Bfs`を起点から`radius`深度まで
   - エッジ`weight`で打ち切り(累積重み < 閾値)
   - 同じ`subkind`のノードが溢れたら距離でtrim
5. ノードメタ付与
   - 各ノードIDに対してSQLite prepared statement 1回
   - stalenessの`age_days` = (now - `last_verified`) / 86400
   - `upcoming_changes` = `scheduled_changes`のうち該当type_nameのもの
6. `suggested_reading_order`の計算(ルール、LLMなし)
   - rankマップ: `{overview: 0, tutorial: 1, how-to: 2, reference: 3, migration: 4}`
   - rank順にsort → 同rank内はconceptの依存順でトポソート(型Aが型Bを参照するならA→B)
7. JSON serialize

**Map UX guardrails (implemented baseline):**

- `nodes` は `path` で安定dedupeする
- `center` は最高scoreのdocだが、`meta.query_interpretation.entry_points` に上位候補を全て残す
- `query_plan` は「次に `shopify_fetch` すべきpath」を短く返す。検索語の言い換えや自然文回答はしない
- 検索結果0件なら、`index_status.doc_count` と `coverage_warning` を返し、v0.5のオンデマンド回収が使える場合はその候補URLを示す
- graphが使えない場合は `edges=[]`、`meta.graph_available=false`、coverage/graph warningを返し、FTS導線にfallbackする

**v0.2.0 graph contract:**

- `meta.graph_available=true` は、少なくとも対象APIバージョンのAdmin GraphQL schemaから `concepts` と `edges` を構築し、`shopify_map` がpetgraph BFSで展開している時だけ返す
- `concept_name` 起点で `concepts.name` が複数バージョンに存在する場合、`version`引数 > `config.toml`の`pinned_version` > latest stable の順で1バージョンに解決する
- `free_text` 起点はbaseline FTS top-kを使うが、返されたdoc pathを `defined_in` / `references_type` edgeでconcept起点へ昇格してからBFSする。昇格できないdocはdocノードとして残す
- `edges` はレスポンスに含めた `nodes[].id` 間のedgeだけを返す。レスポンス外ノードへのedgeは返さない
- `suggested_reading_order` はdoc pathだけを含める。concept/taskノードは読む対象ではないため、対応する `defined_in` doc pathがある場合だけorderに入る
- concept hitで `edges=[]` になる場合、`query_plan[0].action="inspect_status"` とし、`meta.coverage_warning` にgraph構築失敗またはschema coverage不足を明示する
- v0.2.0はAdmin GraphQL graphを最小対象とする。Storefront GraphQL、Liquid、Functions、Polarisのconcept抽出は既存doc/FTS導線を維持し、同じgraph基盤へ後続追加する

### 5.2 `shopify_fetch`

原文を返す。加工なし。

**Input schema:**

```json
{
  "type": "object",
  "anyOf": [
    { "required": ["path"] },
    { "required": ["url"] }
  ],
  "properties": {
    "path": {
      "type": "string",
      "description": "docs path. shopify_mapの結果から得られるpath"
    },
    "anchor": {
      "type": "string",
      "description": "セクションアンカー(h1/h2/h3のslug)。指定するとそのセクションだけ返す"
    },
    "url": {
      "type": "string",
      "description": "v0.5.0。shopify.dev/docs または shopify.dev/changelog 配下のURL。未収録docをオンデマンド取得する時だけ使う"
    },
    "include_code_blocks": {
      "type": "boolean",
      "default": true
    },
    "max_chars": {
      "type": "integer",
      "default": 20000,
      "description": "返却文字数上限"
    }
  }
}
```

**Output:**

```typescript
interface FetchResponse {
  path: string;
  content: string;                // 原文markdown、加工なし
  title: string;
  version?: string;
  staleness: StalenessInfo;       // shopify_mapと同じ構造
  sections?: Array<{              // anchorで見出し一覧だけ欲しい時用
    anchor: string;
    title: string;
    level: number;
    char_range: [number, number];
  }>;
  truncated: boolean;             // max_charsに達した場合true
  source_url: string;             // "https://shopify.dev/..."
}
```

**Indexed path behavior:**

- `path` は既存index内の正規pathだけを受け付ける
- `anchor` 指定時は、該当headingから次の同階層以上のheading直前までの原文Markdownを返す
- `include_code_blocks=false` の時だけ fenced code block を除外できる。ただしデフォルトは原文忠実性を優先して `true`
- path未検出時は `Path not found` と `index_status.doc_count` を返す。ネットワーク取得はしない

**On-demand behavior (v0.5 implemented):**

- `url` または未収録のshopify.dev docs pathを受け取った場合、設定で許可されていればオンデマンドfetchしてraw保存、docs upsert、tantivy差分投入を行う
- オンデマンド取得は `shopify.dev` の `/docs/**` と `/changelog/**` のみに制限する。任意URL fetchにはしない
- `[index].enable_on_demand_fetch=false` の場合、HTTP requestを送らず `-32007` と候補URLを返す
- URL scope違反の場合、HTTP requestを送らず `-32008` を返す
- newly recovered docs は `source="on_demand"` として保存する。既存の `llms` / `sitemap` 由来docをon-demandでrefreshしてもsourceはdowngradeしない

**処理**: `docs.raw_path`を引いて`data/raw/`下のファイルをmmapで読み、anchor指定があればmarkdown parser(例: `pulldown-cmark`)でセクション抽出。

### 5.3 `shopify_status`

インデックス状態確認。エージェントが「今の情報は古すぎるか?」を判断するのに使う。

**Input**: なし

**Output:**

```typescript
interface StatusResponse {
  schema_version: string;
  data_dir: string;
  index_built: boolean;
  doc_count: number;
  last_full_build?: string;       // ISO8601
  index: {
    concept_count: number;
    edge_count: number;
    graph_snapshot: boolean;
  };
  coverage: {
    last_sitemap_at?: string;      // ISO8601
    discovered_count: number;
    indexed_count: number;
    skipped_count: number;
    failed_count: number;
    classified_unknown_count: number;
    sources: {
      llms: number;
      sitemap: number;
      on_demand: number;
      manual: number;
    };
  };
  freshness: {
    fresh_count: number;
    aging_count: number;
    stale_count: number;
  };
  workers: {
    last_changelog_at?: string;
    last_aging_sweep_at?: string;
    last_version_check_at?: string;
  };
  changelog: {
    entry_count: number;
    scheduled_change_count: number;
    unresolved_ref_count: number;
    last_warning?: string;
  };
  warnings: string[];             // 例: "Changelog not polled for 2 days; network may be unavailable"
}
```

---

## 6. インデックス構築

### 6.1 初回ビルド (`shopify-rextant build`)

```
Phase 1: Sitemap discovery
  1. GET https://shopify.dev/llms.txt → LLM向け主要URLリスト取得
  2. GET https://shopify.dev/sitemap.xml → shopify.dev全体のURL候補取得
  3. sitemapから /docs/**, /changelog/** のみ抽出
  4. llms.txt由来URLとsitemap由来URLを正規化してunion
  5. URLごとに content_class / api_surface / doc_type を分類(規則ベース)

Phase 2: Content fetch (並列、concurrency=8)
  6. 各URL + ".md" で原文Markdown取得
     - If-None-Match / If-Modified-Since で既存分はスキップ
     - 成功したら data/raw/ に保存
     - .md が404なら .txt を試す
     - htmlしか存在しないURLはv0.1.1ではskipし、coverage_reportに記録
  7. content_shaを計算して docs テーブルへupsert
  8. タイトル抽出(最初の # 行)、frontmatter title fallback、reading_time_min推定

Phase 3: Admin GraphQL schema snapshots (v0.2.0)
  9. POST https://shopify.dev/admin-graphql-direct-proxy/{version} (全indexed_versions)
     - GraphQL introspection queryを送る。認証済みストアのAdmin API endpointは使わない
     - fields/inputFields/enumValues は deprecated を含めて取得する
     - data/schemas/admin-graphql/{version}.introspection.json に保存する
  10. introspection JSONをパースして concepts / edges(GraphQL型間) をbulk insert
     - Object/InputObject/Interface/Union/Enum/Scalar をconcept化する
     - field/input field/enum valueは親concept配下のconceptとして保持する
     - `defined_in`, `has_field`, `returns`, `accepts_input`, `implements`, `member_of`, `references_type` edgeを作る

Phase 4: Extract edges from guides
  11. 各guide/tutorialのmarkdownをパース
  12. GraphQLコードブロックから型名抽出 → `references_type` edge追加
  13. "See also" / "Related resources"セクションから `see_also` edge
  14. 親子階層から `parent_of` / `next` / `prev` edge

Phase 5: Extract tasks
  15. /docs/apps/build/**/tutorials/** と /docs/storefronts/**/tutorials/** をtask候補として登録
  16. 各taskから参照されるconcepts/docsを related_paths に記録

Phase 6: Tantivy indexing
  17. docs.raw_path → content_en/content_ja フィールドに投入
  18. index.commit()

Phase 7: Graph snapshot
  19. メモリ上graphを構築
  20. data/graph.msgpack に rmp_serde::to_vec で保存

Phase 8: Metadata
  21. coverage_report を保存
  22. schema_meta.last_full_build を更新
```

**所要時間**: shopify.dev全体で~2000ページ、~50MB。初回~2-5分。差分更新は~30秒。

**v0.1.1 coverage contract:**

- `llms.txt` だけをsource of truthにしない。`llms.txt` は主要導線、`sitemap.xml` は網羅性のために必須
- sitemapから抽出したURLは `canonical_doc_path(url)` で正規化し、末尾スラッシュ、`.md`、`.txt`、fragment、queryを取り除く
- `source_url.html` / `source_url.md` frontmatterがある場合はcanonical pathと照合し、不一致をcoverage warningに記録する
- 取得できなかったURL、raw markdownが存在しないURL、分類不能URLは `coverage_report` に残す
- `shopify_status` は `doc_count` だけでなく `coverage.skipped_count`、`coverage.failed_count`、`coverage.last_sitemap_at` を返す

**Why:** `optional_scopes` のような実装上重要なページが `llms.txt` に無いだけで検索不能になるのは、ローカルMCPとして致命的。まず漏れを潰す。

### 6.2 分類ルール(path prefix → メタデータ)

```rust
fn classify(path: &str) -> (ContentClass, ApiSurface, DocType) {
    match path {
        // GraphQL reference
        p if p == "/docs/api/admin-graphql"
            => (ApiRef, AdminGraphql, Reference),
        p if p.starts_with("/docs/api/admin-graphql/") && p.contains("/objects/")
            => (SchemaRef, AdminGraphql, Reference),
        p if p.starts_with("/docs/api/admin-graphql/") && p.contains("/mutations/")
            => (SchemaRef, AdminGraphql, Reference),
        p if p.starts_with("/docs/api/admin-graphql/") && p.contains("/queries/")
            => (SchemaRef, AdminGraphql, Reference),
        p if p.starts_with("/docs/api/admin-graphql/")
            => (ApiRef, AdminGraphql, Reference),
        p if p == "/docs/api/storefront"
            => (ApiRef, Storefront, Reference),
        p if p.starts_with("/docs/api/storefront/")
            => (ApiRef, Storefront, Reference),
        p if p.starts_with("/docs/api/liquid/")
            => (LiquidRef, Liquid, Reference),

        // Release notes / changelog
        p if p.starts_with("/docs/api/release-notes/")
            => (Changelog, Unknown, Reference),
        p if p == "/changelog"
            => (Changelog, Unknown, Reference),

        // Guides
        p if p.starts_with("/docs/apps/build/functions/")
            => (Guide, Functions, infer_doc_type(path)),
        p if p.starts_with("/docs/apps/build/")
            => (Guide, Admin, infer_doc_type(path)),
        p if p.starts_with("/docs/storefronts/headless/hydrogen/")
            => (Guide, Hydrogen, infer_doc_type(path)),
        p if p.starts_with("/docs/storefronts/themes/")
            => (Guide, Liquid, infer_doc_type(path)),

        // Polaris
        p if p.starts_with("/docs/apps/build/design/polaris/")
            => (Polaris, Polaris, Reference),

        _ => (Guide, Unknown, Explanation)
    }
}

fn infer_doc_type(path: &str) -> DocType {
    if path.contains("/tutorials/") || path.contains("/getting-started/") { Tutorial }
    else if path.contains("/overview") || path.contains("/concepts/") { Explanation }
    else if path.contains("/migration") || path.contains("/migrating") { Migration }
    else { HowTo }
}
```

70-80%の精度で十分。query_logから漏れを発見して月次でrule追加。

### 6.3 GraphQL introspectionからのconcept/edge抽出

v0.2.0はAdmin GraphQL schema direct proxyからGraphQL introspection JSONを取得してconcept graphを作る。公式のGraphQL codegen設定が `schema: 'https://shopify.dev/admin-graphql-direct-proxy/{version}'` を使うため、このURLをschema sourceとして扱う。ライブストアのAdmin API endpointは認証が必要なので、このツールのlocal docs index構築には使わない。

```rust
// 擬似コード
let schema = read_introspection_json("data/schemas/admin-graphql/2026-04.introspection.json")?;

for ty in schema.types {
    match ty.kind {
        TypeKind::Object => {
            let concept_id = format!("admin_graphql.2026-04.{}", ty.name);
            insert_concept(&conn, Concept {
                id: concept_id.clone(),
                kind: "graphql_type".to_string(),
                name: ty.name,
                version: Some("2026-04".to_string()),
                defined_in_path: doc_path_for_graphql_type("2026-04", &ty.name),
                deprecated: false,
                ...
            });

            if let Some(path) = doc_path_for_graphql_type("2026-04", &ty.name) {
                insert_edge(&conn, Edge {
                    from_type: "concept", from_id: &concept_id,
                    to_type: "doc", to_id: &path,
                    kind: "defined_in",
                });
            }

            for field in ty.fields {
                let field_concept_id = format!("{}.{}", concept_id, field.name);
                insert_concept(&conn, Concept {
                    id: field_concept_id.clone(),
                    kind: "graphql_field".to_string(),
                    name: format!("{}.{}", ty.name, field.name),
                    deprecated: field.is_deprecated,
                    deprecation_reason: field.deprecation_reason,
                    ...
                });
                insert_edge(&conn, Edge {
                    from: &concept_id, to: &field_concept_id,
                    kind: "has_field",
                });

                if let Some(target_type) = named_type(&field.type_ref) {
                    insert_edge(&conn, Edge {
                        from: &field_concept_id,
                        to: &format!("admin_graphql.2026-04.{}", target_type),
                        kind: "returns",
                    });
                }
            }
        },
        // InputObject, Interface, Union, Enum, Scalar 同様
        _ => ()
    }
}
```

### 6.4 Guideからの参照抽出

```rust
// Markdownのコードブロック内のGraphQLクエリから型名抽出
for doc in guides {
    let md = fs::read_to_string(&doc.raw_path)?;
    let parser = pulldown_cmark::Parser::new(&md);
    let mut in_graphql_block = false;
    let mut block_content = String::new();

    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang))) if lang.contains("graphql") => {
                in_graphql_block = true;
            }
            Event::End(Tag::CodeBlock(_)) => {
                if in_graphql_block {
                    let referenced_types = extract_type_names_from_graphql(&block_content);
                    for t in referenced_types {
                        insert_edge(&conn, Edge {
                            from_type: "doc", from_id: &doc.path,
                            to_type: "concept", to_id: &format!("admin_graphql.2026-04.{}", t),
                            kind: "references_type",
                            source_path: Some(doc.path.clone()),
                        });
                    }
                    block_content.clear();
                    in_graphql_block = false;
                }
            }
            Event::Text(t) if in_graphql_block => block_content.push_str(&t),
            _ => ()
        }
    }
}
```

型名抽出は簡易パーサで十分(100%の精度は不要、漏れは`edge_repairer`が月次で拾う)。

---

## 7. Background workers

全てtokio taskとして起動。MCPサーバプロセスが生きている間だけ動く。

### 7.1 `version_watcher` (24h interval)

```rust
async fn version_watcher(app: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(86400));
    loop {
        interval.tick().await;
        if let Err(e) = check_new_versions(&app).await {
            tracing::warn!("version_watcher error: {}", e);
        }
    }
}

async fn check_new_versions(app: &AppState) -> Result<()> {
    // 1. Shopifyのバージョンリストを取得(changelogまたはversioning pageから)
    let latest = fetch_latest_api_version().await?;

    // 2. 現在indexed_versionsにあるか?
    if !app.has_version(&latest).await? {
        // 3. 新バージョンの取り込みをenqueue
        app.enqueue_full_rebuild_for_version(&latest).await?;
        // freshness="rebuilding" で既存レスポンスは継続
    }
    Ok(())
}
```

Shopifyは四半期ごとにリリースなので、通常24h interval で十分。

### 7.2 `changelog_watcher` (30min interval)

```rust
async fn changelog_watcher(app: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(1800));
    loop {
        interval.tick().await;
        if let Err(e) = poll_changelog(&app).await {
            tracing::warn!("changelog_watcher error: {}", e);
        }
    }
}

async fn poll_changelog(app: &AppState) -> Result<()> {
    // 1. RSS取得(ETag/If-Modified-Since付き)
    let feed = app.http.get("https://shopify.dev/changelog/feed")
        .header("If-None-Match", app.last_changelog_etag().await?)
        .send().await?;

    if feed.status() == 304 { return Ok(()); }

    // 2. パース
    let body = feed.text().await?;
    let parsed = feed_rs::parser::parse(body.as_bytes())?;

    // 3. 新エントリ処理
    for entry in parsed.entries {
        if app.has_changelog_entry(&entry.id).await? { continue; }

        // 4. 影響範囲候補を推定(正規表現ベース)
        let affected_candidates = extract_affected_refs(&entry.title, &entry.content);
        // 5. schema/doc indexをSSoTとして解決できた候補だけを採用する
        let impact = resolve_changelog_impact(&app, affected_candidates).await?;
        let is_breaking = entry.categories.iter().any(|c| c.term == "breaking")
            || entry.title.contains("removed")
            || entry.title.contains("breaking");

        // 6. changelog_entriesに挿入
        app.insert_changelog_entry(ChangelogEntry {
            id: entry.id,
            title: entry.title,
            ...
            is_breaking,
            affected_types: impact.resolved_refs.clone(),
            unresolved_affected_refs: impact.unresolved_refs.clone(),
        }).await?;

        // 7. scheduled_changesを抽出(「XXが2026-07で削除」のようなパターン)
        //    ただしtype_nameはresolved_refsに含まれるconcept/docだけを採用する
        if let Some(sc) = extract_scheduled_change(&entry) {
            app.insert_scheduled_change(sc).await?;
        }

        // 8. 影響を受けるdocsにreferences_deprecated フラグ立て
        for resolved_ref in &impact.resolved_refs {
            app.mark_docs_referencing(resolved_ref).await?;
        }

        // 9. breaking changeなら該当docを即座に再fetch
        if is_breaking {
            for resolved_ref in &impact.resolved_refs {
                for path in app.docs_referencing_type(resolved_ref).await? {
                    app.enqueue_doc_refresh(&path, Priority::High).await?;
                }
            }
        }
    }
    Ok(())
}
```

### 7.3 `aging_sweeper` (6h interval)

```rust
async fn aging_sweeper(app: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(21600));
    loop {
        interval.tick().await;
        if let Err(e) = sweep_aging(&app).await {
            tracing::warn!("aging_sweeper error: {}", e);
        }
    }
}

async fn sweep_aging(app: &AppState) -> Result<()> {
    // 1. freshness遷移
    app.exec("UPDATE docs SET freshness='aging'
              WHERE freshness='fresh' AND last_verified < datetime('now','-7 days')").await?;
    app.exec("UPDATE docs SET freshness='stale'
              WHERE freshness='aging' AND last_verified < datetime('now','-30 days')").await?;

    // 2. stale docsの再検証(conditional GET)
    let stale_docs = app.query_as::<Doc>("SELECT * FROM docs WHERE freshness='stale' LIMIT 100").await?;

    for doc in stale_docs {
        match app.conditional_refetch(&doc).await {
            Ok(RefetchResult::NotModified) => {
                app.exec_prepared("UPDATE docs SET last_verified=? WHERE path=?",
                    &[&now_iso(), &doc.path]).await?;
            }
            Ok(RefetchResult::Modified { new_content, new_sha, new_etag }) => {
                app.replace_doc(&doc, new_content, new_sha, new_etag).await?;
                app.reindex_tantivy(&doc.path, &new_content).await?;
            }
            Err(e) => {
                tracing::warn!("refetch failed for {}: {}", doc.path, e);
                // スキップ、次回に再挑戦
            }
        }
    }
    Ok(())
}
```

### 7.4 `edge_repairer` (72h interval)

Guideからのedge抽出は正規表現ベースで70-80%の精度なので、定期的に再抽出して精度を上げる:

```rust
async fn edge_repairer(app: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(72 * 3600));
    loop {
        interval.tick().await;
        if let Err(e) = repair_edges(&app).await {
            tracing::warn!("edge_repairer error: {}", e);
        }
    }
}

async fn repair_edges(app: &AppState) -> Result<()> {
    // 1. query_logから「返されたけどクエリ直後に別のdocが要求された」パターンを検出
    //    → missing edgeの候補
    let missing_edge_candidates = app.detect_missing_edges().await?;

    // 2. 各候補について、docsの内容を再パースしてedge追加
    for (from_path, suspected_to) in missing_edge_candidates {
        let md = fs::read_to_string(path_to_raw(&from_path))?;
        if md.contains(&suspected_to) {
            app.insert_edge_if_missing(&from_path, &suspected_to, EdgeKind::SeeAlso).await?;
        }
    }
    Ok(())
}
```

**Changelog impact resolution contract (v0.3.0):**

- changelog watcherは、title/body/link/categoriesから `Type.field`、GraphQL type名、API version、docs URL/path、API surface category などの候補を正規表現で抽出する。
- 抽出候補はそのまま真として扱わない。impactのSSoTは `concepts` / `docs` / `edges` とし、index上で解決できた候補だけを `affected_types` と `scheduled_changes.type_name` に採用する。
- `concepts.id` に解決できる候補はconcept impact、`docs.path` に解決できる候補はdoc impactとして扱う。doc impactからconceptへ到達できる場合は `edges` 経由で関連concept/docsも影響範囲に含める。
- index上で解決できない候補は `unresolved_affected_refs` に保存するが、`references_deprecated` や `upcoming_changes` には反映しない。
- `affected_surfaces` はRSS categoryと解決済みconcept/docの `api_surface` から導出する。changelog本文だけでsurfaceを確定しない。

---

## 8. クエリ処理の詳細

### 8.1 `shopify_map` フロー(ナノ秒レベル想定レイテンシ付き)

```
[step 1: 入力解釈] <100µs
  match from {
    path @ "/docs/..." => DocPath(path),
    id @ "^[a-z][a-z0-9-]*$" if task_exists(id) => TaskId(id),
    name @ "^[A-Z][A-Za-z0-9]*$" if concept_exists(name) => ConceptName(name),
    _ => FreeText,
  }

[step 2: 起点解決]
  DocPath => <500µs (SQLite point query)
  TaskId => <500µs
  ConceptName => <500µs
  FreeText => 5-15ms (tantivy search, top-3)

[step 3: BFS展開] <1ms (radius=2, <1000 node traversed)
  let start_nodes = resolve_to_node_indices(entry_points);
  let mut visitor = Bfs::new(&graph, start_nodes[0]);
  let mut collected = Vec::with_capacity(max_nodes);
  while let Some(nx) = visitor.next(&graph) {
      if graph[nx].distance > radius { break; }
      if collected.len() >= max_nodes { break; }
      collected.push(nx);
  }

[step 4: hydrate metadata] <3ms (1 SQL query per node, batched in single transaction)
  let ids: Vec<String> = collected.iter().map(|nx| graph[*nx].id()).collect();
  let docs = conn.query("SELECT * FROM docs WHERE path IN (...)", &ids)?;
  let staleness = compute_staleness(&docs, &scheduled_changes);

[step 5: reading order] <500µs (topological sort)
  collected.sort_by_key(|nx| doc_type_rank(&graph[*nx]));
  let ordered = toposort_within_rank(&collected, &graph);

[step 6: JSON serialize] <1ms
  serde_json::to_string(&response)

Total: 10-25ms typical
```

### 8.2 Query interpretation の微妙なケース

| 入力 | 解決 | 備考 |
|---|---|---|
| `Product` | ConceptName | `concepts.name = 'Product'` がヒット |
| `product` | FreeText | 小文字始まりでConceptNameのパターン外 |
| `/docs/api/...` | DocPath | プレフィックス判定 |
| `productCreate` | ConceptName | kebab-case/camelCaseどちらも許可 |
| `DraftOrderLineItem.grams` | ConceptName | `.`を含むフルパスも許可 |
| `build-discount-function` | TaskId | tasks テーブルマッチ |
| `discount function cart level` | FreeText | スペース複数 |
| `クーポン 重ね掛け` | FreeText | 非ASCII |

### 8.3 tantivy検索クエリ構築

```rust
fn build_query(text: &str, filters: &Filters) -> Box<dyn Query> {
    let schema = ...;
    let content_en = schema.get_field("content_en").unwrap();
    let content_ja = schema.get_field("content_ja").unwrap();
    let title = schema.get_field("title").unwrap();
    let related_concepts = schema.get_field("related_concepts").unwrap();

    let mut sub_queries: Vec<(Occur, Box<dyn Query>)> = vec![];

    // 本文を英日両方で検索(OR)
    let parser_en = QueryParser::for_index(&index, vec![content_en, title, related_concepts]);
    sub_queries.push((Occur::Should, parser_en.parse_query(text)?));

    let parser_ja = QueryParser::for_index(&index, vec![content_ja]);
    sub_queries.push((Occur::Should, parser_ja.parse_query(text)?));

    // フィルタ(version/surface/doc_type)
    if let Some(v) = &filters.version {
        let field = schema.get_field("version").unwrap();
        sub_queries.push((Occur::Must, Box::new(TermQuery::new(
            Term::from_field_text(field, v),
            IndexRecordOption::Basic,
        ))));
    }

    Box::new(BooleanQuery::new(sub_queries))
}
```

### 8.4 Stalenessの計算

```rust
fn compute_staleness(doc: &Doc, schedules: &[ScheduledChange]) -> StalenessInfo {
    let age = (now() - doc.last_verified).num_days() as u32;
    let freshness = match age {
        0..=7 => Freshness::Fresh,
        8..=30 => Freshness::Aging,
        _ => Freshness::Stale,
    };

    let deprecated_refs: Vec<String> = doc.deprecated_refs
        .as_ref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    let upcoming: Vec<_> = schedules.iter()
        .filter(|sc| deprecated_refs.iter().any(|dr| dr == &sc.type_name))
        .filter(|sc| sc.effective_date > now_date())
        .map(|sc| UpcomingChange {
            effective_date: sc.effective_date.clone(),
            change: sc.description.clone(),
            migration_hint: sc.migration_hint.clone(),
        })
        .collect();

    StalenessInfo {
        age_days: age,
        freshness,
        content_verified_at: doc.last_verified.clone(),
        schema_version: doc.version.clone(),
        references_deprecated: !deprecated_refs.is_empty(),
        deprecated_refs,
        upcoming_changes: upcoming,
    }
}
```

---

## 9. 並行性と一貫性

### 9.1 MCP request処理

MCPリクエストは基本的に**readのみ**。writeはbackground workerだけが行う。

- Readパス: `Arc<AppState>` をhandlerに共有。各handlerは`&AppState`経由で以下にアクセス
  - tantivy `IndexReader`: lock-free snapshot
  - SQLite read-only connection pool (r2d2): 複数reader並列
  - `ConceptGraph` via `Arc<ArcSwap<ConceptGraph>>`: wait-free read
- 原則: MCP handler内でロック取得しない。`arc_swap::ArcSwap::load`で現在のグラフのsnapshot取得するだけ

### 9.2 Backgroundの書き込み

- SQLiteは WAL mode で複数readerと1 writer並行可
- Tantivyは `IndexWriter` が単一mutable、reader は`reload()`で新segment取り込み
- In-memory graphは新しいバージョンを別途構築 → `ArcSwap::store` でatomic差し替え

```rust
// graph rebuild in background
async fn rebuild_graph(app: &AppState) -> Result<()> {
    // 1. SQLiteから新グラフ構築(数百ms)
    let new_graph = ConceptGraph::load_from_db(&app.pool).await?;

    // 2. msgpackスナップショット保存(起動時高速化のため)
    let snapshot = rmp_serde::to_vec(&new_graph)?;
    fs::write(app.graph_snapshot_path(), snapshot)?;

    // 3. atomic swap
    app.graph.store(Arc::new(new_graph));

    Ok(())
}
```

### 9.3 失敗時の振る舞い

- HTTP fetch失敗 → 既存データで継続、workerはnext intervalで再試行
- SQLite corruption → `shopify-rextant build --force` で再構築(ユーザ操作)
- Tantivy index corruption → 同上
- Graph snapshot読み込み失敗 → SQLiteから再構築(自動fallback)

---

## 10. CLI インターフェース

### 10.1 サブコマンド

```bash
shopify-rextant serve
    # MCP stdio server開始。起動時にbackground workersもspawn
    
shopify-rextant build [--force] [--limit N]
    # インデックス構築。--forceで既存削除、--limitで取り込み件数を制限
    
shopify-rextant refresh [PATH]
    # PATHが指定されればそのドキュメントだけconditional refetch
    # 未指定なら aging_sweeper を即時実行
    # v0.5.0: --url https://shopify.dev/docs/... で未収録docをオンデマンド取得

shopify-rextant coverage repair
    # v0.5.0: coverage_reports.status="failed" の許可済み公式docs/changelog URLを再試行
    
shopify-rextant status
    # shopify_status ツールと同じ情報をterminalに表示
    # coverage_reportがある場合は skipped/failed/last_sitemap_at を表示
    
shopify-rextant search QUERY [--version V] [--limit N]
    # tantivy検索結果を直接表示(デバッグ用)
    
shopify-rextant show PATH
    # 指定pathの生markdownをterminalに表示
    # --anchor でセクション切り出し
    
shopify-rextant version
    # バイナリバージョン、index schema version、データサイズ表示
```

Planned CLI, not currently implemented:

```bash
shopify-rextant graph --from CONCEPT [--radius 2] [--format mermaid|dot|json]
shopify-rextant changelog [--since 2026-04-01]
```

### 10.2 設定ファイル `~/.shopify-rextant/config.toml`

```toml
[index]
# どのバージョンを取り込むか
versions = ["2026-04", "2026-07"]
# 特定バージョンにpinする(省略時はlatest stable)
pinned_version = "2026-04"
# タスク抽出を有効にするか
enable_task_extraction = true
# sitemap.xmlから/docs/**を取り込むか。v0.1.1以降はtrue推奨
enable_sitemap_discovery = true
# v0.5.0。未収録shopify.dev/docs URLをfetch時に取得してindexへ追加するか
enable_on_demand_fetch = false

[workers]
version_watcher_interval_hours = 24
changelog_watcher_interval_minutes = 30
aging_sweeper_interval_hours = 6
edge_repairer_interval_hours = 72

[network]
user_agent = "shopify-rextant/<binary version>"
request_timeout_seconds = 30
# 並列fetch数
concurrency = 8

[output]
# shopify_mapのレスポンス1ノードあたりのsummary文字数
summary_max_chars = 400
# shopify_fetchのデフォルト上限
fetch_default_max_chars = 20000

[logging]
# trace | debug | info | warn | error
level = "info"
# stderr only (stdoutはMCPプロトコル)
stderr = true
file = true
file_path = "~/.shopify-rextant/logs/worker.log"

[tokenizer]
# 日本語検索を有効化(バイナリサイズ +5MB)
enable_japanese = true
```

---

## 11. 配布・インストール

### 11.1 配布経路

**現在のpre-release導入**: source checkout
```bash
cargo install --path .
```

または:

```bash
cargo build --release
./target/release/shopify-rextant version
```

**Planned Tier 1(推奨)**: crates.io
```bash
cargo install shopify-rextant
```

**Planned Tier 2**: Homebrew (macOS/Linux)
```bash
brew install shopify-rextant/tap/shopify-rextant
```

**Planned Tier 3**: 事前ビルド済みバイナリ
```bash
# GitHub Releasesから
curl -L https://github.com/<owner>/shopify-rextant/releases/latest/download/shopify-rextant-$(uname -s | tr A-Z a-z)-$(uname -m) -o ~/.local/bin/shopify-rextant
chmod +x ~/.local/bin/shopify-rextant
```

**Planned Tier 4**: NixOS flake
```nix
# flake.nix
{
  inputs.shopify-rextant.url = "github:<owner>/shopify-rextant";
  # ...
  home.packages = [ shopify-rextant.packages.${system}.default ];
}
```

### 11.2 初回セットアップ

```bash
# 1. pre-release source install
cargo install --path .

# 2. インデックス構築(2-5分)
shopify-rextant build

# 3. MCPクライアント設定(Claude Code例)
claude mcp add --transport stdio shopify-rextant -- shopify-rextant serve
```

### 11.3 クライアント設定例

**Claude Code**:
```bash
claude mcp add --transport stdio shopify-rextant -- shopify-rextant serve
```

**Cursor**:
```json
{
  "mcpServers": {
    "shopify-rextant": {
      "command": "shopify-rextant",
      "args": ["serve"]
    }
  }
}
```

**Codex CLI** (`~/.codex/config.toml`):
```toml
[mcp_servers.shopify-rextant]
command = "shopify-rextant"
args = ["serve"]
```

### 11.4 共有daemon runtime

`shopify-rextant serve` はMCP stdio entrypointのまま維持する。通常は軽量shimとして起動し、同一identityのlocal daemonへUnix domain socketで接続する。daemonは1つの warmed `ServerState`、Tantivy reader、Japanese tokenizer cache、background worker setを保持するため、global MCP登録とproject-level MCP登録が同じidentityならruntimeを共有できる。

互換性・調査用に direct mode を残す:

```bash
shopify-rextant serve --direct
```

Direct mode は現行の単一プロセスstdio serverとして動作し、newline-delimited JSON と Content-Length framing の両方を受け付ける。MCP transportの切り分け、daemonのstale socket調査、ベンチマークbaselineでは direct mode を使う。

Daemon identity は保守的に計算する:

- canonical `--home`
- package version
- index `SCHEMA_VERSION`
- `config.toml` hash

identity hashから `/tmp/shopify-rextant-daemons/*.sock`、`*.lock`、`*.pid` を作る。socket filenameはhash化してUnix socket path lengthを抑える。socketが応答しない、pidだけ残っている、lockが古い場合はshimが起動時にstale artifactとして回収する。stdoutはshimでもdaemonでもMCP JSON-RPC message専用で、診断ログはstderrに出す。

### 11.5 サイズ見積もり

| 項目 | 概算 |
|---|---|
| バイナリ | 15-25 MB (lindera IPADIC embedded) |
| バイナリ(日本語無効) | 8-12 MB |
| インデックス `~/.shopify-rextant/data/` | 150-400 MB |
| └─ `raw/` 原文markdown | 50-150 MB |
| └─ `tantivy/` 転置index | 50-150 MB |
| └─ `index.db` | 20-50 MB |
| └─ `graph.msgpack` | 2-10 MB |
| メモリ常駐 | 20-50 MB |
| 起動時間 | <20 ms (graph.msgpackあり) |
| `shopify_map` レイテンシ | 10-25 ms |
| `shopify_fetch` レイテンシ | 2-5 ms |

---

## 12. セキュリティ・プライバシ

### 12.1 送信データ

このツールが外部に送信するもの:
- `shopify.dev/**` への GET リクエスト (User-Agent = `shopify-rextant/<binary version>`)
- `shopify.dev/changelog/feed` への GET リクエスト
- v0.2.0以降、`shopify.dev/admin-graphql-direct-proxy/{version}` へのPOSTリクエスト。送信bodyは固定のGraphQL introspection queryのみ

**送信しないもの**:
- ユーザのコード
- ユーザのクエリ内容
- MCPクライアント情報
- その他一切のテレメトリ

公式AI Toolkitの`validate.mjs`とは対照的。商用・非公開プロジェクトでも安心して使える。

### 12.2 ネットワーク設定

- HTTPSのみ (rustls)
- HTTP proxyは`HTTPS_PROXY`環境変数で対応
- 完全オフライン動作: `config.toml`で`[workers]`の各intervalを`0`または十分大きい値にすれば、初回buildだけネット使う

### 12.3 ファイルシステム権限

- `~/.shopify-rextant/` はユーザ権限のみ(0700)
- ログに個人識別情報は書かない(pathとクエリ文字列のみ)

---

## 13. エラー設計

### 13.1 MCPツールエラー

MCPレスポンスのエラー構造(`JSON-RPC error` 準拠):

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "error": {
    "code": -32001,
    "message": "Index not built",
    "data": {
      "suggestion": "Run `shopify-rextant build` first",
      "recoverable": true
    }
  }
}
```

エラーコード割り当て(-32000番台をカスタム):

| Code | 意味 | Recoverable |
|---|---|---|
| -32001 | Index not built | Yes (run build) |
| -32002 | Path not found | No |
| -32003 | Version not indexed | Yes (run build or future version-specific build) |
| -32004 | Corrupted index | Yes (run build --force) |
| -32005 | Network unavailable for refresh | Yes (待機) |
| -32006 | Invalid query syntax | No |
| -32007 | On-demand fetch disabled | Yes (enable_on_demand_fetch=true or run build) |
| -32008 | URL outside allowed scope | No |
| -32009 | Anchor not found | Yes (retry without anchor or inspect sections) |

### 13.2 フォールバック戦略

- graph snapshot missing → SQLiteから再構築(自動、ユーザ通知)
- tantivy index missing → `shopify_map`でfree_textクエリのみ失敗、concept/doc/path解決はSQLiteベースで継続
- SQLite corrupted → `shopify_status` のwarningに表示、`shopify_map`は失敗しエラー返す
- on-demand fetch disabled → `shopify_fetch` はネットに出ず、候補URLと `enable_on_demand_fetch=false` を返す
- URL scope違反 → `shopify.dev/docs/**` / `shopify.dev/changelog/**` 以外は即時拒否し、HTTP requestを送らない

---

## 14. テスト戦略

### 14.1 Unit tests

- `classify(path)` の分類テーブル網羅
- `compute_staleness` のエッジケース(閾値境界)
- GraphQL introspection parser(小規模サンプルJSON)
- BFS + topological sort(人工グラフ)
- query interpretation の優先順位
- `canonical_doc_path(url)` の正規化(fragment/query/trailing slash/.md/.txt除去)
- markdown heading anchor抽出と、同階層以上のheadingまでのsection切り出し
- fenced code block除外(`include_code_blocks=false`)
- newline-delimited JSON と Content-Length のMCP framing互換

### 14.2 Integration tests

- 最小shopify.dev mock (10ページ程度)でフルbuildフロー
- MCP JSON-RPC の request/response (`rmcp` テストヘルパー)
- SQLiteとtantivyの一貫性(同じpathがFTS結果にあればdocsテーブルにも存在)
- atomic graph swap中にreadを走らせても壊れない(loom crateで並行性検証)
- `llms.txt` に無く `sitemap.xml` にだけ存在するdocsがindexされる
- raw markdownが無いURLは `coverage_reports.status="skipped"` として残り、`shopify_status.coverage` に反映される
- `shopify_map` は同一pathを重複nodeとして返さない
- `shopify_fetch(url=...)` は `[index].enable_on_demand_fetch=false` なら `-32007` を返し、許可外host/pathにはHTTP requestを送らず `-32008` を返す
- `shopify_fetch(url=...)` / 未収録canonical path は有効化時にraw保存、`docs.source="on_demand"` upsert、Tantivy差分更新まで完了する
- `shopify_map` は0件時に許可済みURL/pathだけ `on_demand_candidate` を返し、fetchはしない
- `shopify-rextant coverage repair` は同じon-demand policyで failed coverage row を再試行する

### 14.3 E2E tests

- GitHub Actions CI gate runs `cargo fmt --check`, `cargo test`,
  `cargo bench --bench release_contract -- --test`, `cargo package --list`,
  `cargo package`, `cargo build --release`, and newline-delimited MCP `initialize`
  smoke before release branching/tagging.
- `cargo run -- serve` + 実Claude Code起動(手動)
- `cargo run -- build` → `shopify_map` → `shopify_fetch` の往復が成功
- Codex MCP stdioで `initialize` → `tools/list` → `tools/call(shopify_status)` が成功し、初回応答P50 <20ms
- `[index].enable_on_demand_fetch=true` の一時homeで、実Shopify docs URLの `shopify_fetch(url)` がraw markdownを取得し、直後の `search` で `source="on_demand"` として見つかる
- `optional scopes` / `manage access scopes` 系クエリが `shopify_map` から公式docs pathへ到達できる
- `shopify_fetch(path, anchor)` で該当セクションだけが返り、`sections` に見出し一覧が含まれる

### 14.4 性能ベンチマーク

`criterion` crateで以下を計測:
- `shopify_map` on typical queries (P50, P99)
- `shopify_fetch` on large docs
- graph swap latency
- BFS traversal with varying graph size
- MCP initialize round-trip with newline-delimited JSON framing

Implemented release-contract benchmark:

```bash
cargo bench --bench release_contract
```

The benchmark builds a deterministic local fixture once and measures `status`,
`search_docs("Product")`, `shopify_map("Product")`, and `shopify_fetch(Product path)`
without live network access. It is intended as the permanent release gate until the
codebase is split into a library crate and lower-level graph/search microbenchmarks can
call public APIs directly.

### 14.5 回帰保護

- shopify.devのスナップショット(tarball、CI resourceにcommit)を使った再現可能なbuild
- 特定のmap クエリに対する期待される返却ノード集合(フィクスチャ)
- coverage fixture: `llms.txt`に無いが`sitemap.xml`にあるURLを含める
- transport fixture: newline-delimited JSON client と Content-Length client の両方を流す

---

## 15. 実装ロードマップ

### v0.1 (Week 1, ~12時間)
- [x] 設計ドキュメント完成
- [x] Cargoプロジェクト初期化、`rmcp` / `rusqlite` / `tantivy` 依存追加
- [x] SQLite schema定義、migration 1
- [x] shopify.dev llms.txt fetch + raw markdown 保存
- [x] docs テーブル + tantivy index への投入
- [x] MCP `shopify_fetch` 実装
- [x] MCP `shopify_map` 実装(FTS検索のみ、conceptグラフなし)
- [x] CLI `build` / `serve` / `status` / `search` / `show`
- [x] MCP stdio newline-delimited JSON framing対応(rmcp互換)

**動作目標**: Claude Code から `shopify-rextant` で検索と読み取りができる。公式Dev MCPより速い。

### v0.1.1 (Week 1.5, ~4時間)
- [x] sitemap discoveryを実装し、`llms.txt` + `sitemap.xml` のunionでdocsを取り込む
- [x] `coverage_report` を保存し、`shopify_status` に skipped/failed/last_sitemap_at を表示
- [x] `shopify_map` のnodesを `path` でdedupeする
- [x] `shopify_map.meta.graph_available=false` と `query_interpretation` を返し、v0.1系がFTS候補であることを明示する
- [x] `shopify_fetch` / CLI `show` の `anchor` セクション切り出しに対応
- [x] `include_code_blocks=false` のcode block除外に対応
- [x] `/docs/api/admin-graphql`、`/docs/api/storefront` などルートAPIページの `api_surface` 分類を修正
- [x] MCP接続E2EをCI fixture化し、initialize応答P50 <20msを回帰保護する

**動作目標**: `optional_scopes` のような設定系docsをweb searchなしで発見できる。v0.1系のレスポンスが「本物のgraph」と誤解されない。

### v0.1.2 (Test hardening, ~2-3時間)
- [x] `build_index` をテスト用source URL注入可能な内部構造へ分離する(public CLI/MCP契約は変更しない)
- [x] 最小shopify.dev mock fixtureで `llms.txt` + `sitemap.xml` union のfull buildを検証する
- [x] raw markdownが無いURLを `coverage_reports.status="skipped"` として保存し、`shopify_status.coverage` に反映されることを検証する
- [x] `shopify_map` の `meta.graph_available=false` / `query_interpretation` / zero-result query_plan をcontract test化する
- [x] `shopify_fetch(path, anchor)` の `sections` / `truncated` / `include_code_blocks=false` をresponse-levelで検証する
- [x] Content-Length framed MCP requestのtransport fixtureを追加する

**動作目標**: v0.1.1のsitemap discovery、coverage reporting、FTS map contract、fetch section extraction、MCP transport互換性をネットワーク非依存の `cargo test` で回帰保護できる。

### v0.2.0 (implemented, Graph map foundation)

**目的**: v0.1系のFTS候補リストを、Admin GraphQLのconcept/doc graphに昇格する。`shopify_map` が「本物の地図」を返す最初のリリースにする。

**解かないこと**:
- changelog watcher / scheduled_changes / deprecation freshness hydration (v0.3)
- query_log由来のedge_repairerとmissing edge学習 (v0.4)
- URL指定on-demand fetch / coverage repair command (v0.5)
- GraphQL/Liquidコード検証、実店舗操作、LLM要約・回答合成 (scope out / never)

**制約**:
- Graph sourceは公式 `shopify.dev/admin-graphql-direct-proxy/{version}` のintrospectionに限定する。認証済みストアのAdmin API endpointは叩かない
- local-first / zero telemetry / no synthesis を維持する。ユーザコード、クエリ、MCP client情報を外部送信しない
- v0.1.1の `shopify_fetch`、`shopify_status.coverage`、`shopify_map.meta.query_interpretation` は後方互換を維持する
- `meta.graph_available=true` はconcept/edge graphが構築済みの時だけ返す。FTS fallbackのみならfalseのままにする

**最低限**:
- [x] Admin GraphQL direct proxyへintrospection queryを送り、バージョン別schema snapshotを `data/schemas/admin-graphql/{version}.introspection.json` に保存する
- [x] `concepts` / `edges` / `tasks` テーブルを現行SQLite migrationへ追加し、`SCHEMA_VERSION` をv0.2.0用に更新する
- [x] Object/InputObject/Interface/Union/Enum/Scalar/Field/InputField/EnumValueをconcept化し、`defined_in` / `has_field` / `returns` / `accepts_input` / `implements` / `member_of` edgeを作る
- [x] docs階層から `parent_of` / `next` / `prev` edge、Markdown link/related sectionから `see_also` edge、GraphQL code blockから `references_type` edgeを抽出する
- [x] SQLiteからgraph representationを構築し、`data/graph.msgpack` にsnapshot保存する
- [x] `shopify_map` が `concept_name` / `doc_path` / `task_name` / `free_text` を既存ルールで解釈し、concept/doc/task graph上のBFS結果を返す
- [x] `suggested_reading_order` はdoc_type rank + concept依存順の決定的ソートで返す
- [x] graphが空またはschema未取得なら、エラーではなく `graph_available=false` とcoverage/graph warningを返してv0.1.1相当のFTS導線に落とす

**実装ソース**:
- `src/main.rs` の build pipeline は、Admin GraphQL introspection取得、schema snapshot保存、concept/edge投入、graph snapshot保存、changelog pollingまでを実行する
- `src/main.rs` の `shopify_map` はconcept/doc/free textをgraph entry pointへ解釈し、graphが使えない時はFTS fallbackとwarningを返す
- `src/main.rs` のテストは、Product concept map、doc pathからconcept到達、free text昇格、schemaなしfallback、返却node内edge closureをfixtureで検証する

**検証基準**:
- WHEN mock direct proxyがProduct/ProductVariant/ProductInputを含むintrospection JSONを返す THEN `build_index_from_sources` はconcepts/edgesを保存し、`status` に `graph.concept_count > 0` と `graph.edge_count > 0` を返す
- WHEN `shopify_map({"from":"Product","version":"2026-04"})` を呼ぶ THEN `center.kind="concept"`、`meta.graph_available=true`、`meta.query_interpretation.resolved_as="concept_name"`、`edges` が非空、`suggested_reading_order` にProduct reference doc pathが含まれる
- WHEN `shopify_map({"from":"/docs/api/admin-graphql/2026-04/objects/Product"})` を呼ぶ THEN doc nodeから `defined_in` / `references_type` edgeを通じてProduct conceptへ到達できる
- WHEN `shopify_map({"from":"discount function cart level"})` を呼ぶ THEN tantivy top docをentry pointにし、昇格できたconcept/doc graphを返す。昇格できない候補もdoc nodeとして失わない
- WHEN schema snapshotが無い、壊れている、またはedge数0 THEN `shopify_map` はpanicせず `graph_available=false` と `query_plan[0].action="inspect_status"` を返す
- WHEN graph snapshotが存在する THEN `serve` 起動時にSQLite full rebuildを避け、`initialize` → `tools/list` → `tools/call(shopify_status)` のP50 <20msを維持する

**動作目標**: `shopify_map("Product")` でAdmin GraphQLの関連型・関連guide・読む順を含む地図が返る。

### v0.3.0 (Freshness and changelog impact, Week 3, ~10時間)

**目的**: changelogとindex鮮度をsource mapに接続し、エージェントが古い・deprecated・近く壊れる可能性のあるdocsを誤って信頼しないようにする。

**解かないこと**:
- changelog本文だけをSSoTにした影響範囲確定
- schema diffによるversion間の完全なbreaking change検出
- query_log由来のedge_repairerとmissing edge学習 (v0.4)
- URL指定on-demand fetch / coverage repair command (v0.5)
- GraphQL/Liquidコード検証、実店舗操作、LLM要約・回答合成 (scope out / never)

**制約**:
- changelog impactのSSoTは `concepts` / `docs` / `edges` とする。RSS本文から抽出した候補は、index上で解決できたものだけを採用する
- SSoTに解決できない候補は `unresolved_affected_refs` として保持し、`references_deprecated` / `upcoming_changes` には反映しない
- version watcherのSSoTはschema/doc indexとする。HTMLだけをversion sourceにしない
- local-first / zero telemetry / no synthesis を維持する。ユーザコード、クエリ、MCP client情報を外部送信しない
- `refresh` はPATH指定時の単一doc更新と、PATH未指定時のaging/stale sweepを分ける。full rebuildは `build` の責務に残す

**最低限**:
- [x] changelog RSS watcher
- [x] changelog title/body/link/categoriesから影響候補を抽出し、`concepts` / `docs` / `edges` で解決できた候補だけを `affected_types` として保存する
- [x] 未解決候補を `unresolved_affected_refs` として保存し、deprecated/staleness判定から除外する
- [x] scheduled_changes 抽出(正規表現ベース、ただしtype_nameは解決済みconcept/docに限定)
- [x] references_deprecated フラグ付与
- [x] upcoming_changes のstaleness埋め込み
- [x] aging_sweeper
- [x] version_watcher
- [x] `shopify_status` ツール + CLI `status`

**検証基準**:
- WHEN changelog feed fixture contains `DraftOrderLineItem.grams field removed in 2026-07` AND `concepts` contains `admin_graphql.2026-04.DraftOrderLineItem.grams` THEN `scheduled_changes` stores the removal and docs connected to that concept return `staleness.references_deprecated=true`
- WHEN changelog text mentions an unknown symbol that is absent from `concepts` and `docs` THEN it is stored in `unresolved_affected_refs` and no doc is marked `references_deprecated`
- WHEN a changelog entry links to an indexed docs path THEN the watcher resolves the path through `docs.path`, expands impact through `edges`, and records affected docs/concepts without relying on changelog prose alone
- WHEN `shopify_map` returns a doc or concept affected by a scheduled change THEN the node `staleness.upcoming_changes` includes `effective_date`, `change`, and optional `migration_hint`
- WHEN `shopify-rextant refresh` is run without PATH THEN only aging/stale docs are considered for refresh, and full index rebuild is not invoked
- WHEN `shopify_status` is called after workers run THEN it includes worker last-run timestamps, freshness distribution, and changelog polling warnings

**動作目標**: deprecation警告つきのmapが返る。古くなったインデックスが自動更新される。

### v0.4.0 (remaining continuous improvement)

**目的**: v0.2 graphとv0.5 on-demand fetchの運用ログを使い、検索・edge coverageの穴を継続的に小さくする。

**残対応**:
- [ ] edge_repairer (query_logから欠落エッジ検出)
- [x] 日本語tokenizer (lindera) 本格統合
- [ ] query_logから低ヒット率クエリを抽出し、検索ルール改善候補を生成
- [ ] map直後にfetchされたdocをmissing edge候補としてedge_repairerに投入

**検証基準**:
- WHEN low-hit query_log rows exist THEN diagnostics reports candidate rule/edge improvements without mutating docs automatically
- WHEN `shopify_map` is followed by `shopify_fetch` for a doc outside the returned graph THEN an idempotent missing-edge candidate is recorded
- WHEN an edge candidate has enough source evidence THEN edge_repairer inserts an evidence-backed edge; otherwise it remains a rejected/pending candidate

### v0.5.0 (implemented, on-demand recovery)
- [x] `shopify_fetch` / `refresh --url` のオンデマンドfetchを実装する
- [x] 未収録の `https://shopify.dev/docs/**` URLを受け取ったらraw取得、docs upsert、tantivy差分投入を行う
- [x] オンデマンドfetch対象を `shopify.dev/docs/**` と `shopify.dev/changelog/**` に制限する
- [x] `shopify_map` 検索0件時に、推定docs URL候補と `enable_on_demand_fetch` の状態を返す
- [x] coverage_reportの失敗URLを再試行する `shopify-rextant coverage repair` を追加
- [x] on-demandで追加されたdocを `source="on_demand"` として記録し、次回full buildでsitemap由来docと統合する

**動作目標**: indexに漏れた公式docsでも、エージェントがURLまたはpathを知っていればその場で回収できる。ただし任意URL fetcherにはしない。

### v1.0 (remaining public release readiness)

**目的**: ローカル利用の実装を、他の開発者が安全に導入・検証・配布できる公開品質へ引き上げる。

**残対応**:
- [x] `Cargo.toml` の package version と公開metadataをリリース対象に合わせる
- [x] `shopify-rextant version` / `--version` / HTTP User-Agent が公開バージョンと一致することを確認する
- [ ] pre-release source install手順からpublic install手順へREADMEを切り替える
- [x] CI release gate(fmt/test/bench compile/package/MCP initialize smoke)
- [ ] E2Eテスト(実Claude Code/Codex + on-demand実URL)、性能保証(P50/P99閾値)
- [ ] セキュリティレビュー
- [ ] Homebrew tap + GitHub Actions release
- [ ] NixOS flake
- [x] ベンチマーク + チューニング
- [x] ドキュメントサイト(README、CONTRIBUTING)
- [ ] Shopify developer コミュニティへの紹介記事(Zenn日本語 + dev.to英語)

**検証基準**:
- WHEN release CI runs THEN cargo tests, MCP fixture smoke, packaging checks, and security checks pass on supported platforms
- WHEN a release tag is cut THEN `Cargo.toml` package version, CLI `--version`, User-Agent, and release artifact names all refer to the same version
- WHEN a new user installs from a documented channel THEN `shopify-rextant build`, `serve`, `search`, and `show` work without reading project internals
- WHEN benchmark fixtures run THEN local search/fetch latency and index size stay within documented thresholds

### 明示未対応項目の振り分け

ユーザ調査・実MCP接続・`optional_scopes` 調査で出たが、当初SPECに明示されていなかったものは以下に集約する。

| 項目 | 対応バージョン | 理由 |
|---|---:|---|
| 正式名称 `shopify-rextant` への統一 | v0.1 | 既存表記の整合性。機能追加ではない |
| MCP stdio newline-delimited JSON framing | v0.1 | 接続不能/10s timeoutのブロッカー。既に最小実装済み |
| `llms.txt` に無いdocsを拾うsitemap discovery | v0.1.1 | 同じ「検索で見つからない」問題の軽量な根本対策 |
| `coverage_report` と `shopify_status.coverage` | v0.1.1 | sitemap discoveryと同時に入れないと漏れの可視化ができない |
| `shopify_map` のpath dedupe | v0.1.1 | FTS結果のUX修正。軽量 |
| v0.1系がgraphではなくFTS候補である明示 | v0.1.1 | レスポンスの誤読を防ぐ軽量な契約修正 |
| `anchor` / `include_code_blocks=false` | v0.1.1 | fetchの読み取り効率改善。既存raw markdown上で完結 |
| root API pageの分類修正 | v0.1.1 | sitemap拡張時に同時対応すべき分類バグ |
| initialize P50 <20ms のE2E回帰保護 | v0.1.1 | framing修正の再発防止。機能追加と同じタイミングでCI化 |
| 低ヒット率クエリから検索改善候補を作る | v0.4 | query_log運用が必要で、単発修正より継続改善寄り |
| map直後fetchをmissing edge候補にする | v0.4 | graph/edge_repairerと同じ改善サイクルに属する |
| URL指定のon-demand fetch | v0.5 | ネットワーク・DB upsert・tantivy差分投入・scope制限を伴うため新規機能として重い |
| coverage repair command | v0.5 | on-demand fetch基盤を再利用するため同じ新規機能にまとめる |
| GraphQL/Liquid validation | Scope out | 別MCP/兄弟プロジェクト向き。ローカルdocs mapの責務を超える |
| 実店舗操作・Admin API実行 | Scope out | safety/credential境界が別物 |
| リモート共有/チームサーバ | Scope out | local-first/zero telemetryと衝突するため配布改善で扱う |
| LLM要約・回答合成 | Never | No synthesis原則に反する |

---

## 16. 将来の拡張

### 16.1 他ドキュメントへの汎化

このアーキテクチャは`shopify.dev`特有のものではない。以下に複製可能:
- `emdash-docs-map`: Cloudflare EmDash CMS docs
- `cloudflare-docs-map`: Cloudflare Workers docs
- `stripe-docs-map`: Stripe API docs

共通ロジックを `docs-map-core` crateとして切り出し、ドメイン特化層を上にかぶせる構造に発展可能。

### 16.2 差分学習の強化

`query_log` を使った月次バッチで:
- ヒット率が低いクエリ → 検索ルール追加候補
- 返した直後に別のdocが呼ばれる → missing edge
- 特定doc+クエリで連続reading失敗 → doc内容の不足

これらを `skill/shopify-dev-workflow/SKILL.md` 等に反映する半自動パイプライン。

このうち「低ヒット率クエリ抽出」と「map直後fetchからmissing edge候補を作る」まではv0.4に前倒しする。skill更新やワークフロー自動生成は将来拡張のままにする。

### 16.3 IDE統合

- VS Code extension: エディタ内で現在の型名をホバーするとmap表示
- CLI completion: bash/zsh補完で型名候補提示
- `codemap.com` との連携: アプリ内の型使用箇所とmap結果をリンク

### 16.4 検証ツール分離

`shopify_validate` を独立MCPツールとして作る:
- GraphQL SDL + クエリ → 型チェック
- Liquid template validation
- Function entrypoint signature verification

これは本設計のスコープ外だが、同じindex.dbを共有して動く兄弟プロジェクトにできる。

---

## 17. 設計判断の記録(ADR 相当)

| # | 判断 | 選択 | 却下 | 理由 |
|---|---|---|---|---|
| 1 | 実装言語 | Rust | Node.js, Python | 並列性、tantivy利用、起動速度、単一バイナリ |
| 2 | 検索エンジン | tantivy | SQLite FTS5, Meilisearch | 起動<10ms、BM25、lindera対応 |
| 3 | メタデータDB | SQLite + rusqlite | RocksDB, DuckDB | トランザクション、ポータビリティ |
| 4 | レスポンス形式 | Graph map | Synthesized answer, search list | 情報歪みゼロ、エージェント自律性 |
| 5 | MCPツール数 | 3 | 5-6 | contextオーバーロード回避 |
| 6 | LLM利用 | 使わない | Haiku等でクエリ書換え | ローカル完結、決定性、コストゼロ |
| 7 | 配布形態 | 単一バイナリ | Node package, Docker image | 依存ゼロ、起動速度 |
| 8 | 日本語対応 | lindera (IPADIC embedded) | kuromoji, 無し | Shopify docs英語中心だが日本語クエリ発生、バイナリ+5MBで妥協 |
| 9 | Graph store | In-memory (petgraph) + msgpack snapshot | Neo4j, in-DB | 起動速度、外部プロセス不要 |
| 10 | Background worker | tokio task (same process) | OS cron, systemd timer | インストール容易、プロセス管理簡素 |
| 11 | staleness表現 | 構造データ(age/freshness/upcoming) | 文字列警告 | エージェントが判断可能 |
| 12 | Version pin | config.toml | MCP引数毎に指定 | Shopify APIのバージョンpin慣習に合致 |

---

## 18. 参考

- Shopify API versioning: https://shopify.dev/docs/api/usage/versioning
- Shopify llms.txt: https://shopify.dev/llms.txt
- Shopify changelog: https://shopify.dev/changelog (RSS available)
- GraphQL schema direct proxy: `https://shopify.dev/admin-graphql-direct-proxy/YYYY-MM`
- 公式Dev MCP (比較対象): https://github.com/Shopify/dev-mcp
- 公式AI Toolkit (比較対象): https://github.com/Shopify/Shopify-AI-Toolkit
- rmcp (Rust MCP SDK): https://github.com/modelcontextprotocol/rust-sdk
- tantivy: https://github.com/quickwit-oss/tantivy
- lindera-tantivy: https://docs.rs/lindera-tantivy
- petgraph: https://docs.rs/petgraph
- llms.txt standard: https://llmstxt.org/
- Diátaxis framework (doc_type分類): https://diataxis.fr/

---

## 19. 用語集

- **MCP**: Model Context Protocol. Anthropicが策定したAIアシスタントと外部ツールの通信規格
- **stdio transport**: stdin/stdout経由のJSON-RPC通信。ローカルMCPサーバの標準
- **tantivy**: Rust製の全文検索ライブラリ、Apache Luceneインスパイア
- **lindera**: Rust製の形態素解析ライブラリ(日本語/韓国語/中国語)
- **petgraph**: Rust製のグラフ処理ライブラリ
- **BFS**: 幅優先探索
- **Diátaxis**: ドキュメント4分類(Tutorial / How-to / Reference / Explanation)
- **Concept graph**: API型やエンティティ間の関係グラフ
- **Document graph**: ドキュメントページ間の階層・参照グラフ
- **Task graph**: 実装タスクと必要なconceptの対応グラフ
- **Staleness**: キャッシュされたデータの古さを示す指標
- **Conditional GET**: ETag/If-Modified-Since ヘッダを使った差分取得
- **ArcSwap**: Rustで、読みが支配的なデータのatomic置換を行うイディオム
- **WAL mode**: SQLiteのWrite-Ahead Logging。並行readerと1 writer可能

---

*End of design document.*
