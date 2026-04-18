mod markdown;
mod mcp_framing;
mod on_demand;
mod url_policy;
mod util;

use markdown::{
    MarkdownLink, SectionInfo, dedupe_links_by_path, extract_sections, parse_markdown_links,
    parse_sitemap_links, remove_fenced_code_blocks, section_content, title_from_markdown,
};
use url_policy::{
    canonical_doc_path, classify_api_surface, classify_content_class, classify_doc_type,
    extract_version, is_indexable_shopify_url, raw_doc_candidates, raw_path_for, reading_time_min,
};
use util::hash::hex_sha256;
use util::json::{doc_json_field, escape_query, merge_json_arrays, print_json, to_json_value};
use util::time::now_iso;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use feed_rs::parser;
use lindera::dictionary::load_dictionary;
use lindera::mode::Mode;
use lindera::segmenter::Segmenter;
use lindera_tantivy::tokenizer::LinderaTokenizer;
use mcp_framing::{read_message as read_mcp_message, write_json as write_mcp_message};
use on_demand::{
    FetchCandidate as OnDemandFetchCandidate, FetchPolicy as OnDemandFetchPolicy,
    is_allowed_path as is_on_demand_allowed_path,
};
use regex::Regex;
use reqwest::StatusCode;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{
    Field, IndexRecordOption, STORED, STRING, Schema, TEXT, TantivyDocument, TextFieldIndexing,
    TextOptions,
};
use tantivy::{Document, Index, Term, doc};

const SHOPIFY_LLMS_URL: &str = "https://shopify.dev/llms.txt";
const SHOPIFY_SITEMAP_URL: &str = "https://shopify.dev/sitemap.xml";
const SHOPIFY_CHANGELOG_FEED_URL: &str = "https://shopify.dev/changelog/feed.xml";
const SHOPIFY_VERSIONING_URL: &str = "https://shopify.dev/docs/api/usage/versioning";
const USER_AGENT: &str = concat!("shopify-rextant/", env!("CARGO_PKG_VERSION"));
const SCHEMA_VERSION: &str = "3";
const ADMIN_GRAPHQL_INTROSPECTION_QUERY: &str = r#"
query ShopifyRextantIntrospection {
  __schema {
    types {
      kind
      name
      description
      fields(includeDeprecated: true) {
        name
        description
        args {
          name
          description
          type { kind name ofType { kind name ofType { kind name ofType { kind name } } } }
          defaultValue
        }
        type { kind name ofType { kind name ofType { kind name ofType { kind name } } } }
        isDeprecated
        deprecationReason
      }
      inputFields {
        name
        description
        type { kind name ofType { kind name ofType { kind name ofType { kind name } } } }
        defaultValue
        isDeprecated
        deprecationReason
      }
      interfaces { kind name }
      enumValues(includeDeprecated: true) {
        name
        description
        isDeprecated
        deprecationReason
      }
      possibleTypes { kind name }
    }
  }
}
"#;

#[derive(Debug, Parser)]
#[command(name = "shopify-rextant")]
#[command(version)]
#[command(about = "Local Shopify docs map MCP server")]
struct Cli {
    #[arg(long, env = "SHOPIFY_REXTANT_HOME")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve {
        #[arg(long)]
        direct: bool,
    },
    #[command(hide = true)]
    Daemon {
        #[arg(long, default_value_t = 600)]
        idle_timeout_secs: u64,
    },
    Build {
        #[arg(long)]
        force: bool,
        #[arg(long)]
        limit: Option<usize>,
    },
    Refresh {
        path: Option<String>,
        #[arg(long)]
        url: Option<String>,
    },
    Coverage {
        #[command(subcommand)]
        command: CoverageCommand,
    },
    Status,
    Search {
        query: String,
        #[arg(long)]
        version: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    Show {
        path: String,
        #[arg(long)]
        anchor: Option<String>,
        #[arg(long, default_value_t = true)]
        include_code_blocks: bool,
        #[arg(long)]
        max_chars: Option<usize>,
    },
    Version,
}

#[derive(Debug, Subcommand)]
enum CoverageCommand {
    Repair,
}

#[derive(Debug, Clone)]
pub(crate) struct Paths {
    home: PathBuf,
    data: PathBuf,
    raw: PathBuf,
    tantivy: PathBuf,
    db: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DocRecord {
    path: String,
    title: String,
    url: String,
    version: Option<String>,
    doc_type: String,
    api_surface: Option<String>,
    content_class: String,
    content_sha: String,
    last_verified: String,
    last_changed: String,
    freshness: String,
    references_deprecated: bool,
    deprecated_refs: Vec<String>,
    summary_raw: String,
    reading_time_min: Option<i64>,
    raw_path: String,
    source: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct StatusResponse {
    schema_version: String,
    data_dir: String,
    index_built: bool,
    doc_count: i64,
    last_full_build: Option<String>,
    index: GraphIndexStatus,
    coverage: CoverageStatus,
    freshness: FreshnessStatus,
    workers: WorkerStatus,
    changelog: ChangelogStatus,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct GraphIndexStatus {
    concept_count: i64,
    edge_count: i64,
    graph_snapshot: bool,
}

#[derive(Debug, Clone, Serialize)]
struct CoverageStatus {
    last_sitemap_at: Option<String>,
    discovered_count: i64,
    indexed_count: i64,
    skipped_count: i64,
    failed_count: i64,
    classified_unknown_count: i64,
    sources: CoverageSources,
}

#[derive(Debug, Clone, Serialize)]
struct CoverageSources {
    llms: i64,
    sitemap: i64,
    on_demand: i64,
    manual: i64,
}

#[derive(Debug, Clone, Serialize)]
struct FreshnessStatus {
    fresh_count: i64,
    aging_count: i64,
    stale_count: i64,
}

#[derive(Debug, Clone, Serialize)]
struct WorkerStatus {
    last_changelog_at: Option<String>,
    last_aging_sweep_at: Option<String>,
    last_version_check_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ChangelogStatus {
    entry_count: i64,
    scheduled_change_count: i64,
    unresolved_ref_count: i64,
    last_warning: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FetchResponse {
    path: String,
    title: String,
    url: String,
    source_url: String,
    content: String,
    sections: Vec<SectionInfo>,
    truncated: bool,
    staleness: Staleness,
}

#[derive(Debug, Serialize)]
pub(crate) struct MapResponse {
    center: MapCenter,
    nodes: Vec<MapNode>,
    edges: Vec<Value>,
    suggested_reading_order: Vec<String>,
    query_plan: Vec<QueryPlanStep>,
    index_status: StatusResponse,
    meta: MapMeta,
}

#[derive(Debug, Serialize)]
struct QueryPlanStep {
    step: usize,
    action: String,
    path: Option<String>,
    reason: String,
}

#[derive(Debug, Serialize)]
struct MapMeta {
    generated_at: String,
    index_age_days: i64,
    versions_available: Vec<String>,
    version_used: String,
    coverage_warning: Option<String>,
    graph_available: bool,
    index_status: MapIndexStatus,
    on_demand_candidate: Option<OnDemandCandidate>,
    query_interpretation: QueryInterpretation,
}

#[derive(Debug, Serialize)]
struct MapIndexStatus {
    doc_count: i64,
    skipped_count: i64,
    failed_count: i64,
}

#[derive(Debug, Serialize)]
struct OnDemandCandidate {
    url: String,
    enabled: bool,
    reason: String,
}

#[derive(Debug, Serialize)]
struct QueryInterpretation {
    resolved_as: String,
    entry_points: Vec<String>,
    confidence: String,
}

#[derive(Debug, Serialize)]
struct MapCenter {
    id: String,
    kind: String,
    path: Option<String>,
    title: String,
}

#[derive(Debug, Serialize)]
struct MapNode {
    id: String,
    kind: String,
    subkind: String,
    path: String,
    title: String,
    summary_from_source: String,
    version: Option<String>,
    api_surface: Option<String>,
    doc_type: String,
    reading_time_min: Option<i64>,
    staleness: Staleness,
    distance_from_center: usize,
}

#[derive(Debug, Serialize)]
struct Staleness {
    age_days: i64,
    freshness: String,
    content_verified_at: String,
    schema_version: Option<String>,
    references_deprecated: bool,
    deprecated_refs: Vec<String>,
    upcoming_changes: Vec<Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MapArgs {
    pub(crate) from: String,
    pub(crate) radius: Option<u8>,
    pub(crate) lens: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) max_nodes: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FetchArgs {
    pub(crate) path: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) anchor: Option<String>,
    pub(crate) include_code_blocks: Option<bool>,
    pub(crate) max_chars: Option<usize>,
}

#[derive(Debug, Clone)]
struct AppConfig {
    index: IndexConfig,
}

#[derive(Debug, Clone)]
struct IndexConfig {
    enable_on_demand_fetch: bool,
}

#[derive(Debug)]
pub(crate) enum ToolError {
    Rpc {
        code: i64,
        message: String,
        data: Value,
    },
    Internal(anyhow::Error),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc { message, .. } => write!(f, "{message}"),
            Self::Internal(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ToolError {}

impl From<anyhow::Error> for ToolError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

impl ToolError {
    fn json_rpc_error(&self) -> Value {
        match self {
            Self::Rpc {
                code,
                message,
                data,
            } => json_rpc_error(*code, message, data.clone()),
            Self::Internal(error) => json_rpc_error(-32000, &error.to_string(), Value::Null),
        }
    }

    fn disabled(candidate: &OnDemandFetchCandidate) -> Self {
        Self::Rpc {
            code: -32007,
            message: "On-demand fetch is disabled".to_string(),
            data: json!({
                "candidate_url": candidate.source_url,
                "canonical_path": candidate.canonical_path,
                "enable_on_demand_fetch": false,
            }),
        }
    }

    fn outside_scope(input: &str) -> Self {
        Self::Rpc {
            code: -32008,
            message: "URL outside allowed Shopify docs scope".to_string(),
            data: json!({
                "input": input,
                "allowed_scope": [
                    "https://shopify.dev/docs/**",
                    "https://shopify.dev/changelog/**"
                ],
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
struct SearchArgs {
    query: String,
    version: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct IndexSourceUrls {
    llms: String,
    sitemap: String,
    changelog: String,
    versioning: String,
}

impl Default for IndexSourceUrls {
    fn default() -> Self {
        Self {
            llms: SHOPIFY_LLMS_URL.to_string(),
            sitemap: SHOPIFY_SITEMAP_URL.to_string(),
            changelog: SHOPIFY_CHANGELOG_FEED_URL.to_string(),
            versioning: SHOPIFY_VERSIONING_URL.to_string(),
        }
    }
}

pub(crate) trait TextSource {
    async fn fetch_text(&self, url: &str) -> std::result::Result<String, SourceFetchError>;

    async fn fetch_admin_graphql_introspection(
        &self,
        url: &str,
    ) -> std::result::Result<String, SourceFetchError> {
        self.fetch_text(url).await
    }
}

struct ReqwestTextSource {
    client: reqwest::Client,
}

impl ReqwestTextSource {
    fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder().user_agent(USER_AGENT).build()?,
        })
    }
}

impl TextSource for ReqwestTextSource {
    async fn fetch_text(&self, url: &str) -> std::result::Result<String, SourceFetchError> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| SourceFetchError {
                status: "failed".to_string(),
                reason: format!("network_error: {e}"),
                http_status: None,
            })?;
        let status = response.status();
        if status == StatusCode::OK {
            return response.text().await.map_err(|e| SourceFetchError {
                status: "failed".to_string(),
                reason: format!("network_error: {e}"),
                http_status: Some(StatusCode::OK.as_u16()),
            });
        }
        Err(SourceFetchError {
            status: "skipped".to_string(),
            reason: format!("http_status: {status}"),
            http_status: Some(status.as_u16()),
        })
    }

    async fn fetch_admin_graphql_introspection(
        &self,
        url: &str,
    ) -> std::result::Result<String, SourceFetchError> {
        let response = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .body(
                serde_json::to_vec(&json!({
                    "query": ADMIN_GRAPHQL_INTROSPECTION_QUERY
                }))
                .map_err(|e| SourceFetchError {
                    status: "failed".to_string(),
                    reason: format!("serialize_introspection_query: {e}"),
                    http_status: None,
                })?,
            )
            .send()
            .await
            .map_err(|e| SourceFetchError {
                status: "failed".to_string(),
                reason: format!("network_error: {e}"),
                http_status: None,
            })?;
        let status = response.status();
        if status == StatusCode::OK {
            return response.text().await.map_err(|e| SourceFetchError {
                status: "failed".to_string(),
                reason: format!("network_error: {e}"),
                http_status: Some(StatusCode::OK.as_u16()),
            });
        }
        Err(SourceFetchError {
            status: "skipped".to_string(),
            reason: format!("http_status: {status}"),
            http_status: Some(status.as_u16()),
        })
    }
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::new(cli.home)?;

    match cli.command {
        Command::Serve { direct } => {
            if direct {
                serve_direct(paths).await
            } else {
                serve(paths).await
            }
        }
        Command::Daemon { idle_timeout_secs } => {
            run_daemon(paths, Duration::from_secs(idle_timeout_secs)).await
        }
        Command::Build { force, limit } => build_index(&paths, force, limit).await,
        Command::Refresh { path, url } => refresh(&paths, path, url).await,
        Command::Coverage {
            command: CoverageCommand::Repair,
        } => print_json(&coverage_repair(&paths).await?),
        Command::Status => print_json(&status(&paths)?),
        Command::Search {
            query,
            version,
            limit,
        } => print_json(&search_docs(&paths, &query, version.as_deref(), limit)?),
        Command::Show {
            path,
            anchor,
            include_code_blocks,
            max_chars,
        } => {
            let response = shopify_fetch(
                &paths,
                &FetchArgs {
                    path: Some(path),
                    url: None,
                    anchor,
                    include_code_blocks: Some(include_code_blocks),
                    max_chars,
                },
            )
            .await?;
            println!("{}", response.content);
            Ok(())
        }
        Command::Version => {
            println!("shopify-rextant {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

impl Paths {
    pub(crate) fn new(home: Option<PathBuf>) -> Result<Self> {
        let home = match home {
            Some(path) => path,
            None => dirs::home_dir()
                .ok_or_else(|| anyhow!("could not resolve home directory"))?
                .join(".shopify-rextant"),
        };
        let data = home.join("data");
        Ok(Self {
            home,
            raw: data.join("raw"),
            tantivy: data.join("tantivy"),
            db: data.join("index.db"),
            data,
        })
    }

    fn raw_file(&self, raw_path: &str) -> PathBuf {
        self.raw.join(raw_path)
    }

    fn config_file(&self) -> PathBuf {
        self.home.join("config.toml")
    }
}

fn load_config(paths: &Paths) -> Result<AppConfig> {
    let raw = match fs::read_to_string(paths.config_file()) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let mut section = String::new();
    let mut enable_on_demand_fetch = false;
    for line in raw.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line.trim_matches(['[', ']']).trim().to_string();
            continue;
        }
        if section == "index" {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key.trim() == "enable_on_demand_fetch" {
                enable_on_demand_fetch = match value.trim() {
                    "true" => true,
                    "false" => false,
                    other => bail!("invalid enable_on_demand_fetch value: {other}"),
                };
            }
        }
    }
    Ok(AppConfig {
        index: IndexConfig {
            enable_on_demand_fetch,
        },
    })
}

impl OnDemandFetchPolicy {
    fn from_config(config: &AppConfig) -> Self {
        Self::new(config.index.enable_on_demand_fetch)
    }
}

fn ensure_on_demand_enabled(
    policy: &OnDemandFetchPolicy,
    candidate: &OnDemandFetchCandidate,
) -> std::result::Result<(), ToolError> {
    if policy.is_enabled() {
        Ok(())
    } else {
        Err(ToolError::disabled(candidate))
    }
}

async fn build_index(paths: &Paths, force: bool, limit: Option<usize>) -> Result<()> {
    let source = ReqwestTextSource::new()?;
    build_index_from_sources(paths, force, limit, &IndexSourceUrls::default(), &source).await
}

pub(crate) async fn build_index_from_sources<S: TextSource>(
    paths: &Paths,
    force: bool,
    limit: Option<usize>,
    source_urls: &IndexSourceUrls,
    source: &S,
) -> Result<()> {
    if force && paths.data.exists() {
        fs::remove_dir_all(&paths.data)
            .with_context(|| format!("remove {}", paths.data.display()))?;
    }
    fs::create_dir_all(&paths.raw)?;
    fs::create_dir_all(&paths.tantivy)?;

    let conn = open_db(paths)?;
    init_db(&conn)?;
    let schema = search_schema();
    let index = create_or_reset_index(paths, schema.clone(), force)?;
    register_japanese_tokenizer(&index)?;
    let mut writer = index.writer(50_000_000)?;
    writer.delete_all_documents()?;

    let llms = fetch_required_text(source, &source_urls.llms).await?;
    let mut docs = vec![SourceDoc {
        url: source_urls.llms.clone(),
        title_hint: Some("Shopify Developer Platform".to_string()),
        content: llms.clone(),
        source: "llms".to_string(),
    }];

    let sitemap = fetch_required_text(source, &source_urls.sitemap).await?;
    let mut links = parse_markdown_links(&llms);
    links.extend(parse_sitemap_links(&sitemap));
    let selected_links = dedupe_links_by_path(links)
        .into_iter()
        .filter(|link| is_indexable_shopify_url(&link.url))
        .take(limit.unwrap_or(usize::MAX));

    let mut coverage_events = Vec::new();
    for link in selected_links {
        match fetch_source_doc(source, &link).await {
            Ok(source) => {
                coverage_events.push(CoverageEvent::indexed(&link));
                docs.push(source);
            }
            Err(error) => coverage_events.push(CoverageEvent::from_fetch_error(&link, error)),
        }
    }

    let graph_build = build_admin_graphql_graph(paths, &docs, source).await?;
    let fields = SearchFields::from_schema(&schema)?;
    let tx = conn.unchecked_transaction()?;
    clear_coverage_reports(&tx)?;
    clear_graph_tables(&tx)?;
    let mut indexed_records = Vec::new();
    for source in &docs {
        let record = store_source_doc(paths, source)?;
        upsert_doc(&tx, &record)?;
        add_tantivy_doc(&mut writer, fields, &record, &source.content)?;
        indexed_records.push(record);
    }
    for event in coverage_events {
        insert_coverage_event(&tx, &event)?;
    }
    for concept in &graph_build.concepts {
        insert_concept(&tx, concept)?;
    }
    for edge in &graph_build.edges {
        insert_edge(&tx, edge)?;
    }
    refresh_indexed_versions(&tx, &indexed_records)?;
    tx.execute(
        "INSERT INTO schema_meta(key, value) VALUES('last_sitemap_at', ?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![now_iso()],
    )?;
    tx.execute(
        "INSERT INTO schema_meta(key, value) VALUES('last_full_build', ?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![now_iso()],
    )?;
    tx.commit()?;
    writer.commit()?;
    if !graph_build.concepts.is_empty() && !graph_build.edges.is_empty() {
        persist_graph_snapshot(paths, &graph_build)?;
    }
    let changelog_report =
        poll_changelog_from_source(paths, &source_urls.changelog, source).await?;
    if changelog_report.entries_seen > 0 || !changelog_report.warnings.is_empty() {
        let conn = open_db(paths)?;
        set_meta(
            &conn,
            "last_changelog_entries_seen",
            &changelog_report.entries_seen.to_string(),
        )?;
        set_meta(
            &conn,
            "last_changelog_warning_count",
            &changelog_report.warnings.len().to_string(),
        )?;
    }
    Ok(())
}

async fn refresh(paths: &Paths, path: Option<String>, url: Option<String>) -> Result<()> {
    let source = ReqwestTextSource::new()?;
    match (path, url) {
        (Some(_), Some(_)) => bail!("refresh accepts either PATH or --url, not both"),
        (Some(path), None) => refresh_doc_from_source(paths, &path, &source).await,
        (None, Some(url)) => refresh_url_from_source(paths, &url, &source).await,
        (None, None) => refresh_stale_docs_from_source(paths, &source).await,
    }
}

async fn refresh_url_from_source<S: TextSource>(
    paths: &Paths,
    url: &str,
    source: &S,
) -> Result<()> {
    on_demand_fetch_from_input(paths, url, source)
        .await
        .map(|_| ())
        .map_err(|error| anyhow!(error.to_string()))
}

#[derive(Debug, Serialize)]
struct CoverageRepairSummary {
    attempted: usize,
    repaired: usize,
    still_failed: usize,
    skipped_policy: usize,
    skipped_disabled: usize,
}

#[derive(Debug)]
struct CoverageRepairRow {
    id: i64,
    source_url: String,
}

async fn coverage_repair(paths: &Paths) -> Result<CoverageRepairSummary> {
    let source = ReqwestTextSource::new()?;
    coverage_repair_from_source(paths, &source).await
}

async fn coverage_repair_from_source<S: TextSource>(
    paths: &Paths,
    source: &S,
) -> Result<CoverageRepairSummary> {
    let conn = open_db(paths)?;
    init_db(&conn)?;
    let rows = failed_coverage_rows(&conn)?;
    let config = load_config(paths)?;
    let policy = OnDemandFetchPolicy::from_config(&config);
    let mut summary = CoverageRepairSummary {
        attempted: 0,
        repaired: 0,
        still_failed: 0,
        skipped_policy: 0,
        skipped_disabled: 0,
    };
    for row in rows {
        let candidate = match OnDemandFetchPolicy::candidate_from_input(&row.source_url) {
            Ok(candidate) => candidate,
            Err(_) => {
                summary.skipped_policy += 1;
                continue;
            }
        };
        if !policy.is_enabled() {
            summary.skipped_disabled += 1;
            continue;
        }
        summary.attempted += 1;
        match on_demand_fetch_candidate(paths, candidate.clone(), source, false).await {
            Ok(_) => {
                update_coverage_repaired(&conn, row.id, &candidate)?;
                summary.repaired += 1;
            }
            Err(error) => {
                update_coverage_failed(&conn, row.id, &error.to_string())?;
                summary.still_failed += 1;
            }
        }
    }
    Ok(summary)
}

async fn refresh_doc_from_source<S: TextSource>(
    paths: &Paths,
    path: &str,
    source: &S,
) -> Result<()> {
    let conn = open_db(paths)?;
    init_db(&conn)?;
    let doc = get_doc(&conn, path)?.ok_or_else(|| anyhow!("path not found: {path}"))?;
    let content = fetch_required_text(source, &doc.url).await?;
    let source_doc = SourceDoc {
        url: doc.url,
        title_hint: Some(doc.title),
        content,
        source: doc.source,
    };
    let record = store_source_doc(paths, &source_doc)?;
    upsert_doc(&conn, &record)?;
    rebuild_tantivy_from_db(paths)?;
    Ok(())
}

async fn refresh_stale_docs_from_source<S: TextSource>(paths: &Paths, source: &S) -> Result<()> {
    let conn = open_db(paths)?;
    init_db(&conn)?;
    update_doc_freshness_states(&conn)?;
    let docs = stale_refresh_candidates(&conn, 100)?;
    let mut refreshed = 0usize;
    let mut warnings = Vec::new();
    for doc in docs {
        match source.fetch_text(&doc.url).await {
            Ok(content) => {
                let source_doc = SourceDoc {
                    url: doc.url,
                    title_hint: Some(doc.title),
                    content,
                    source: doc.source,
                };
                let record = store_source_doc(paths, &source_doc)?;
                upsert_doc(&conn, &record)?;
                refreshed += 1;
            }
            Err(error) => warnings.push(format!("{}: {}", doc.path, error.reason)),
        }
    }
    set_meta(&conn, "last_aging_sweep_at", &now_iso())?;
    if !warnings.is_empty() {
        set_meta(&conn, "last_aging_sweep_warning", &warnings.join("; "))?;
    }
    if refreshed > 0 {
        rebuild_tantivy_from_db(paths)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct DaemonIdentity {
    canonical_home: PathBuf,
    package_version: String,
    schema_version: String,
    config_hash: String,
}

#[derive(Debug, Clone)]
struct DaemonPaths {
    identity: DaemonIdentity,
    socket: PathBuf,
    lock: PathBuf,
    pid: PathBuf,
}

struct DaemonLock {
    path: PathBuf,
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl DaemonIdentity {
    fn for_paths(paths: &Paths) -> Result<Self> {
        fs::create_dir_all(&paths.home)
            .with_context(|| format!("create {}", paths.home.display()))?;
        let canonical_home = fs::canonicalize(&paths.home)
            .with_context(|| format!("canonicalize {}", paths.home.display()))?;
        let config_hash = hash_optional_file(&canonical_home.join("config.toml"))?;
        Ok(Self {
            canonical_home,
            package_version: env!("CARGO_PKG_VERSION").to_string(),
            schema_version: SCHEMA_VERSION.to_string(),
            config_hash,
        })
    }

    fn hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.canonical_home.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(self.package_version.as_bytes());
        hasher.update([0]);
        hasher.update(self.schema_version.as_bytes());
        hasher.update([0]);
        hasher.update(self.config_hash.as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

impl DaemonPaths {
    fn for_paths(paths: &Paths) -> Result<Self> {
        let identity = DaemonIdentity::for_paths(paths)?;
        let identity_hash = identity.hash();
        let runtime_dir = daemon_runtime_dir()?;
        Ok(Self {
            socket: runtime_dir.join(format!("{identity_hash}.sock")),
            lock: runtime_dir.join(format!("{identity_hash}.lock")),
            pid: runtime_dir.join(format!("{identity_hash}.pid")),
            identity,
        })
    }
}

impl DaemonLock {
    fn acquire(path: &Path, timeout: Duration) -> Result<Self> {
        let started = Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())?;
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_file_is_stale(path, Duration::from_secs(30))
                        || started.elapsed() > timeout
                    {
                        let _ = fs::remove_file(path);
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

fn hash_optional_file(path: &Path) -> Result<String> {
    match fs::read(path) {
        Ok(bytes) => {
            let mut hasher = Sha256::new();
            hasher.update(bytes);
            Ok(format!("{:x}", hasher.finalize()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok("missing".to_string()),
        Err(error) => Err(error.into()),
    }
}

fn daemon_runtime_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("shopify-rextant-daemons");
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    Ok(dir)
}

fn lock_file_is_stale(path: &Path, max_age: Duration) -> bool {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .map(|elapsed| elapsed > max_age)
        .unwrap_or(true)
}

fn daemon_socket_healthy(socket: &Path) -> bool {
    let Ok(mut writer) = UnixStream::connect(socket) else {
        return false;
    };
    let _ = writer.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = writer.set_write_timeout(Some(Duration::from_millis(500)));
    let Ok(reader_stream) = writer.try_clone() else {
        return false;
    };
    let mut reader = std::io::BufReader::new(reader_stream);
    if write_mcp_message(
        &mut writer,
        &json!({"jsonrpc":"2.0","id":"health","method":"tools/list"}),
    )
    .is_err()
    {
        return false;
    }
    let Ok(Some(message)) = read_mcp_message(&mut reader) else {
        return false;
    };
    serde_json::from_slice::<Value>(&message)
        .ok()
        .and_then(|value| value.pointer("/result/tools").cloned())
        .and_then(|tools| tools.as_array().map(|tools| !tools.is_empty()))
        .unwrap_or(false)
}

fn cleanup_stale_daemon_artifacts(paths: &DaemonPaths) -> Result<()> {
    if paths.socket.exists() && !daemon_socket_healthy(&paths.socket) {
        fs::remove_file(&paths.socket)
            .with_context(|| format!("remove stale socket {}", paths.socket.display()))?;
    }
    if paths.pid.exists() && !paths.socket.exists() {
        fs::remove_file(&paths.pid)
            .with_context(|| format!("remove stale pid {}", paths.pid.display()))?;
    }
    Ok(())
}

fn wait_for_daemon_ready(socket: &Path, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if daemon_socket_healthy(socket) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("daemon did not become ready at {}", socket.display())
}

fn daemon_idle_timeout_secs() -> u64 {
    std::env::var("SHOPIFY_REXTANT_DAEMON_IDLE_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(600)
}

fn spawn_daemon(paths: &DaemonPaths) -> Result<()> {
    let exe = std::env::current_exe()?;
    ProcessCommand::new(exe)
        .arg("--home")
        .arg(&paths.identity.canonical_home)
        .arg("daemon")
        .arg("--idle-timeout-secs")
        .arg(daemon_idle_timeout_secs().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawn shopify-rextant daemon")?;
    Ok(())
}

fn ensure_daemon(paths: &Paths) -> Result<DaemonPaths> {
    let daemon_paths = DaemonPaths::for_paths(paths)?;
    if daemon_socket_healthy(&daemon_paths.socket) {
        return Ok(daemon_paths);
    }
    let _lock = DaemonLock::acquire(&daemon_paths.lock, Duration::from_secs(5))?;
    if daemon_socket_healthy(&daemon_paths.socket) {
        return Ok(daemon_paths);
    }
    cleanup_stale_daemon_artifacts(&daemon_paths)?;
    spawn_daemon(&daemon_paths)?;
    wait_for_daemon_ready(&daemon_paths.socket, Duration::from_secs(5))?;
    Ok(daemon_paths)
}

fn write_mcp_body<W: Write>(writer: &mut W, body: &[u8]) -> Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body)?;
    writer.flush()?;
    Ok(())
}

async fn serve(paths: Paths) -> Result<()> {
    let daemon_paths = ensure_daemon(&paths)?;
    let mut daemon_writer = UnixStream::connect(&daemon_paths.socket)
        .with_context(|| format!("connect {}", daemon_paths.socket.display()))?;
    let daemon_reader_stream = daemon_writer.try_clone()?;
    let mut daemon_reader = std::io::BufReader::new(daemon_reader_stream);
    let stdin = std::io::stdin();
    let mut stdin_reader = std::io::BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut stdout_writer = stdout.lock();

    while let Some(message) = read_mcp_message(&mut stdin_reader)? {
        let request: Value = serde_json::from_slice(&message)?;
        write_mcp_body(&mut daemon_writer, &message)?;
        if request.get("id").is_none() {
            continue;
        }
        let response = read_mcp_message(&mut daemon_reader)?
            .ok_or_else(|| anyhow!("daemon disconnected before response"))?;
        let response: Value = serde_json::from_slice(&response)?;
        write_mcp_message(&mut stdout_writer, &response)?;
    }
    Ok(())
}

async fn serve_direct(paths: Paths) -> Result<()> {
    spawn_background_workers(paths.clone());
    let state = Arc::new(ServerState::new(paths));
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();

    while let Some(message) = read_mcp_message(&mut reader)? {
        let request: Value = serde_json::from_slice(&message)?;
        if request.get("id").is_none() {
            continue;
        }
        let response = state.handle_mcp_request(request).await;
        write_mcp_message(&mut writer, &response)?;
    }
    Ok(())
}

async fn run_daemon(paths: Paths, idle_timeout: Duration) -> Result<()> {
    let daemon_paths = DaemonPaths::for_paths(&paths)?;
    cleanup_stale_daemon_artifacts(&daemon_paths)?;
    if daemon_paths.socket.exists() {
        fs::remove_file(&daemon_paths.socket)
            .with_context(|| format!("remove existing socket {}", daemon_paths.socket.display()))?;
    }
    let listener = UnixListener::bind(&daemon_paths.socket)
        .with_context(|| format!("bind {}", daemon_paths.socket.display()))?;
    let _ = fs::set_permissions(&daemon_paths.socket, fs::Permissions::from_mode(0o600));
    fs::write(&daemon_paths.pid, std::process::id().to_string())
        .with_context(|| format!("write {}", daemon_paths.pid.display()))?;
    listener.set_nonblocking(true)?;

    spawn_background_workers(paths.clone());
    let state = Arc::new(ServerState::new(paths));
    state.spawn_search_warmup();

    let active_clients = Arc::new(AtomicUsize::new(0));
    let last_idle = Arc::new(Mutex::new(Instant::now()));
    let handle = tokio::runtime::Handle::current();

    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                active_clients.fetch_add(1, Ordering::SeqCst);
                spawn_daemon_client(
                    stream,
                    Arc::clone(&state),
                    Arc::clone(&active_clients),
                    Arc::clone(&last_idle),
                    handle.clone(),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }

        if active_clients.load(Ordering::SeqCst) == 0 {
            let idle_for = last_idle
                .lock()
                .map_err(|_| anyhow!("daemon idle lock poisoned"))?
                .elapsed();
            if idle_for >= idle_timeout {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = fs::remove_file(&daemon_paths.socket);
    let _ = fs::remove_file(&daemon_paths.pid);
    Ok(())
}

fn spawn_daemon_client(
    stream: UnixStream,
    state: Arc<ServerState>,
    active_clients: Arc<AtomicUsize>,
    last_idle: Arc<Mutex<Instant>>,
    handle: tokio::runtime::Handle,
) {
    std::thread::spawn(move || {
        let result = handle_daemon_client(stream, state, handle);
        if let Err(error) = result {
            eprintln!("daemon client error: {error}");
        }
        if active_clients.fetch_sub(1, Ordering::SeqCst) == 1 {
            if let Ok(mut last_idle) = last_idle.lock() {
                *last_idle = Instant::now();
            }
        }
    });
}

fn handle_daemon_client(
    stream: UnixStream,
    state: Arc<ServerState>,
    handle: tokio::runtime::Handle,
) -> Result<()> {
    let reader_stream = stream.try_clone()?;
    let mut reader = std::io::BufReader::new(reader_stream);
    let mut writer = stream;
    while let Some(message) = read_mcp_message(&mut reader)? {
        let request: Value = serde_json::from_slice(&message)?;
        if request.get("id").is_none() {
            continue;
        }
        let response = handle.block_on(state.handle_mcp_request(request));
        write_mcp_message(&mut writer, &response)?;
    }
    Ok(())
}

struct ServerState {
    paths: Paths,
    search_runtime: Mutex<Option<SearchRuntime>>,
    search_warmup: Mutex<Option<std::thread::JoinHandle<Result<()>>>>,
}

impl ServerState {
    fn new(paths: Paths) -> Self {
        Self {
            paths,
            search_runtime: Mutex::new(None),
            search_warmup: Mutex::new(None),
        }
    }

    fn spawn_search_warmup(self: &Arc<Self>) {
        if self
            .search_runtime
            .lock()
            .map(|runtime| runtime.is_some())
            .unwrap_or(false)
        {
            return;
        }
        let mut warmup = match self.search_warmup.lock() {
            Ok(warmup) => warmup,
            Err(_) => {
                eprintln!("search runtime warmup setup error: lock poisoned");
                return;
            }
        };
        if warmup.is_some() {
            return;
        }
        let state = Arc::clone(self);
        let handle = std::thread::spawn(move || state.warm_search_runtime());
        *warmup = Some(handle);
    }

    fn warm_search_runtime(&self) -> Result<()> {
        let runtime = SearchRuntime::open(&self.paths)?;
        let _ = japanese_segmenter()?;
        let mut guard = self
            .search_runtime
            .lock()
            .map_err(|_| anyhow!("search runtime lock poisoned"))?;
        if guard.is_none() {
            *guard = runtime;
        }
        Ok(())
    }

    fn finish_search_warmup(&self) -> Result<()> {
        let handle = self
            .search_warmup
            .lock()
            .map_err(|_| anyhow!("search warmup lock poisoned"))?
            .take();
        if let Some(handle) = handle {
            handle
                .join()
                .map_err(|_| anyhow!("search runtime warmup panicked"))??;
        }
        Ok(())
    }

    async fn handle_mcp_request(self: &Arc<Self>, request: Value) -> Value {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let result = match method {
            "initialize" => {
                self.spawn_search_warmup();
                Ok(json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "shopify-rextant",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }))
            }
            "tools/list" => Ok(json!({ "tools": tool_descriptors() })),
            "tools/call" => {
                self.call_mcp_tool(request.get("params").cloned().unwrap_or(Value::Null))
                    .await
            }
            _ => Err(json_rpc_error(
                -32601,
                "Method not found",
                json!({ "method": method }),
            )),
        };

        match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
        }
    }

    async fn call_mcp_tool(&self, params: Value) -> std::result::Result<Value, Value> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| json_rpc_error(-32602, "Missing tool name", Value::Null))?;
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        let value = match name {
            "shopify_status" => status(&self.paths)
                .map(to_json_value)
                .map_err(ToolError::from),
            "shopify_fetch" => {
                let args: Result<FetchArgs> = serde_json::from_value(args)
                    .map_err(|e| anyhow!("invalid shopify_fetch args: {e}"));
                match args {
                    Ok(args) => match shopify_fetch(&self.paths, &args).await {
                        Ok(response) => self
                            .reload_search_runtime()
                            .map(|()| to_json_value(response))
                            .map_err(ToolError::from),
                        Err(error) => Err(error),
                    },
                    Err(error) => Err(ToolError::from(error)),
                }
            }
            "shopify_map" => {
                let args: Result<MapArgs> = serde_json::from_value(args)
                    .map_err(|e| anyhow!("invalid shopify_map args: {e}"));
                args.and_then(|args| {
                    self.ensure_search_runtime()?;
                    let guard = self
                        .search_runtime
                        .lock()
                        .map_err(|_| anyhow!("search runtime lock poisoned"))?;
                    shopify_map_with_runtime(&self.paths, &args, guard.as_ref()).map(to_json_value)
                })
                .map_err(ToolError::from)
            }
            "shopify_search" => {
                let args: Result<SearchArgs> = serde_json::from_value(args)
                    .map_err(|e| anyhow!("invalid shopify_search args: {e}"));
                args.and_then(|args| {
                    self.ensure_search_runtime()?;
                    let guard = self
                        .search_runtime
                        .lock()
                        .map_err(|_| anyhow!("search runtime lock poisoned"))?;
                    search_docs_with_runtime(
                        &self.paths,
                        guard.as_ref(),
                        &args.query,
                        args.version.as_deref(),
                        args.limit.unwrap_or(10),
                    )
                    .map(to_json_value)
                })
                .map_err(ToolError::from)
            }
            _ => Err(ToolError::from(anyhow!("unknown tool: {name}"))),
        }
        .map_err(|e| e.json_rpc_error())?;

        let text = serde_json::to_string_pretty(&value)
            .map_err(|e| json_rpc_error(-32000, &e.to_string(), Value::Null))?;
        Ok(json!({
            "content": [{ "type": "text", "text": text }],
            "structuredContent": value,
            "isError": false
        }))
    }

    fn ensure_search_runtime(&self) -> Result<()> {
        self.finish_search_warmup()?;
        let mut guard = self
            .search_runtime
            .lock()
            .map_err(|_| anyhow!("search runtime lock poisoned"))?;
        if guard.is_none() && self.paths.tantivy.join("meta.json").exists() {
            *guard = SearchRuntime::open(&self.paths)?;
        }
        Ok(())
    }

    fn reload_search_runtime(&self) -> Result<()> {
        let mut guard = self
            .search_runtime
            .lock()
            .map_err(|_| anyhow!("search runtime lock poisoned"))?;
        *guard = SearchRuntime::open(&self.paths)?;
        Ok(())
    }
}

fn spawn_background_workers(paths: Paths) {
    tokio::spawn(async move {
        let source_urls = IndexSourceUrls::default();
        let source = match ReqwestTextSource::new() {
            Ok(source) => source,
            Err(error) => {
                eprintln!("version_watcher setup error: {error}");
                return;
            }
        };
        let mut interval = tokio::time::interval(Duration::from_secs(86_400));
        loop {
            interval.tick().await;
            if let Err(error) = check_new_versions_from_source(&paths, &source_urls, &source).await
            {
                eprintln!("version_watcher error: {error}");
            }
        }
    });
}

#[cfg(test)]
async fn handle_mcp_request(paths: &Paths, request: Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-11-25",
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": {
                "name": "shopify-rextant",
                "version": env!("CARGO_PKG_VERSION")
            }
        })),
        "tools/list" => Ok(json!({ "tools": tool_descriptors() })),
        "tools/call" => {
            call_mcp_tool(paths, request.get("params").cloned().unwrap_or(Value::Null)).await
        }
        _ => Err(json_rpc_error(
            -32601,
            "Method not found",
            json!({ "method": method }),
        )),
    };

    match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
    }
}

#[cfg(test)]
async fn call_mcp_tool(paths: &Paths, params: Value) -> std::result::Result<Value, Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| json_rpc_error(-32602, "Missing tool name", Value::Null))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let value = match name {
        "shopify_status" => status(paths).map(to_json_value).map_err(ToolError::from),
        "shopify_fetch" => {
            let args: Result<FetchArgs> = serde_json::from_value(args)
                .map_err(|e| anyhow!("invalid shopify_fetch args: {e}"));
            match args {
                Ok(args) => shopify_fetch(paths, &args).await.map(to_json_value),
                Err(error) => Err(ToolError::from(error)),
            }
        }
        "shopify_map" => {
            let args: Result<MapArgs> =
                serde_json::from_value(args).map_err(|e| anyhow!("invalid shopify_map args: {e}"));
            args.and_then(|args| shopify_map(paths, &args).map(to_json_value))
                .map_err(ToolError::from)
        }
        "shopify_search" => {
            let args: Result<SearchArgs> = serde_json::from_value(args)
                .map_err(|e| anyhow!("invalid shopify_search args: {e}"));
            args.and_then(|args| {
                search_docs(
                    paths,
                    &args.query,
                    args.version.as_deref(),
                    args.limit.unwrap_or(10),
                )
                .map(to_json_value)
            })
            .map_err(ToolError::from)
        }
        _ => Err(ToolError::from(anyhow!("unknown tool: {name}"))),
    }
    .map_err(|e| e.json_rpc_error())?;

    let text = serde_json::to_string_pretty(&value)
        .map_err(|e| json_rpc_error(-32000, &e.to_string(), Value::Null))?;
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": value,
        "isError": false
    }))
}

fn tool_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "shopify_map",
            "description": "Return a local source map of Shopify docs for a query, path, or concept-like term.",
            "inputSchema": {
                "type": "object",
                "required": ["from"],
                "properties": {
                    "from": { "type": "string" },
                    "radius": { "type": "integer", "enum": [1, 2, 3], "default": 2 },
                    "lens": { "type": "string", "enum": ["concept", "doc", "task", "auto"], "default": "auto" },
                    "version": { "type": "string" },
                    "max_nodes": { "type": "integer", "minimum": 1, "maximum": 100, "default": 30 }
                }
            }
        }),
        json!({
            "name": "shopify_fetch",
            "description": "Fetch raw Shopify documentation by local docs path. When local on-demand fetch is enabled, allowed shopify.dev docs/changelog URLs or unindexed canonical paths can be recovered and indexed.",
            "inputSchema": {
                "type": "object",
                "anyOf": [
                    { "required": ["path"] },
                    { "required": ["url"] }
                ],
                "properties": {
                    "path": { "type": "string" },
                    "url": {
                        "type": "string",
                        "description": "Allowed only for https://shopify.dev/docs/** and https://shopify.dev/changelog/**. Requires [index].enable_on_demand_fetch=true in local config."
                    },
                    "anchor": { "type": "string" },
                    "include_code_blocks": { "type": "boolean", "default": true },
                    "max_chars": { "type": "integer", "minimum": 1, "default": 20000 }
                }
            }
        }),
        json!({
            "name": "shopify_search",
            "description": "Search the local Shopify documentation index.",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string" },
                    "version": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 10 }
                }
            }
        }),
        json!({
            "name": "shopify_status",
            "description": "Return local index status and warnings.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
    ]
}

fn json_rpc_error(code: i64, message: &str, data: Value) -> Value {
    json!({ "code": code, "message": message, "data": data })
}

pub(crate) async fn shopify_fetch(
    paths: &Paths,
    args: &FetchArgs,
) -> std::result::Result<FetchResponse, ToolError> {
    let source = ReqwestTextSource::new()?;
    shopify_fetch_from_source(paths, args, &source).await
}

async fn shopify_fetch_from_source<S: TextSource>(
    paths: &Paths,
    args: &FetchArgs,
    source: &S,
) -> std::result::Result<FetchResponse, ToolError> {
    if let Some(url) = &args.url {
        let record = on_demand_fetch_from_input(paths, url, source).await?;
        return fetch_local_doc(paths, &record.path, args).map_err(ToolError::from);
    }
    let path = args
        .path
        .as_deref()
        .ok_or_else(|| ToolError::from(anyhow!("shopify_fetch requires path")))?;
    let conn = open_db(paths)?;
    init_db(&conn)?;
    if get_doc(&conn, path)?.is_none() && is_on_demand_allowed_path(path) {
        let record = on_demand_fetch_from_input(paths, path, source).await?;
        return fetch_local_doc(paths, &record.path, args).map_err(ToolError::from);
    }
    fetch_local_doc(paths, path, args).map_err(ToolError::from)
}

async fn on_demand_fetch_from_input<S: TextSource>(
    paths: &Paths,
    input: &str,
    source: &S,
) -> std::result::Result<DocRecord, ToolError> {
    let candidate = OnDemandFetchPolicy::candidate_from_input(input)
        .map_err(|_| ToolError::outside_scope(input))?;
    let config = load_config(paths)?;
    let policy = OnDemandFetchPolicy::from_config(&config);
    ensure_on_demand_enabled(&policy, &candidate)?;
    on_demand_fetch_candidate(paths, candidate, source, true).await
}

async fn on_demand_fetch_candidate<S: TextSource>(
    paths: &Paths,
    candidate: OnDemandFetchCandidate,
    source: &S,
    record_coverage: bool,
) -> std::result::Result<DocRecord, ToolError> {
    fs::create_dir_all(&paths.raw).map_err(|e| ToolError::from(anyhow!(e)))?;
    fs::create_dir_all(&paths.tantivy).map_err(|e| ToolError::from(anyhow!(e)))?;
    let conn = open_db(paths)?;
    init_db(&conn)?;
    let link = MarkdownLink {
        title: candidate.canonical_path.clone(),
        url: candidate.source_url.clone(),
        source: "on_demand".to_string(),
    };
    let source_doc = fetch_source_doc(source, &link)
        .await
        .map_err(|error| ToolError::from(anyhow!("GET {} failed: {}", link.url, error.reason)))?;
    let content = source_doc.content.clone();
    let record = store_source_doc(paths, &source_doc)?;
    upsert_doc(&conn, &record)?;
    if record_coverage {
        insert_coverage_event(&conn, &CoverageEvent::indexed(&link))?;
    }
    upsert_tantivy_doc(paths, &record, &content)?;
    Ok(get_doc(&conn, &record.path)?.unwrap_or(record))
}

fn fetch_local_doc(paths: &Paths, path: &str, args: &FetchArgs) -> Result<FetchResponse> {
    let conn = open_db(paths)?;
    let doc = get_doc(&conn, path)?.ok_or_else(|| {
        let doc_count = count_docs(&conn).unwrap_or(0);
        anyhow!("path not found: {path}; index_status.doc_count={doc_count}")
    })?;
    let mut content = fs::read_to_string(paths.raw_file(&doc.raw_path))?;
    let sections = extract_sections(&content);
    if let Some(anchor) = args.anchor.as_deref() {
        content = section_content(&content, &sections, anchor)
            .ok_or_else(|| anyhow!("anchor not found: {anchor}"))?;
    }
    if args.include_code_blocks == Some(false) {
        content = remove_fenced_code_blocks(&content);
    }
    let max_chars = args.max_chars.unwrap_or(20_000);
    let truncated = content.chars().count() > max_chars;
    if truncated {
        content = content.chars().take(max_chars).collect();
    }
    let source_url = doc.url.clone();
    Ok(FetchResponse {
        path: doc.path.clone(),
        title: doc.title.clone(),
        url: source_url.clone(),
        source_url,
        content,
        sections,
        truncated,
        staleness: staleness_for_doc(&conn, &doc)?,
    })
}

#[allow(dead_code)]
pub(crate) fn shopify_map(paths: &Paths, args: &MapArgs) -> Result<MapResponse> {
    shopify_map_with_runtime(paths, args, None)
}

fn shopify_map_with_runtime(
    paths: &Paths,
    args: &MapArgs,
    search_runtime: Option<&SearchRuntime>,
) -> Result<MapResponse> {
    let limit = args.max_nodes.unwrap_or(30).clamp(1, 100);
    let radius = args.radius.unwrap_or(2).clamp(1, 3);
    let lens = args.lens.as_deref().unwrap_or("auto");
    let conn = open_db(paths)?;
    init_db(&conn)?;
    let index_status = status(paths)?;
    let versions_available = versions_available(paths).unwrap_or_default();
    let version_used = args
        .version
        .clone()
        .or_else(|| versions_available.first().cloned())
        .unwrap_or_else(|| "evergreen".to_string());
    let mut docs = Vec::new();
    let mut start_nodes = Vec::new();
    let mut resolved_as = "free_text";
    let confidence;
    let graph_has_edges = index_status.index.concept_count > 0 && index_status.index.edge_count > 0;

    if is_doc_like_query(&args.from) {
        docs = get_doc(&conn, &args.from)?.into_iter().collect();
        if !docs.is_empty() {
            start_nodes.push(GraphNodeKey {
                node_type: "doc".to_string(),
                id: args.from.clone(),
            });
        }
        resolved_as = "doc_path";
        confidence = if docs.is_empty() { "low" } else { "exact" };
    } else if graph_has_edges {
        if let Some(concept) = find_concept_by_name(&conn, &args.from, args.version.as_deref())? {
            start_nodes.push(GraphNodeKey {
                node_type: "concept".to_string(),
                id: concept.id,
            });
            resolved_as = "concept_name";
            confidence = "exact";
        } else {
            docs = search_docs_with_runtime(
                paths,
                search_runtime,
                &args.from,
                args.version.as_deref(),
                limit,
            )?;
            docs = dedupe_docs_by_path(docs);
            docs.truncate(limit);
            start_nodes.extend(docs.iter().map(|doc| GraphNodeKey {
                node_type: "doc".to_string(),
                id: doc.path.clone(),
            }));
            confidence = if docs.is_empty() { "low" } else { "medium" };
        }
    } else {
        docs = search_docs_with_runtime(
            paths,
            search_runtime,
            &args.from,
            args.version.as_deref(),
            limit,
        )?;
        docs = dedupe_docs_by_path(docs);
        docs.truncate(limit);
        confidence = if docs.is_empty() { "low" } else { "medium" };
    }

    let graph_expansion = if graph_has_edges && !start_nodes.is_empty() {
        Some(expand_graph(
            &conn,
            &start_nodes,
            radius as usize,
            limit,
            &docs,
        )?)
    } else {
        None
    };
    let graph_available = graph_expansion
        .as_ref()
        .is_some_and(|expansion| !expansion.edges.is_empty());
    let entry_points = start_nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    let mut coverage_warning = map_coverage_warning(&index_status);
    if !graph_available {
        coverage_warning.get_or_insert_with(|| {
            "Graph coverage is unavailable for this query; using v0.1 FTS fallback.".to_string()
        });
    }
    let on_demand_candidate = if docs.is_empty() {
        OnDemandFetchPolicy::candidate_from_input(&args.from)
            .ok()
            .map(|candidate| OnDemandCandidate {
                url: candidate.source_url,
                enabled: load_config(paths)
                    .map(|config| config.index.enable_on_demand_fetch)
                    .unwrap_or(false),
                reason: "No local docs matched. shopify_fetch can recover this official Shopify docs URL only when local on-demand fetch is enabled.".to_string(),
            })
    } else {
        None
    };
    let meta = MapMeta {
        generated_at: now_iso(),
        index_age_days: index_age_days(index_status.last_full_build.as_deref()),
        versions_available,
        version_used,
        coverage_warning: coverage_warning.clone(),
        graph_available,
        index_status: MapIndexStatus {
            doc_count: index_status.doc_count,
            skipped_count: index_status.coverage.skipped_count,
            failed_count: index_status.coverage.failed_count,
        },
        on_demand_candidate,
        query_interpretation: QueryInterpretation {
            resolved_as: resolved_as.to_string(),
            entry_points,
            confidence: confidence.to_string(),
        },
    };

    if graph_available {
        let expansion = graph_expansion.expect("checked graph expansion");
        let center = center_for_key(&conn, start_nodes.first(), &args.from)?;
        let query_plan =
            graph_query_plan(&expansion.suggested_reading_order, lens, radius as usize);
        return Ok(MapResponse {
            center,
            nodes: expansion.nodes,
            edges: expansion.edges,
            suggested_reading_order: expansion.suggested_reading_order,
            query_plan,
            index_status,
            meta,
        });
    }

    let Some(center_doc) = docs.first() else {
        return Ok(MapResponse {
            center: MapCenter {
                id: args.from.clone(),
                kind: "doc".to_string(),
                path: None,
                title: args.from.clone(),
            },
            nodes: Vec::new(),
            edges: Vec::new(),
            suggested_reading_order: Vec::new(),
            query_plan: vec![
                QueryPlanStep {
                    step: 1,
                    action: "inspect_status".to_string(),
                    path: None,
                    reason: "No local docs matched; inspect index and coverage before using web fallback."
                        .to_string(),
                },
                QueryPlanStep {
                    step: 2,
                    action: "refresh".to_string(),
                    path: None,
                    reason: "Rebuild or refresh if the index is empty, stale, or has coverage failures."
                        .to_string(),
                },
            ],
            index_status,
            meta,
        });
    };
    let center = MapCenter {
        id: center_doc.path.clone(),
        kind: "doc".to_string(),
        path: Some(center_doc.path.clone()),
        title: center_doc.title.clone(),
    };
    let nodes = docs
        .iter()
        .enumerate()
        .map(|(i, doc)| MapNode {
            id: doc.path.clone(),
            kind: "doc".to_string(),
            subkind: doc.content_class.clone(),
            path: doc.path.clone(),
            title: doc.title.clone(),
            summary_from_source: doc.summary_raw.clone(),
            version: doc.version.clone(),
            api_surface: doc.api_surface.clone(),
            doc_type: doc.doc_type.clone(),
            reading_time_min: doc.reading_time_min,
            staleness: staleness_for_doc(&conn, doc).unwrap_or_else(|_| staleness(doc)),
            distance_from_center: usize::from(i > 0),
        })
        .collect::<Vec<_>>();
    let suggested_reading_order = nodes.iter().map(|node| node.path.clone()).collect();
    let mut query_plan = Vec::new();
    if !graph_available {
        query_plan.push(QueryPlanStep {
            step: 1,
            action: "inspect_status".to_string(),
            path: None,
            reason: "Graph data is unavailable or empty; inspect status before trusting coverage."
                .to_string(),
        });
    }
    let step_offset = query_plan.len();
    query_plan.extend(nodes
        .iter()
        .take(3)
        .enumerate()
        .map(|(i, node)| QueryPlanStep {
            step: step_offset + i + 1,
            action: "fetch".to_string(),
            path: Some(node.path.clone()),
            reason: if i == 0 {
                format!(
                    "Highest-ranked local FTS candidate for lens={lens}, radius={radius}; fetch raw source before answering."
                )
            } else {
                "Secondary local FTS candidate to compare against the primary source.".to_string()
            },
        }));
    Ok(MapResponse {
        center,
        nodes,
        edges: Vec::new(),
        suggested_reading_order,
        query_plan,
        index_status,
        meta,
    })
}

fn is_doc_like_query(value: &str) -> bool {
    value.starts_with("/docs/") || value.starts_with("/changelog") || value == "/llms.txt"
}

fn find_concept_by_name(
    conn: &Connection,
    name: &str,
    version: Option<&str>,
) -> Result<Option<ConceptRecord>> {
    conn.query_row(
        "
        SELECT id, kind, name, version, defined_in_path, deprecated, deprecated_since,
               deprecation_reason, replaced_by, kind_metadata
        FROM concepts
        WHERE name = ?1
          AND (?2 IS NULL OR version = ?2)
        ORDER BY
          CASE kind
            WHEN 'graphql_type' THEN 0
            WHEN 'graphql_input_object' THEN 1
            ELSE 2
          END,
          id
        LIMIT 1
        ",
        params![name, version],
        concept_from_row,
    )
    .optional()
    .map_err(Into::into)
}

fn get_concept(conn: &Connection, id: &str) -> Result<Option<ConceptRecord>> {
    conn.query_row(
        "
        SELECT id, kind, name, version, defined_in_path, deprecated, deprecated_since,
               deprecation_reason, replaced_by, kind_metadata
        FROM concepts
        WHERE id = ?1
        ",
        params![id],
        concept_from_row,
    )
    .optional()
    .map_err(Into::into)
}

fn concept_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConceptRecord> {
    Ok(ConceptRecord {
        id: row.get(0)?,
        kind: row.get(1)?,
        name: row.get(2)?,
        version: row.get(3)?,
        defined_in_path: row.get(4)?,
        deprecated: row.get::<_, i64>(5)? != 0,
        deprecated_since: row.get(6)?,
        deprecation_reason: row.get(7)?,
        replaced_by: row.get(8)?,
        kind_metadata: row.get(9)?,
    })
}

fn expand_graph(
    conn: &Connection,
    start_nodes: &[GraphNodeKey],
    radius: usize,
    limit: usize,
    extra_docs: &[DocRecord],
) -> Result<GraphExpansion> {
    let all_edges = load_edges(conn)?;
    let mut distances = HashMap::<String, usize>::new();
    let mut node_types = HashMap::<String, String>::new();
    let mut queue = VecDeque::new();

    for start in start_nodes {
        distances.insert(start.id.clone(), 0);
        node_types.insert(start.id.clone(), start.node_type.clone());
        queue.push_back(start.id.clone());
    }

    while let Some(current) = queue.pop_front() {
        let distance = *distances.get(&current).unwrap_or(&0);
        if distance >= radius || distances.len() >= limit {
            continue;
        }
        for edge in &all_edges {
            let neighbor = if edge.from_id == current {
                Some((edge.to_id.as_str(), edge.to_type.as_str()))
            } else if edge.to_id == current {
                Some((edge.from_id.as_str(), edge.from_type.as_str()))
            } else {
                None
            };
            if let Some((neighbor_id, neighbor_type)) = neighbor {
                if !distances.contains_key(neighbor_id) && distances.len() < limit {
                    distances.insert(neighbor_id.to_string(), distance + 1);
                    node_types.insert(neighbor_id.to_string(), neighbor_type.to_string());
                    queue.push_back(neighbor_id.to_string());
                }
            }
        }
    }

    for doc in extra_docs {
        if distances.len() >= limit {
            break;
        }
        distances.entry(doc.path.clone()).or_insert(usize::from(
            !start_nodes.iter().any(|node| node.id == doc.path),
        ));
        node_types
            .entry(doc.path.clone())
            .or_insert_with(|| "doc".to_string());
    }

    let mut nodes = distances
        .iter()
        .filter_map(|(id, distance)| {
            let node_type = node_types.get(id)?;
            graph_map_node(conn, node_type, id, *distance).transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    nodes.sort_by(|a, b| {
        a.distance_from_center
            .cmp(&b.distance_from_center)
            .then_with(|| node_kind_rank(&a.kind).cmp(&node_kind_rank(&b.kind)))
            .then_with(|| a.id.cmp(&b.id))
    });

    let returned_ids = nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<HashSet<_>>();
    let mut edges = all_edges
        .iter()
        .filter(|edge| returned_ids.contains(&edge.from_id) && returned_ids.contains(&edge.to_id))
        .map(|edge| {
            json!({
                "from": edge.from_id,
                "to": edge.to_id,
                "kind": edge.kind,
                "weight": edge.weight,
                "source_path": edge.source_path,
            })
        })
        .collect::<Vec<_>>();
    edges.sort_by_key(|edge| {
        format!(
            "{}:{}:{}",
            edge.get("from").and_then(Value::as_str).unwrap_or_default(),
            edge.get("kind").and_then(Value::as_str).unwrap_or_default(),
            edge.get("to").and_then(Value::as_str).unwrap_or_default()
        )
    });

    let mut suggested_reading_order = nodes
        .iter()
        .filter(|node| {
            node.kind == "doc"
                && (node.path.starts_with("/docs/") || node.path.starts_with("/changelog/"))
        })
        .map(|node| node.path.clone())
        .collect::<Vec<_>>();
    suggested_reading_order.sort_by(|a, b| {
        let a_rank = get_doc(conn, a)
            .ok()
            .flatten()
            .map(|doc| doc_type_rank(&doc.doc_type))
            .unwrap_or(99);
        let b_rank = get_doc(conn, b)
            .ok()
            .flatten()
            .map(|doc| doc_type_rank(&doc.doc_type))
            .unwrap_or(99);
        a_rank.cmp(&b_rank).then_with(|| a.cmp(b))
    });
    suggested_reading_order.dedup();

    Ok(GraphExpansion {
        nodes,
        edges,
        suggested_reading_order,
    })
}

fn load_edges(conn: &Connection) -> Result<Vec<GraphEdgeRecord>> {
    let mut stmt = conn.prepare(
        "
        SELECT from_type, from_id, to_type, to_id, kind, weight, source_path
        FROM edges
        ORDER BY from_type, from_id, kind, to_type, to_id
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(GraphEdgeRecord {
            from_type: row.get(0)?,
            from_id: row.get(1)?,
            to_type: row.get(2)?,
            to_id: row.get(3)?,
            kind: row.get(4)?,
            weight: row.get(5)?,
            source_path: row.get(6)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn graph_map_node(
    conn: &Connection,
    node_type: &str,
    id: &str,
    distance: usize,
) -> Result<Option<MapNode>> {
    match node_type {
        "doc" => Ok(get_doc(conn, id)?.map(|doc| doc_map_node(conn, &doc, distance))),
        "concept" => Ok(get_concept(conn, id)?.map(|concept| {
            let backing_doc = concept
                .defined_in_path
                .as_deref()
                .and_then(|path| get_doc(conn, path).ok().flatten());
            concept_map_node(conn, &concept, backing_doc.as_ref(), distance)
        })),
        _ => Ok(None),
    }
}

fn doc_map_node(conn: &Connection, doc: &DocRecord, distance: usize) -> MapNode {
    MapNode {
        id: doc.path.clone(),
        kind: "doc".to_string(),
        subkind: doc.content_class.clone(),
        path: doc.path.clone(),
        title: doc.title.clone(),
        summary_from_source: doc.summary_raw.clone(),
        version: doc.version.clone(),
        api_surface: doc.api_surface.clone(),
        doc_type: doc.doc_type.clone(),
        reading_time_min: doc.reading_time_min,
        staleness: staleness_for_doc(conn, doc).unwrap_or_else(|_| staleness(doc)),
        distance_from_center: distance,
    }
}

fn concept_map_node(
    conn: &Connection,
    concept: &ConceptRecord,
    backing_doc: Option<&DocRecord>,
    distance: usize,
) -> MapNode {
    let fallback_verified_at = now_iso();
    let fallback_staleness = || Staleness {
        age_days: 0,
        freshness: "fresh".to_string(),
        content_verified_at: fallback_verified_at.clone(),
        schema_version: concept.version.clone(),
        references_deprecated: concept.deprecated,
        deprecated_refs: Vec::new(),
        upcoming_changes: scheduled_changes_for_concept(conn, concept).unwrap_or_default(),
    };
    MapNode {
        id: concept.id.clone(),
        kind: "concept".to_string(),
        subkind: concept.kind.clone(),
        path: concept
            .defined_in_path
            .clone()
            .unwrap_or_else(|| concept.id.clone()),
        title: concept.name.clone(),
        summary_from_source: backing_doc
            .map(|doc| doc.summary_raw.clone())
            .or_else(|| concept.kind_metadata.clone())
            .unwrap_or_default()
            .chars()
            .take(400)
            .collect(),
        version: concept.version.clone(),
        api_surface: Some("admin_graphql".to_string()),
        doc_type: "concept".to_string(),
        reading_time_min: backing_doc.and_then(|doc| doc.reading_time_min),
        staleness: backing_doc
            .map(|doc| {
                let mut staleness = staleness_for_doc(conn, doc).unwrap_or_else(|_| staleness(doc));
                let concept_changes =
                    scheduled_changes_for_concept(conn, concept).unwrap_or_default();
                if !concept_changes.is_empty() {
                    staleness.upcoming_changes =
                        merge_json_arrays(staleness.upcoming_changes, concept_changes);
                    staleness.references_deprecated = true;
                }
                staleness
            })
            .unwrap_or_else(fallback_staleness),
        distance_from_center: distance,
    }
}

fn center_for_key(
    conn: &Connection,
    key: Option<&GraphNodeKey>,
    fallback: &str,
) -> Result<MapCenter> {
    let Some(key) = key else {
        return Ok(MapCenter {
            id: fallback.to_string(),
            kind: "doc".to_string(),
            path: None,
            title: fallback.to_string(),
        });
    };
    if key.node_type == "concept" {
        if let Some(concept) = get_concept(conn, &key.id)? {
            return Ok(MapCenter {
                id: concept.id,
                kind: "concept".to_string(),
                path: concept.defined_in_path,
                title: concept.name,
            });
        }
    }
    if let Some(doc) = get_doc(conn, &key.id)? {
        return Ok(MapCenter {
            id: doc.path.clone(),
            kind: "doc".to_string(),
            path: Some(doc.path),
            title: doc.title,
        });
    }
    Ok(MapCenter {
        id: fallback.to_string(),
        kind: key.node_type.clone(),
        path: None,
        title: fallback.to_string(),
    })
}

fn graph_query_plan(paths: &[String], lens: &str, radius: usize) -> Vec<QueryPlanStep> {
    paths
        .iter()
        .take(3)
        .enumerate()
        .map(|(i, path)| QueryPlanStep {
            step: i + 1,
            action: "fetch".to_string(),
            path: Some(path.clone()),
            reason: if i == 0 {
                format!("Primary graph source for lens={lens}, radius={radius}; fetch raw source before answering.")
            } else {
                "Related graph source to compare against the primary source.".to_string()
            },
        })
        .collect()
}

fn node_kind_rank(kind: &str) -> usize {
    match kind {
        "doc" => 0,
        "concept" => 1,
        _ => 2,
    }
}

fn doc_type_rank(doc_type: &str) -> usize {
    match doc_type {
        "reference" => 0,
        "how-to" => 1,
        "tutorial" => 2,
        "explanation" => 3,
        "migration" => 4,
        _ => 9,
    }
}

pub(crate) fn status(paths: &Paths) -> Result<StatusResponse> {
    let mut warnings = Vec::new();
    if !paths.db.exists() {
        warnings.push("Index not built. Run `shopify-rextant build` first.".to_string());
        return Ok(StatusResponse {
            schema_version: SCHEMA_VERSION.to_string(),
            data_dir: paths.data.display().to_string(),
            index_built: false,
            doc_count: 0,
            last_full_build: None,
            index: GraphIndexStatus {
                concept_count: 0,
                edge_count: 0,
                graph_snapshot: false,
            },
            coverage: CoverageStatus::empty(),
            freshness: FreshnessStatus::empty(),
            workers: WorkerStatus::empty(),
            changelog: ChangelogStatus::empty(),
            warnings,
        });
    }

    let conn = open_db(paths)?;
    init_db(&conn)?;
    let doc_count = count_docs(&conn)?;
    let last_full_build = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key='last_full_build'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let schema_version = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_else(|| "unknown".to_string());
    let coverage = coverage_status(&conn)?;
    let freshness = freshness_status(&conn)?;
    let workers = worker_status(&conn)?;
    let changelog = changelog_status(&conn)?;
    if !paths.tantivy.join("meta.json").exists() {
        warnings.push("Tantivy index missing; run `shopify-rextant build`.".to_string());
    }
    if coverage.failed_count > 0 {
        warnings.push(format!(
            "{} discovered Shopify docs failed during the last build.",
            coverage.failed_count
        ));
    }
    if coverage.skipped_count > 0 {
        warnings.push(format!(
            "{} discovered Shopify docs were skipped because raw markdown was unavailable.",
            coverage.skipped_count
        ));
    }
    let index = graph_index_status(paths, &conn)?;
    if doc_count > 0 && index.edge_count == 0 {
        warnings.push(
            "Graph coverage is unavailable; shopify_map will use v0.1 FTS fallback.".to_string(),
        );
    }
    let pending_version_rebuilds =
        count_where(&conn, "version_rebuild_queue", "status = 'pending'").unwrap_or(0);
    if pending_version_rebuilds > 0 {
        warnings.push(format!(
            "{pending_version_rebuilds} API version rebuild request(s) are pending."
        ));
    }
    if let Some(warning) = get_meta(&conn, "last_version_check_warning")? {
        warnings.push(format!("Version watcher warning: {warning}"));
    }
    if let Some(warning) = &changelog.last_warning {
        warnings.push(format!("Changelog polling warning: {warning}"));
    }
    Ok(StatusResponse {
        schema_version,
        data_dir: paths.data.display().to_string(),
        index_built: doc_count > 0,
        doc_count,
        last_full_build,
        index,
        coverage,
        freshness,
        workers,
        changelog,
        warnings,
    })
}

fn count_docs(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM docs", [], |row| row.get(0))
        .map_err(Into::into)
}

fn graph_index_status(paths: &Paths, conn: &Connection) -> Result<GraphIndexStatus> {
    Ok(GraphIndexStatus {
        concept_count: count_where(conn, "concepts", "1=1").unwrap_or(0),
        edge_count: count_where(conn, "edges", "1=1").unwrap_or(0),
        graph_snapshot: paths.data.join("graph.msgpack").exists(),
    })
}

fn coverage_status(conn: &Connection) -> Result<CoverageStatus> {
    let mut status = CoverageStatus::empty();
    status.discovered_count = count_where(conn, "coverage_reports", "1=1")?;
    status.indexed_count = count_where(conn, "coverage_reports", "status = 'indexed'")?;
    status.skipped_count = count_where(conn, "coverage_reports", "status = 'skipped'")?;
    status.failed_count = count_where(conn, "coverage_reports", "status = 'failed'")?;
    status.classified_unknown_count =
        count_where(conn, "coverage_reports", "status = 'classified_unknown'")?;
    status.sources = CoverageSources {
        llms: count_where(conn, "coverage_reports", "source = 'llms'")?,
        sitemap: count_where(conn, "coverage_reports", "source = 'sitemap'")?,
        on_demand: count_where(conn, "coverage_reports", "source = 'on_demand'")?,
        manual: count_where(conn, "coverage_reports", "source = 'manual'")?,
    };
    status.last_sitemap_at = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key='last_sitemap_at'",
            [],
            |row| row.get(0),
        )
        .optional()?
        .or_else(|| {
            conn.query_row(
                "SELECT MAX(checked_at) FROM coverage_reports WHERE source='sitemap'",
                [],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten()
        });
    if status.discovered_count == 0 {
        let doc_count = count_docs(conn)?;
        status.discovered_count = doc_count;
        status.indexed_count = doc_count;
        status.sources.llms = count_where(conn, "docs", "source = 'llms'")?;
        status.sources.sitemap = count_where(conn, "docs", "source = 'sitemap'")?;
        status.sources.on_demand = count_where(conn, "docs", "source = 'on_demand'")?;
        status.sources.manual = count_where(conn, "docs", "source = 'manual'")?;
    }
    Ok(status)
}

fn freshness_status(conn: &Connection) -> Result<FreshnessStatus> {
    Ok(FreshnessStatus {
        fresh_count: count_where(conn, "docs", "freshness = 'fresh'")?,
        aging_count: count_where(conn, "docs", "freshness = 'aging'")?,
        stale_count: count_where(conn, "docs", "freshness = 'stale'")?,
    })
}

fn worker_status(conn: &Connection) -> Result<WorkerStatus> {
    Ok(WorkerStatus {
        last_changelog_at: get_meta(conn, "last_changelog_at")?,
        last_aging_sweep_at: get_meta(conn, "last_aging_sweep_at")?,
        last_version_check_at: get_meta(conn, "last_version_check_at")?,
    })
}

fn changelog_status(conn: &Connection) -> Result<ChangelogStatus> {
    let mut unresolved_ref_count = 0;
    let mut stmt = conn.prepare("SELECT unresolved_affected_refs FROM changelog_entries")?;
    let rows = stmt.query_map([], |row| row.get::<_, Option<String>>(0))?;
    for row in rows {
        unresolved_ref_count += parse_json_string_vec(row?.as_deref()).len() as i64;
    }
    Ok(ChangelogStatus {
        entry_count: count_where(conn, "changelog_entries", "1=1")?,
        scheduled_change_count: count_where(conn, "scheduled_changes", "1=1")?,
        unresolved_ref_count,
        last_warning: get_meta(conn, "last_changelog_warning")?,
    })
}

fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM schema_meta WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO schema_meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn refresh_indexed_versions(conn: &Connection, docs: &[DocRecord]) -> Result<()> {
    let mut counts = HashMap::<String, i64>::new();
    for doc in docs {
        if doc.api_surface.as_deref() == Some("admin_graphql") {
            if let Some(version) = &doc.version {
                *counts.entry(version.clone()).or_insert(0) += 1;
            }
        }
    }
    conn.execute("DELETE FROM indexed_versions", [])?;
    for (version, doc_count) in counts {
        conn.execute(
            "
            INSERT INTO indexed_versions(version, api_surface, indexed_at, doc_count)
            VALUES(?1, 'admin_graphql', ?2, ?3)
            ON CONFLICT(version) DO UPDATE SET
              api_surface=excluded.api_surface,
              indexed_at=excluded.indexed_at,
              doc_count=excluded.doc_count
            ",
            params![version, now_iso(), doc_count],
        )?;
    }
    Ok(())
}

fn indexed_version_exists(conn: &Connection, version: &str, api_surface: &str) -> Result<bool> {
    conn.query_row(
        "
        SELECT 1 FROM indexed_versions
        WHERE version = ?1 AND api_surface = ?2
        ",
        params![version, api_surface],
        |_| Ok(()),
    )
    .optional()
    .map(|value| value.is_some())
    .map_err(Into::into)
}

fn enqueue_version_rebuild(
    conn: &Connection,
    version: &str,
    api_surface: &str,
    reason: &str,
) -> Result<()> {
    conn.execute(
        "
        INSERT INTO version_rebuild_queue(version, api_surface, status, reason, enqueued_at)
        VALUES(?1, ?2, 'pending', ?3, ?4)
        ON CONFLICT(version, api_surface) DO UPDATE SET
          status='pending',
          reason=excluded.reason,
          enqueued_at=excluded.enqueued_at
        ",
        params![version, api_surface, reason, now_iso()],
    )?;
    Ok(())
}

fn update_doc_freshness_states(conn: &Connection) -> Result<()> {
    let docs = all_docs(conn)?;
    for doc in docs {
        let computed = staleness(&doc).freshness;
        if computed != doc.freshness {
            conn.execute(
                "UPDATE docs SET freshness = ?1 WHERE path = ?2",
                params![computed, doc.path],
            )?;
        }
    }
    Ok(())
}

fn stale_refresh_candidates(conn: &Connection, limit: usize) -> Result<Vec<DocRecord>> {
    let mut stmt = conn.prepare(
        "SELECT path, title, url, version, doc_type, api_surface, content_class, content_sha,
                last_verified, last_changed, freshness, references_deprecated, deprecated_refs,
                summary_raw, reading_time_min, raw_path, source
         FROM docs
         WHERE freshness IN ('aging', 'stale')
         ORDER BY CASE freshness WHEN 'stale' THEN 0 ELSE 1 END, last_verified
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], doc_from_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn all_docs(conn: &Connection) -> Result<Vec<DocRecord>> {
    let mut stmt = conn.prepare(
        "SELECT path, title, url, version, doc_type, api_surface, content_class, content_sha,
                last_verified, last_changed, freshness, references_deprecated, deprecated_refs,
                summary_raw, reading_time_min, raw_path, source
         FROM docs",
    )?;
    let rows = stmt.query_map([], doc_from_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn count_where(conn: &Connection, table: &str, where_clause: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {where_clause}");
    conn.query_row(&sql, [], |row| row.get(0))
        .map_err(Into::into)
}

fn versions_available(paths: &Paths) -> Result<Vec<String>> {
    if !paths.db.exists() {
        return Ok(Vec::new());
    }
    let conn = open_db(paths)?;
    init_db(&conn)?;
    let mut stmt = conn.prepare(
        "SELECT DISTINCT COALESCE(version, 'evergreen') FROM docs ORDER BY COALESCE(version, 'evergreen')",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn index_age_days(last_full_build: Option<&str>) -> i64 {
    last_full_build
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|dt| (Utc::now() - dt.with_timezone(&Utc)).num_days())
        .unwrap_or(0)
}

fn map_coverage_warning(status: &StatusResponse) -> Option<String> {
    if status.doc_count == 0 {
        Some("Index is empty; run shopify-rextant build.".to_string())
    } else if status.coverage.failed_count > 0 || status.coverage.skipped_count > 0 {
        Some(format!(
            "Coverage has {} skipped and {} failed URLs from the last build.",
            status.coverage.skipped_count, status.coverage.failed_count
        ))
    } else if index_age_days(status.last_full_build.as_deref()) > 14 {
        Some("Index is older than 14 days; run shopify-rextant refresh.".to_string())
    } else {
        None
    }
}

fn dedupe_docs_by_path(docs: Vec<DocRecord>) -> Vec<DocRecord> {
    let mut seen = HashSet::new();
    docs.into_iter()
        .filter(|doc| seen.insert(doc.path.clone()))
        .collect()
}

pub(crate) fn search_docs(
    paths: &Paths,
    query: &str,
    version: Option<&str>,
    limit: usize,
) -> Result<Vec<DocRecord>> {
    search_docs_with_runtime(paths, None, query, version, limit)
}

fn search_docs_with_runtime(
    paths: &Paths,
    runtime: Option<&SearchRuntime>,
    query: &str,
    version: Option<&str>,
    limit: usize,
) -> Result<Vec<DocRecord>> {
    let conn = open_db(paths)?;
    if let Some(runtime) = runtime {
        return runtime.search(&conn, query, version, limit);
    }
    if !paths.tantivy.join("meta.json").exists() {
        return sqlite_like_search(&conn, query, version, limit);
    }
    let Some(runtime) = SearchRuntime::open(paths)? else {
        return sqlite_like_search(&conn, query, version, limit);
    };
    runtime.search(&conn, query, version, limit)
}

struct SearchRuntime {
    index: Index,
    fields: SearchFields,
    reader: tantivy::IndexReader,
}

impl SearchRuntime {
    fn open(paths: &Paths) -> Result<Option<Self>> {
        if !paths.tantivy.join("meta.json").exists() {
            return Ok(None);
        }
        let index = Index::open_in_dir(&paths.tantivy)?;
        let fields = match SearchFields::from_schema(&index.schema()) {
            Ok(fields) => fields,
            Err(_) => return Ok(None),
        };
        let reader = index.reader()?;
        Ok(Some(Self {
            index,
            fields,
            reader,
        }))
    }

    fn search(
        &self,
        conn: &Connection,
        query: &str,
        version: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DocRecord>> {
        let searcher = self.reader.searcher();
        let mut query_fields = vec![self.fields.title, self.fields.path];
        if query_needs_japanese_tokenizer(query) {
            register_japanese_tokenizer(&self.index)?;
            query_fields.extend(self.fields.content_fields());
        } else {
            query_fields.push(self.fields.content_en);
        }
        let parser = QueryParser::for_index(&self.index, query_fields);
        let parsed = parser
            .parse_query(query)
            .or_else(|_| parser.parse_query(&escape_query(query)))?;
        let top_docs = searcher.search(&parsed, &TopDocs::with_limit(limit))?;
        let schema = self.index.schema();
        let mut records = Vec::new();
        for (_score, address) in top_docs {
            let retrieved = searcher.doc::<TantivyDocument>(address)?;
            let Some(path) = doc_json_field(&retrieved.to_json(&schema), "path") else {
                continue;
            };
            if let Some(record) = get_doc(conn, &path)? {
                if version.is_none_or(|v| {
                    record.version.is_none() || record.version.as_deref() == Some(v)
                }) {
                    records.push(record);
                }
            }
        }
        Ok(records)
    }
}

fn query_needs_japanese_tokenizer(query: &str) -> bool {
    query.chars().any(|ch| !ch.is_ascii())
}

fn sqlite_like_search(
    conn: &Connection,
    query: &str,
    version: Option<&str>,
    limit: usize,
) -> Result<Vec<DocRecord>> {
    let like = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
    let mut stmt = conn.prepare(
        "SELECT path, title, url, version, doc_type, api_surface, content_class, content_sha,
                last_verified, last_changed, freshness, references_deprecated, deprecated_refs,
                summary_raw, reading_time_min, raw_path, source
         FROM docs
         WHERE (title LIKE ?1 ESCAPE '\\' OR path LIKE ?1 ESCAPE '\\' OR summary_raw LIKE ?1 ESCAPE '\\')
           AND (?2 IS NULL OR version IS NULL OR version = ?2)
         ORDER BY hit_count DESC, title
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![like, version, limit as i64], doc_from_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn rebuild_tantivy_from_db(paths: &Paths) -> Result<()> {
    let schema = search_schema();
    let index = create_or_reset_index(paths, schema.clone(), true)?;
    register_japanese_tokenizer(&index)?;
    let fields = SearchFields::from_schema(&schema)?;
    let conn = open_db(paths)?;
    let mut stmt = conn.prepare(
        "SELECT path, title, url, version, doc_type, api_surface, content_class, content_sha,
                last_verified, last_changed, freshness, references_deprecated, deprecated_refs,
                summary_raw, reading_time_min, raw_path, source
         FROM docs",
    )?;
    let docs = stmt.query_map([], doc_from_row)?;
    let mut writer = index.writer(50_000_000)?;
    for doc in docs {
        let doc = doc?;
        let content = fs::read_to_string(paths.raw_file(&doc.raw_path)).unwrap_or_default();
        add_tantivy_doc(&mut writer, fields, &doc, &content)?;
    }
    writer.commit()?;
    Ok(())
}

fn upsert_tantivy_doc(paths: &Paths, record: &DocRecord, content: &str) -> Result<()> {
    let schema = search_schema();
    let index = create_or_reset_index(paths, schema.clone(), false)?;
    register_japanese_tokenizer(&index)?;
    let fields = match SearchFields::from_schema(&index.schema()) {
        Ok(fields) => fields,
        Err(_) => {
            rebuild_tantivy_from_db(paths)?;
            return Ok(());
        }
    };
    let mut writer = index.writer(50_000_000)?;
    writer.delete_term(Term::from_field_text(fields.path, &record.path));
    add_tantivy_doc(&mut writer, fields, record, content)?;
    writer.commit()?;
    Ok(())
}

fn add_tantivy_doc(
    writer: &mut tantivy::IndexWriter<TantivyDocument>,
    fields: SearchFields,
    record: &DocRecord,
    content: &str,
) -> Result<()> {
    writer.add_document(doc!(
        fields.path => record.path.clone(),
        fields.title => record.title.clone(),
        fields.url => record.url.clone(),
        fields.version => record.version.clone().unwrap_or_else(|| "evergreen".to_string()),
        fields.api_surface => record.api_surface.clone().unwrap_or_else(|| "unknown".to_string()),
        fields.doc_type => record.doc_type.clone(),
        fields.content_en => content.chars().take(4_000).collect::<String>(),
        fields.content_ja => content.chars().take(4_000).collect::<String>(),
    ))?;
    Ok(())
}

#[derive(Debug)]
struct SourceDoc {
    url: String,
    title_hint: Option<String>,
    content: String,
    source: String,
}

#[derive(Debug)]
pub(crate) struct SourceFetchError {
    pub(crate) status: String,
    pub(crate) reason: String,
    pub(crate) http_status: Option<u16>,
}

#[derive(Debug, Clone, Serialize)]
struct ConceptRecord {
    id: String,
    kind: String,
    name: String,
    version: Option<String>,
    defined_in_path: Option<String>,
    deprecated: bool,
    deprecated_since: Option<String>,
    deprecation_reason: Option<String>,
    replaced_by: Option<String>,
    kind_metadata: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct GraphEdgeRecord {
    from_type: String,
    from_id: String,
    to_type: String,
    to_id: String,
    kind: String,
    weight: f64,
    source_path: Option<String>,
}

#[derive(Debug, Default, Serialize)]
struct GraphBuild {
    concepts: Vec<ConceptRecord>,
    edges: Vec<GraphEdgeRecord>,
}

#[derive(Debug, Clone)]
struct GraphNodeKey {
    node_type: String,
    id: String,
}

#[derive(Debug)]
struct GraphExpansion {
    nodes: Vec<MapNode>,
    edges: Vec<Value>,
    suggested_reading_order: Vec<String>,
}

#[derive(Debug)]
struct CoverageEvent {
    source: String,
    canonical_path: Option<String>,
    source_url: String,
    status: String,
    reason: Option<String>,
    http_status: Option<u16>,
    checked_at: String,
}

#[derive(Debug, Clone)]
struct ChangelogEntryInput {
    id: String,
    title: String,
    link: String,
    body: String,
    posted_at: String,
    categories: Vec<String>,
}

#[derive(Debug, Clone)]
struct ResolvedImpact {
    refs: Vec<String>,
    doc_paths: Vec<String>,
    concept_ids: Vec<String>,
    surfaces: Vec<String>,
    unresolved_refs: Vec<String>,
}

#[derive(Debug, Clone)]
struct ScheduledChangeRecord {
    id: String,
    type_name: String,
    change: String,
    effective_date: Option<String>,
    migration_hint: Option<String>,
    source_changelog_id: String,
}

#[derive(Debug, Default)]
struct ChangelogPollReport {
    entries_seen: usize,
    entries_inserted: usize,
    scheduled_changes: usize,
    warnings: Vec<String>,
}

#[derive(Debug, Default)]
struct VersionCheckReport {
    latest_candidate: Option<String>,
    already_indexed: bool,
    enqueued: bool,
    warning: Option<String>,
}

impl CoverageEvent {
    fn indexed(link: &MarkdownLink) -> Self {
        Self {
            source: link.source.clone(),
            canonical_path: canonical_doc_path(&link.url).ok(),
            source_url: link.url.clone(),
            status: "indexed".to_string(),
            reason: None,
            http_status: None,
            checked_at: now_iso(),
        }
    }

    fn from_fetch_error(link: &MarkdownLink, error: SourceFetchError) -> Self {
        Self {
            source: link.source.clone(),
            canonical_path: canonical_doc_path(&link.url).ok(),
            source_url: link.url.clone(),
            status: error.status,
            reason: Some(error.reason),
            http_status: error.http_status,
            checked_at: now_iso(),
        }
    }
}

impl CoverageStatus {
    fn empty() -> Self {
        Self {
            last_sitemap_at: None,
            discovered_count: 0,
            indexed_count: 0,
            skipped_count: 0,
            failed_count: 0,
            classified_unknown_count: 0,
            sources: CoverageSources {
                llms: 0,
                sitemap: 0,
                on_demand: 0,
                manual: 0,
            },
        }
    }
}

impl FreshnessStatus {
    fn empty() -> Self {
        Self {
            fresh_count: 0,
            aging_count: 0,
            stale_count: 0,
        }
    }
}

impl WorkerStatus {
    fn empty() -> Self {
        Self {
            last_changelog_at: None,
            last_aging_sweep_at: None,
            last_version_check_at: None,
        }
    }
}

impl ChangelogStatus {
    fn empty() -> Self {
        Self {
            entry_count: 0,
            scheduled_change_count: 0,
            unresolved_ref_count: 0,
            last_warning: None,
        }
    }
}

async fn fetch_source_doc<S: TextSource>(
    source: &S,
    link: &MarkdownLink,
) -> std::result::Result<SourceDoc, SourceFetchError> {
    let candidates = raw_doc_candidates(&link.url).map_err(|e| SourceFetchError {
        status: "failed".to_string(),
        reason: format!("invalid_url: {e}"),
        http_status: None,
    })?;
    let mut last_status = None;
    for url in candidates {
        match source.fetch_text(&url).await {
            Ok(content) => {
                return std::result::Result::Ok(SourceDoc {
                    url,
                    title_hint: Some(link.title.clone()),
                    content,
                    source: link.source.clone(),
                });
            }
            Err(error) if error.status == "skipped" => {
                last_status = error.http_status;
            }
            Err(error) => return Err(error),
        }
    }
    Err(SourceFetchError {
        status: "skipped".to_string(),
        reason: "markdown_not_found".to_string(),
        http_status: last_status,
    })
}

async fn fetch_required_text<S: TextSource>(source: &S, url: &str) -> Result<String> {
    source.fetch_text(url).await.map_err(|error| {
        anyhow!(
            "GET {url} failed: {} (status={}, http_status={:?})",
            error.reason,
            error.status,
            error.http_status
        )
    })
}

async fn poll_changelog_from_source<S: TextSource>(
    paths: &Paths,
    feed_url: &str,
    source: &S,
) -> Result<ChangelogPollReport> {
    let conn = open_db(paths)?;
    init_db(&conn)?;
    let feed = match source.fetch_text(feed_url).await {
        Ok(feed) => feed,
        Err(error) => {
            let warning = format!("GET {feed_url} failed: {}", error.reason);
            set_meta(&conn, "last_changelog_warning", &warning)?;
            return Ok(ChangelogPollReport {
                warnings: vec![warning],
                ..Default::default()
            });
        }
    };
    let entries = match parse_changelog_feed(&feed) {
        Ok(entries) => entries,
        Err(error) => {
            let warning = format!("parse changelog feed failed: {error}");
            set_meta(&conn, "last_changelog_warning", &warning)?;
            return Ok(ChangelogPollReport {
                warnings: vec![warning],
                ..Default::default()
            });
        }
    };
    let mut report = ChangelogPollReport {
        entries_seen: entries.len(),
        ..Default::default()
    };
    for entry in entries {
        if changelog_entry_exists(&conn, &entry.id)? {
            continue;
        }
        let impact = resolve_changelog_impact(&conn, &entry)?;
        let scheduled_changes = scheduled_changes_from_entry(&entry, &impact);
        insert_changelog_entry(&conn, &entry, &impact)?;
        for change in &scheduled_changes {
            insert_scheduled_change(&conn, change)?;
        }
        if !scheduled_changes.is_empty() {
            mark_docs_deprecated(&conn, &impact.doc_paths, &impact.refs)?;
        }
        report.entries_inserted += 1;
        report.scheduled_changes += scheduled_changes.len();
    }
    set_meta(&conn, "last_changelog_at", &now_iso())?;
    conn.execute(
        "DELETE FROM schema_meta WHERE key = 'last_changelog_warning'",
        [],
    )?;
    Ok(report)
}

async fn check_new_versions_from_source<S: TextSource>(
    paths: &Paths,
    source_urls: &IndexSourceUrls,
    source: &S,
) -> Result<VersionCheckReport> {
    let conn = open_db(paths)?;
    init_db(&conn)?;
    let mut report = VersionCheckReport::default();
    let versioning_page = match source.fetch_text(&source_urls.versioning).await {
        Ok(page) => page,
        Err(error) => {
            let warning = format!("GET {} failed: {}", source_urls.versioning, error.reason);
            set_meta(&conn, "last_version_check_at", &now_iso())?;
            set_meta(&conn, "last_version_check_warning", &warning)?;
            report.warning = Some(warning);
            return Ok(report);
        }
    };
    let candidates = version_candidates_desc(&versioning_page);
    if candidates.is_empty() {
        let warning = "no API version candidates found in versioning page".to_string();
        set_meta(&conn, "last_version_check_at", &now_iso())?;
        set_meta(&conn, "last_version_check_warning", &warning)?;
        report.warning = Some(warning);
        return Ok(report);
    };

    for candidate in candidates {
        if indexed_version_exists(&conn, &candidate, "admin_graphql")? {
            report.latest_candidate = Some(candidate);
            report.already_indexed = true;
            set_meta(&conn, "last_version_check_at", &now_iso())?;
            conn.execute(
                "DELETE FROM schema_meta WHERE key = 'last_version_check_warning'",
                [],
            )?;
            return Ok(report);
        }
        if validate_admin_graphql_version(source, &candidate).await? {
            enqueue_version_rebuild(
                &conn,
                &candidate,
                "admin_graphql",
                "latest validated Admin GraphQL version is not indexed",
            )?;
            report.latest_candidate = Some(candidate);
            report.enqueued = true;
            set_meta(&conn, "last_version_check_at", &now_iso())?;
            conn.execute(
                "DELETE FROM schema_meta WHERE key = 'last_version_check_warning'",
                [],
            )?;
            return Ok(report);
        }
    }

    let warning = "no API version candidates passed Admin GraphQL validation".to_string();
    set_meta(&conn, "last_version_check_warning", &warning)?;
    report.warning = Some(warning);
    set_meta(&conn, "last_version_check_at", &now_iso())?;
    Ok(report)
}

fn version_candidates_desc(page: &str) -> Vec<String> {
    let re = Regex::new(r"\b20\d{2}-\d{2}\b").expect("valid API version regex");
    re.find_iter(page)
        .map(|m| m.as_str().to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .rev()
        .collect()
}

async fn validate_admin_graphql_version<S: TextSource>(source: &S, version: &str) -> Result<bool> {
    let url = admin_graphql_direct_proxy_url(version);
    let Ok(snapshot) = source.fetch_admin_graphql_introspection(&url).await else {
        return Ok(false);
    };
    let value: Value = serde_json::from_str(&snapshot)
        .with_context(|| format!("parse Admin GraphQL introspection for {version}"))?;
    Ok(value
        .pointer("/data/__schema/types")
        .and_then(Value::as_array)
        .is_some_and(|types| !types.is_empty()))
}

fn parse_changelog_feed(xml: &str) -> Result<Vec<ChangelogEntryInput>> {
    let feed = parser::parse(xml.as_bytes()).context("parse RSS/Atom changelog feed")?;
    Ok(feed
        .entries
        .into_iter()
        .map(|entry| {
            let link = entry
                .links
                .first()
                .map(|link| link.href.clone())
                .unwrap_or_else(|| entry.id.clone());
            let id = if entry.id.trim().is_empty() {
                link.clone()
            } else {
                entry.id
            };
            let title = entry
                .title
                .map(|title| title.content)
                .unwrap_or_else(|| id.clone());
            let body = entry
                .content
                .and_then(|content| content.body)
                .or_else(|| entry.summary.map(|summary| summary.content))
                .unwrap_or_default();
            let posted_at = entry
                .published
                .or(entry.updated)
                .unwrap_or_else(Utc::now)
                .to_rfc3339();
            let categories = entry
                .categories
                .into_iter()
                .flat_map(|category| [Some(category.term), category.label].into_iter().flatten())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            ChangelogEntryInput {
                id,
                title,
                link,
                body,
                posted_at,
                categories,
            }
        })
        .collect())
}

fn changelog_entry_exists(conn: &Connection, id: &str) -> Result<bool> {
    conn.query_row(
        "SELECT 1 FROM changelog_entries WHERE id = ?1",
        params![id],
        |_| Ok(()),
    )
    .optional()
    .map(|value| value.is_some())
    .map_err(Into::into)
}

fn resolve_changelog_impact(
    conn: &Connection,
    entry: &ChangelogEntryInput,
) -> Result<ResolvedImpact> {
    let candidates = extract_impact_candidates(entry);
    let version_hint = candidates
        .iter()
        .find(|candidate| is_api_version(candidate))
        .cloned();
    let all_edges = load_edges(conn)?;
    let mut refs = BTreeSet::new();
    let mut doc_paths = BTreeSet::new();
    let mut concept_ids = BTreeSet::new();
    let mut surfaces = BTreeSet::new();
    let mut unresolved_refs = BTreeSet::new();

    for category in &entry.categories {
        if let Some(surface) = surface_from_category(category) {
            surfaces.insert(surface);
        }
    }

    for candidate in candidates {
        if is_api_version(&candidate) || surface_from_category(&candidate).is_some() {
            continue;
        }
        if let Some(path) = candidate_to_doc_path(&candidate)
            .filter(|path| get_doc(conn, path).ok().flatten().is_some())
        {
            refs.insert(path.clone());
            doc_paths.insert(path.clone());
            collect_graph_neighbors(
                conn,
                &all_edges,
                "doc",
                &path,
                &mut refs,
                &mut doc_paths,
                &mut concept_ids,
                &mut surfaces,
            )?;
            continue;
        }
        let concept = match version_hint.as_deref() {
            Some(version) => find_concept_by_name(conn, &candidate, Some(version))?
                .or_else(|| find_concept_by_name(conn, &candidate, None).ok().flatten()),
            None => find_concept_by_name(conn, &candidate, None)?,
        };
        if let Some(concept) = concept {
            refs.insert(concept.name.clone());
            concept_ids.insert(concept.id.clone());
            if let Some(path) = &concept.defined_in_path {
                doc_paths.insert(path.clone());
            }
            surfaces.insert("admin_graphql".to_string());
            collect_graph_neighbors(
                conn,
                &all_edges,
                "concept",
                &concept.id,
                &mut refs,
                &mut doc_paths,
                &mut concept_ids,
                &mut surfaces,
            )?;
            continue;
        }
        if looks_like_reference_candidate(&candidate) {
            unresolved_refs.insert(candidate);
        }
    }

    Ok(ResolvedImpact {
        refs: refs.into_iter().collect(),
        doc_paths: doc_paths.into_iter().collect(),
        concept_ids: concept_ids.into_iter().collect(),
        surfaces: surfaces.into_iter().collect(),
        unresolved_refs: unresolved_refs.into_iter().collect(),
    })
}

fn extract_impact_candidates(entry: &ChangelogEntryInput) -> Vec<String> {
    let mut candidates = BTreeSet::new();
    let text = format!(
        "{}\n{}\n{}\n{}",
        entry.title,
        entry.body,
        entry.link,
        entry.categories.join("\n")
    );
    let doc_re = Regex::new(
        r#"https://shopify\.dev/(?:docs|changelog)/[^\s<>)"']+|/(?:docs|changelog)/[^\s<>)"']+"#,
    )
    .expect("valid changelog doc path regex");
    for caps in doc_re.captures_iter(&text) {
        candidates.insert(trim_candidate(caps.get(0).unwrap().as_str()));
    }
    let version_re = Regex::new(r"\b20\d{2}-\d{2}\b").expect("valid API version regex");
    for caps in version_re.captures_iter(&text) {
        candidates.insert(caps.get(0).unwrap().as_str().to_string());
    }
    let symbol_re = Regex::new(r"\b[A-Z][A-Za-z0-9]+(?:\.[A-Za-z_][A-Za-z0-9_]*)?\b")
        .expect("valid GraphQL symbol regex");
    for caps in symbol_re.captures_iter(&text) {
        candidates.insert(trim_candidate(caps.get(0).unwrap().as_str()));
    }
    candidates.into_iter().collect()
}

fn trim_candidate(value: &str) -> String {
    value
        .trim_matches(|ch: char| matches!(ch, '.' | ',' | ';' | ':' | ')' | '(' | '"' | '\''))
        .to_string()
}

fn is_api_version(value: &str) -> bool {
    Regex::new(r"^20\d{2}-\d{2}$")
        .expect("valid API version regex")
        .is_match(value)
}

fn candidate_to_doc_path(candidate: &str) -> Option<String> {
    if candidate.starts_with("/docs/") || candidate.starts_with("/changelog") {
        return Some(candidate.trim_end_matches('/').to_string());
    }
    if candidate.starts_with("https://shopify.dev/")
        || candidate.starts_with("https://www.shopify.dev/")
    {
        return canonical_doc_path(candidate).ok();
    }
    None
}

fn looks_like_reference_candidate(candidate: &str) -> bool {
    candidate.contains('.') || candidate.chars().next().is_some_and(char::is_uppercase)
}

fn surface_from_category(category: &str) -> Option<String> {
    let normalized = category.to_ascii_lowercase();
    if normalized.contains("admin graphql") || normalized.contains("graphql admin") {
        Some("admin_graphql".to_string())
    } else if normalized.contains("storefront") {
        Some("storefront".to_string())
    } else if normalized.contains("liquid") {
        Some("liquid".to_string())
    } else if normalized.contains("polaris") {
        Some("polaris".to_string())
    } else {
        None
    }
}

fn collect_graph_neighbors(
    conn: &Connection,
    edges: &[GraphEdgeRecord],
    start_type: &str,
    start_id: &str,
    refs: &mut BTreeSet<String>,
    doc_paths: &mut BTreeSet<String>,
    concept_ids: &mut BTreeSet<String>,
    surfaces: &mut BTreeSet<String>,
) -> Result<()> {
    let mut queue = VecDeque::from([(start_type.to_string(), start_id.to_string(), 0usize)]);
    let mut seen = HashSet::new();
    while let Some((node_type, node_id, distance)) = queue.pop_front() {
        if !seen.insert(format!("{node_type}:{node_id}")) || distance > 2 {
            continue;
        }
        match node_type.as_str() {
            "doc" => {
                if let Some(doc) = get_doc(conn, &node_id)? {
                    refs.insert(doc.path.clone());
                    doc_paths.insert(doc.path.clone());
                    if let Some(surface) = doc.api_surface {
                        surfaces.insert(surface);
                    }
                }
            }
            "concept" => {
                if let Some(concept) = get_concept(conn, &node_id)? {
                    refs.insert(concept.name.clone());
                    concept_ids.insert(concept.id.clone());
                    if let Some(path) = concept.defined_in_path {
                        doc_paths.insert(path);
                    }
                    surfaces.insert("admin_graphql".to_string());
                }
            }
            _ => {}
        }
        if distance == 2 {
            continue;
        }
        for edge in edges {
            let neighbor = if edge.from_type == node_type && edge.from_id == node_id {
                Some((edge.to_type.clone(), edge.to_id.clone()))
            } else if edge.to_type == node_type && edge.to_id == node_id {
                Some((edge.from_type.clone(), edge.from_id.clone()))
            } else {
                None
            };
            if let Some(neighbor) = neighbor {
                queue.push_back((neighbor.0, neighbor.1, distance + 1));
            }
        }
    }
    Ok(())
}

fn scheduled_changes_from_entry(
    entry: &ChangelogEntryInput,
    impact: &ResolvedImpact,
) -> Vec<ScheduledChangeRecord> {
    let change = classify_change(entry);
    if change.is_none() || impact.refs.is_empty() {
        return Vec::new();
    }
    let change = change.unwrap();
    let effective_date = extract_effective_date(entry);
    let migration_hint = extract_migration_hint(entry);
    impact
        .refs
        .iter()
        .map(|reference| {
            let id = hex_sha256(&format!(
                "{}:{}:{}:{}",
                entry.id,
                reference,
                change,
                effective_date.as_deref().unwrap_or("")
            ));
            ScheduledChangeRecord {
                id,
                type_name: reference.clone(),
                change: change.clone(),
                effective_date: effective_date.clone(),
                migration_hint: migration_hint.clone(),
                source_changelog_id: entry.id.clone(),
            }
        })
        .collect()
}

fn classify_change(entry: &ChangelogEntryInput) -> Option<String> {
    let text = format!("{}\n{}", entry.title, entry.body).to_ascii_lowercase();
    if text.contains("removed") || text.contains("removal") || text.contains("will be removed") {
        Some("removal".to_string())
    } else if text.contains("deprecated") || text.contains("deprecation") {
        Some("deprecation".to_string())
    } else {
        None
    }
}

fn extract_effective_date(entry: &ChangelogEntryInput) -> Option<String> {
    let text = format!("{}\n{}", entry.title, entry.body);
    Regex::new(r"\b20\d{2}-\d{2}\b")
        .expect("valid API version regex")
        .find(&text)
        .map(|m| m.as_str().to_string())
        .or_else(|| {
            Regex::new(r"\b20\d{2}-\d{2}-\d{2}\b")
                .expect("valid date regex")
                .find(&text)
                .map(|m| m.as_str().to_string())
        })
}

fn extract_migration_hint(entry: &ChangelogEntryInput) -> Option<String> {
    entry
        .body
        .lines()
        .find(|line| line.to_ascii_lowercase().contains("migrat"))
        .map(|line| line.trim().chars().take(300).collect::<String>())
}

fn insert_changelog_entry(
    conn: &Connection,
    entry: &ChangelogEntryInput,
    impact: &ResolvedImpact,
) -> Result<()> {
    let affected_types = impact_affected_types(impact);
    conn.execute(
        "
        INSERT INTO changelog_entries (
          id, title, url, posted_at, body, categories, affected_types,
          affected_surfaces, unresolved_affected_refs, processed_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ON CONFLICT(id) DO NOTHING
        ",
        params![
            entry.id,
            entry.title,
            entry.link,
            entry.posted_at,
            entry.body,
            serde_json::to_string(&entry.categories)?,
            serde_json::to_string(&affected_types)?,
            serde_json::to_string(&impact.surfaces)?,
            serde_json::to_string(&impact.unresolved_refs)?,
            now_iso(),
        ],
    )?;
    Ok(())
}

fn impact_affected_types(impact: &ResolvedImpact) -> Vec<String> {
    let mut affected = impact
        .refs
        .iter()
        .chain(impact.concept_ids.iter())
        .cloned()
        .collect::<Vec<_>>();
    affected.sort();
    affected.dedup();
    affected
}

fn insert_scheduled_change(conn: &Connection, change: &ScheduledChangeRecord) -> Result<()> {
    conn.execute(
        "
        INSERT INTO scheduled_changes (
          id, type_name, change, effective_date, migration_hint, source_changelog_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ON CONFLICT(id) DO NOTHING
        ",
        params![
            change.id,
            change.type_name,
            change.change,
            change.effective_date,
            change.migration_hint,
            change.source_changelog_id,
        ],
    )?;
    Ok(())
}

fn mark_docs_deprecated(conn: &Connection, doc_paths: &[String], refs: &[String]) -> Result<()> {
    for path in doc_paths {
        let existing = get_doc(conn, path)?;
        let mut merged = existing
            .as_ref()
            .map(|doc| doc.deprecated_refs.clone())
            .unwrap_or_default();
        merged.extend(refs.iter().cloned());
        merged.sort();
        merged.dedup();
        conn.execute(
            "
            UPDATE docs
            SET references_deprecated = 1,
                deprecated_refs = ?1
            WHERE path = ?2
            ",
            params![serde_json::to_string(&merged)?, path],
        )?;
    }
    Ok(())
}

async fn build_admin_graphql_graph<S: TextSource>(
    paths: &Paths,
    docs: &[SourceDoc],
    source: &S,
) -> Result<GraphBuild> {
    let doc_paths = docs
        .iter()
        .filter_map(|doc| canonical_doc_path(&doc.url).ok())
        .collect::<HashSet<_>>();
    let doc_contents = docs
        .iter()
        .filter_map(|doc| {
            canonical_doc_path(&doc.url)
                .ok()
                .map(|path| (path, doc.content.as_str()))
        })
        .collect::<HashMap<_, _>>();
    let versions = admin_graphql_versions(docs);
    let mut graph = GraphBuild::default();
    let mut concept_ids = HashSet::new();
    let mut edge_keys = HashSet::new();

    for version in versions {
        let url = admin_graphql_direct_proxy_url(&version);
        let Ok(snapshot) = source.fetch_admin_graphql_introspection(&url).await else {
            continue;
        };
        persist_schema_snapshot(paths, &version, &snapshot)?;
        let schema_json: Value = serde_json::from_str(&snapshot)
            .with_context(|| format!("parse Admin GraphQL introspection for {version}"))?;
        ingest_introspection_schema(
            &schema_json,
            &version,
            &doc_paths,
            &mut graph,
            &mut concept_ids,
            &mut edge_keys,
        )?;
    }

    add_doc_graph_edges(
        &doc_paths,
        &doc_contents,
        &concept_ids,
        &mut graph,
        &mut edge_keys,
    )?;
    Ok(graph)
}

fn admin_graphql_versions(docs: &[SourceDoc]) -> Vec<String> {
    docs.iter()
        .filter_map(|doc| canonical_doc_path(&doc.url).ok())
        .filter(|path| classify_api_surface(path).as_deref() == Some("admin_graphql"))
        .filter_map(|path| extract_version(&path))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn admin_graphql_direct_proxy_url(version: &str) -> String {
    format!("https://shopify.dev/admin-graphql-direct-proxy/{version}")
}

fn persist_schema_snapshot(paths: &Paths, version: &str, snapshot: &str) -> Result<()> {
    let schema_dir = paths.data.join("schemas/admin-graphql");
    fs::create_dir_all(&schema_dir)?;
    fs::write(
        schema_dir.join(format!("{version}.introspection.json")),
        snapshot,
    )?;
    Ok(())
}

fn persist_graph_snapshot(paths: &Paths, graph: &GraphBuild) -> Result<()> {
    fs::create_dir_all(&paths.data)?;
    fs::write(
        paths.data.join("graph.msgpack"),
        serde_json::to_vec(graph).context("serialize graph snapshot")?,
    )?;
    Ok(())
}

fn ingest_introspection_schema(
    schema_json: &Value,
    version: &str,
    doc_paths: &HashSet<String>,
    graph: &mut GraphBuild,
    concept_ids: &mut HashSet<String>,
    edge_keys: &mut HashSet<String>,
) -> Result<()> {
    let Some(types) = schema_json
        .pointer("/data/__schema/types")
        .and_then(Value::as_array)
    else {
        return Ok(());
    };

    for gql_type in types {
        let Some(name) = gql_type.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name.starts_with("__") {
            continue;
        }
        let kind = gql_type
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("OBJECT");
        let concept_kind = graphql_concept_kind(kind);
        let id = concept_id(version, name);
        let defined_in_path = graphql_reference_path(version, kind, name);
        let stored_defined_in_path = defined_in_path.clone();
        insert_unique_concept(
            graph,
            concept_ids,
            ConceptRecord {
                id: id.clone(),
                kind: concept_kind.to_string(),
                name: name.to_string(),
                version: Some(version.to_string()),
                defined_in_path: stored_defined_in_path,
                deprecated: false,
                deprecated_since: None,
                deprecation_reason: None,
                replaced_by: None,
                kind_metadata: serde_json::to_string(gql_type).ok(),
            },
        );
        if let Some(path) = defined_in_path.filter(|path| doc_paths.contains(path)) {
            insert_unique_edge(
                graph,
                edge_keys,
                GraphEdgeRecord {
                    from_type: "concept".to_string(),
                    from_id: id.clone(),
                    to_type: "doc".to_string(),
                    to_id: path.clone(),
                    kind: "defined_in".to_string(),
                    weight: 1.0,
                    source_path: Some(path),
                },
            );
        }
    }

    for gql_type in types {
        let Some(parent_name) = gql_type.get("name").and_then(Value::as_str) else {
            continue;
        };
        if parent_name.starts_with("__") {
            continue;
        }
        let parent_id = concept_id(version, parent_name);
        let source_path = graphql_reference_path(
            version,
            gql_type
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("OBJECT"),
            parent_name,
        );
        if let Some(fields) = gql_type.get("fields").and_then(Value::as_array) {
            for field in fields {
                ingest_graphql_field(
                    version,
                    &parent_id,
                    parent_name,
                    field,
                    "graphql_field",
                    "has_field",
                    "returns",
                    source_path.as_deref(),
                    graph,
                    concept_ids,
                    edge_keys,
                )?;
            }
        }
        if let Some(input_fields) = gql_type.get("inputFields").and_then(Value::as_array) {
            for field in input_fields {
                ingest_graphql_field(
                    version,
                    &parent_id,
                    parent_name,
                    field,
                    "graphql_input_field",
                    "accepts_input",
                    "references_type",
                    source_path.as_deref(),
                    graph,
                    concept_ids,
                    edge_keys,
                )?;
            }
        }
        if let Some(interfaces) = gql_type.get("interfaces").and_then(Value::as_array) {
            for interface in interfaces {
                if let Some(interface_name) = interface.get("name").and_then(Value::as_str) {
                    let interface_id = concept_id(version, interface_name);
                    if concept_ids.contains(&interface_id) {
                        insert_unique_edge(
                            graph,
                            edge_keys,
                            GraphEdgeRecord {
                                from_type: "concept".to_string(),
                                from_id: parent_id.clone(),
                                to_type: "concept".to_string(),
                                to_id: interface_id,
                                kind: "implements".to_string(),
                                weight: 1.0,
                                source_path: source_path.clone(),
                            },
                        );
                    }
                }
            }
        }
        if let Some(enum_values) = gql_type.get("enumValues").and_then(Value::as_array) {
            for enum_value in enum_values {
                if let Some(value_name) = enum_value.get("name").and_then(Value::as_str) {
                    let value_id = format!("{parent_id}.{value_name}");
                    insert_unique_concept(
                        graph,
                        concept_ids,
                        ConceptRecord {
                            id: value_id.clone(),
                            kind: "graphql_enum_value".to_string(),
                            name: format!("{parent_name}.{value_name}"),
                            version: Some(version.to_string()),
                            defined_in_path: source_path.clone(),
                            deprecated: enum_value
                                .get("isDeprecated")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            deprecated_since: None,
                            deprecation_reason: enum_value
                                .get("deprecationReason")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            replaced_by: None,
                            kind_metadata: serde_json::to_string(enum_value).ok(),
                        },
                    );
                    insert_unique_edge(
                        graph,
                        edge_keys,
                        GraphEdgeRecord {
                            from_type: "concept".to_string(),
                            from_id: parent_id.clone(),
                            to_type: "concept".to_string(),
                            to_id: value_id,
                            kind: "member_of".to_string(),
                            weight: 1.0,
                            source_path: source_path.clone(),
                        },
                    );
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn ingest_graphql_field(
    version: &str,
    parent_id: &str,
    parent_name: &str,
    field: &Value,
    concept_kind: &str,
    parent_edge_kind: &str,
    target_edge_kind: &str,
    source_path: Option<&str>,
    graph: &mut GraphBuild,
    concept_ids: &mut HashSet<String>,
    edge_keys: &mut HashSet<String>,
) -> Result<()> {
    let Some(field_name) = field.get("name").and_then(Value::as_str) else {
        return Ok(());
    };
    let field_id = format!("{parent_id}.{field_name}");
    insert_unique_concept(
        graph,
        concept_ids,
        ConceptRecord {
            id: field_id.clone(),
            kind: concept_kind.to_string(),
            name: format!("{parent_name}.{field_name}"),
            version: Some(version.to_string()),
            defined_in_path: source_path.map(ToOwned::to_owned),
            deprecated: field
                .get("isDeprecated")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            deprecated_since: None,
            deprecation_reason: field
                .get("deprecationReason")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            replaced_by: None,
            kind_metadata: serde_json::to_string(field).ok(),
        },
    );
    insert_unique_edge(
        graph,
        edge_keys,
        GraphEdgeRecord {
            from_type: "concept".to_string(),
            from_id: parent_id.to_string(),
            to_type: "concept".to_string(),
            to_id: field_id.clone(),
            kind: parent_edge_kind.to_string(),
            weight: 1.0,
            source_path: source_path.map(ToOwned::to_owned),
        },
    );
    if let Some(target_name) = field.get("type").and_then(extract_named_type) {
        if let Some(target_id) = resolve_concept_id(version, &target_name, concept_ids) {
            insert_unique_edge(
                graph,
                edge_keys,
                GraphEdgeRecord {
                    from_type: "concept".to_string(),
                    from_id: field_id,
                    to_type: "concept".to_string(),
                    to_id: target_id,
                    kind: target_edge_kind.to_string(),
                    weight: 1.0,
                    source_path: source_path.map(ToOwned::to_owned),
                },
            );
        }
    }
    Ok(())
}

fn add_doc_graph_edges(
    doc_paths: &HashSet<String>,
    doc_contents: &HashMap<String, &str>,
    concept_ids: &HashSet<String>,
    graph: &mut GraphBuild,
    edge_keys: &mut HashSet<String>,
) -> Result<()> {
    let concept_names = concept_ids
        .iter()
        .filter_map(|id| {
            let mut parts = id.split('.');
            let surface = parts.next()?;
            let version = parts.next()?;
            let name = parts.next()?;
            if surface == "admin_graphql" && !name.contains('.') {
                Some((version.to_string(), name.to_string(), id.clone()))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    for (path, content) in doc_contents {
        for (_, name, id) in &concept_names {
            if markdown_mentions_type(content, name) {
                insert_unique_edge(
                    graph,
                    edge_keys,
                    GraphEdgeRecord {
                        from_type: "doc".to_string(),
                        from_id: path.clone(),
                        to_type: "concept".to_string(),
                        to_id: id.clone(),
                        kind: "references_type".to_string(),
                        weight: 1.0,
                        source_path: Some(path.clone()),
                    },
                );
            }
        }
        for link in parse_markdown_links(content) {
            if let Ok(target_path) = canonical_doc_path(&link.url) {
                if doc_paths.contains(&target_path) {
                    insert_unique_edge(
                        graph,
                        edge_keys,
                        GraphEdgeRecord {
                            from_type: "doc".to_string(),
                            from_id: path.clone(),
                            to_type: "doc".to_string(),
                            to_id: target_path,
                            kind: "see_also".to_string(),
                            weight: 1.0,
                            source_path: Some(path.clone()),
                        },
                    );
                }
            }
        }
    }

    let mut sorted_docs = doc_paths.iter().cloned().collect::<Vec<_>>();
    sorted_docs.sort();
    for pair in sorted_docs.windows(2) {
        if let [prev, next] = pair {
            insert_unique_edge(
                graph,
                edge_keys,
                GraphEdgeRecord {
                    from_type: "doc".to_string(),
                    from_id: prev.clone(),
                    to_type: "doc".to_string(),
                    to_id: next.clone(),
                    kind: "next".to_string(),
                    weight: 1.0,
                    source_path: None,
                },
            );
            insert_unique_edge(
                graph,
                edge_keys,
                GraphEdgeRecord {
                    from_type: "doc".to_string(),
                    from_id: next.clone(),
                    to_type: "doc".to_string(),
                    to_id: prev.clone(),
                    kind: "prev".to_string(),
                    weight: 1.0,
                    source_path: None,
                },
            );
        }
    }
    Ok(())
}

fn graphql_concept_kind(kind: &str) -> &'static str {
    match kind {
        "OBJECT" => "graphql_type",
        "INPUT_OBJECT" => "graphql_input_object",
        "INTERFACE" => "graphql_interface",
        "UNION" => "graphql_union",
        "ENUM" => "graphql_enum",
        "SCALAR" => "graphql_scalar",
        _ => "graphql_type",
    }
}

fn graphql_reference_path(version: &str, kind: &str, name: &str) -> Option<String> {
    let section = match kind {
        "OBJECT" => "objects",
        "INPUT_OBJECT" => "input-objects",
        "INTERFACE" => "interfaces",
        "UNION" => "unions",
        "ENUM" => "enums",
        "SCALAR" => "scalars",
        _ => return None,
    };
    Some(format!(
        "/docs/api/admin-graphql/{version}/{section}/{name}"
    ))
}

fn concept_id(version: &str, name: &str) -> String {
    format!("admin_graphql.{version}.{name}")
}

fn resolve_concept_id(
    version: &str,
    target_name: &str,
    concept_ids: &HashSet<String>,
) -> Option<String> {
    let exact = concept_id(version, target_name);
    if concept_ids.contains(&exact) {
        return Some(exact);
    }
    if let Some(stripped) = target_name.strip_suffix("Connection") {
        let stripped_id = concept_id(version, stripped);
        if concept_ids.contains(&stripped_id) {
            return Some(stripped_id);
        }
    }
    None
}

fn extract_named_type(value: &Value) -> Option<String> {
    if let Some(name) = value.get("name").and_then(Value::as_str) {
        return Some(name.to_string());
    }
    value.get("ofType").and_then(extract_named_type)
}

fn markdown_mentions_type(markdown: &str, type_name: &str) -> bool {
    let pattern = format!(r"\b{}\b", regex::escape(type_name));
    Regex::new(&pattern)
        .expect("escaped type name regex is valid")
        .is_match(markdown)
}

fn insert_unique_concept(
    graph: &mut GraphBuild,
    concept_ids: &mut HashSet<String>,
    concept: ConceptRecord,
) {
    if concept_ids.insert(concept.id.clone()) {
        graph.concepts.push(concept);
    }
}

fn insert_unique_edge(
    graph: &mut GraphBuild,
    edge_keys: &mut HashSet<String>,
    edge: GraphEdgeRecord,
) {
    let key = format!(
        "{}:{}:{}:{}:{}",
        edge.from_type, edge.from_id, edge.kind, edge.to_type, edge.to_id
    );
    if edge_keys.insert(key) {
        graph.edges.push(edge);
    }
}

fn store_source_doc(paths: &Paths, source: &SourceDoc) -> Result<DocRecord> {
    let path = canonical_doc_path(&source.url)?;
    let title = title_from_markdown(&source.content)
        .or_else(|| source.title_hint.clone())
        .unwrap_or_else(|| path.clone());
    let sha = hex_sha256(&source.content);
    let raw_path = raw_path_for(&path);
    let raw_file = paths.raw_file(&raw_path);
    if let Some(parent) = raw_file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&raw_file, &source.content)?;
    let now = now_iso();
    Ok(DocRecord {
        path: path.clone(),
        title,
        url: source.url.clone(),
        version: extract_version(&path),
        doc_type: classify_doc_type(&path),
        api_surface: classify_api_surface(&path),
        content_class: classify_content_class(&path),
        content_sha: sha,
        last_verified: now.clone(),
        last_changed: now,
        freshness: "fresh".to_string(),
        references_deprecated: false,
        deprecated_refs: Vec::new(),
        summary_raw: source.content.chars().take(400).collect(),
        reading_time_min: Some(reading_time_min(&source.content)),
        raw_path,
        source: source.source.clone(),
    })
}

fn open_db(paths: &Paths) -> Result<Connection> {
    if let Some(parent) = paths.db.parent() {
        fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(&paths.db)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(conn)
}

fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS schema_meta (
          key TEXT PRIMARY KEY,
          value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS docs (
          path TEXT PRIMARY KEY,
          title TEXT NOT NULL,
          url TEXT NOT NULL,
          version TEXT,
          doc_type TEXT NOT NULL,
          api_surface TEXT,
          content_class TEXT NOT NULL,
          content_sha TEXT NOT NULL,
          last_verified TEXT NOT NULL,
          last_changed TEXT NOT NULL,
          freshness TEXT NOT NULL,
          references_deprecated INTEGER NOT NULL DEFAULT 0,
          deprecated_refs TEXT,
          summary_raw TEXT NOT NULL,
          reading_time_min INTEGER,
          raw_path TEXT NOT NULL,
          source TEXT NOT NULL DEFAULT 'llms',
          hit_count INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_docs_version ON docs(version);
        CREATE INDEX IF NOT EXISTS idx_docs_surface ON docs(api_surface);
        CREATE INDEX IF NOT EXISTS idx_docs_class ON docs(content_class, api_surface);
        CREATE INDEX IF NOT EXISTS idx_docs_freshness ON docs(freshness);
        CREATE INDEX IF NOT EXISTS idx_docs_source ON docs(source);
        CREATE TABLE IF NOT EXISTS coverage_reports (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          source TEXT NOT NULL,
          canonical_path TEXT,
          source_url TEXT NOT NULL,
          status TEXT NOT NULL,
          reason TEXT,
          http_status INTEGER,
          checked_at TEXT NOT NULL,
          retry_after TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_coverage_status ON coverage_reports(status, checked_at);
        CREATE INDEX IF NOT EXISTS idx_coverage_path ON coverage_reports(canonical_path);
        CREATE TABLE IF NOT EXISTS concepts (
          id TEXT PRIMARY KEY,
          kind TEXT NOT NULL,
          name TEXT NOT NULL,
          version TEXT,
          defined_in_path TEXT,
          deprecated INTEGER NOT NULL DEFAULT 0,
          deprecated_since TEXT,
          deprecation_reason TEXT,
          replaced_by TEXT,
          kind_metadata TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_concepts_name ON concepts(name);
        CREATE INDEX IF NOT EXISTS idx_concepts_kind_version ON concepts(kind, version);
        CREATE TABLE IF NOT EXISTS edges (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          from_type TEXT NOT NULL,
          from_id TEXT NOT NULL,
          to_type TEXT NOT NULL,
          to_id TEXT NOT NULL,
          kind TEXT NOT NULL,
          weight REAL NOT NULL DEFAULT 1.0,
          source_path TEXT,
          extracted_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_edges_from ON edges(from_type, from_id);
        CREATE INDEX IF NOT EXISTS idx_edges_to ON edges(to_type, to_id);
        CREATE INDEX IF NOT EXISTS idx_edges_kind ON edges(kind);
        CREATE TABLE IF NOT EXISTS changelog_entries (
          id TEXT PRIMARY KEY,
          title TEXT NOT NULL,
          url TEXT NOT NULL,
          posted_at TEXT,
          body TEXT NOT NULL,
          categories TEXT NOT NULL,
          affected_types TEXT NOT NULL,
          affected_surfaces TEXT NOT NULL,
          unresolved_affected_refs TEXT NOT NULL,
          processed_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_changelog_posted_at ON changelog_entries(posted_at);
        CREATE TABLE IF NOT EXISTS scheduled_changes (
          id TEXT PRIMARY KEY,
          type_name TEXT NOT NULL,
          change TEXT NOT NULL,
          effective_date TEXT,
          migration_hint TEXT,
          source_changelog_id TEXT,
          FOREIGN KEY (source_changelog_id) REFERENCES changelog_entries(id)
        );
        CREATE INDEX IF NOT EXISTS idx_scheduled_changes_type ON scheduled_changes(type_name);
        CREATE INDEX IF NOT EXISTS idx_scheduled_changes_effective ON scheduled_changes(effective_date);
        CREATE TABLE IF NOT EXISTS indexed_versions (
          version TEXT PRIMARY KEY,
          api_surface TEXT NOT NULL,
          indexed_at TEXT NOT NULL,
          doc_count INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_indexed_versions_surface ON indexed_versions(api_surface);
        CREATE TABLE IF NOT EXISTS version_rebuild_queue (
          version TEXT NOT NULL,
          api_surface TEXT NOT NULL,
          status TEXT NOT NULL,
          reason TEXT NOT NULL,
          enqueued_at TEXT NOT NULL,
          PRIMARY KEY(version, api_surface)
        );
        CREATE INDEX IF NOT EXISTS idx_version_rebuild_queue_status ON version_rebuild_queue(status, enqueued_at);
        CREATE TABLE IF NOT EXISTS tasks (
          id TEXT PRIMARY KEY,
          title TEXT NOT NULL,
          description TEXT,
          root_path TEXT,
          related_paths TEXT NOT NULL
        );
        ",
    )?;
    ensure_column(conn, "docs", "source", "TEXT NOT NULL DEFAULT 'llms'")?;
    ensure_column(
        conn,
        "docs",
        "references_deprecated",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(conn, "docs", "deprecated_refs", "TEXT")?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_docs_source ON docs(source)",
        [],
    )?;
    conn.execute(
        "INSERT INTO schema_meta(key, value) VALUES('schema_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![SCHEMA_VERSION],
    )?;
    Ok(())
}

fn ensure_column(conn: &Connection, table: &str, column: &str, definition: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for existing in columns {
        if existing? == column {
            return Ok(());
        }
    }
    conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
        [],
    )?;
    Ok(())
}

fn clear_coverage_reports(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM coverage_reports", [])?;
    Ok(())
}

fn clear_graph_tables(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM edges", [])?;
    conn.execute("DELETE FROM concepts", [])?;
    conn.execute("DELETE FROM tasks", [])?;
    Ok(())
}

fn insert_coverage_event(conn: &Connection, event: &CoverageEvent) -> Result<()> {
    conn.execute(
        "
        INSERT INTO coverage_reports (
          source, canonical_path, source_url, status, reason, http_status, checked_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ",
        params![
            event.source,
            event.canonical_path,
            event.source_url,
            event.status,
            event.reason,
            event.http_status,
            event.checked_at,
        ],
    )?;
    Ok(())
}

fn failed_coverage_rows(conn: &Connection) -> Result<Vec<CoverageRepairRow>> {
    let mut stmt = conn.prepare(
        "
        SELECT id, source_url
        FROM coverage_reports
        WHERE status = 'failed'
        ORDER BY checked_at, id
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(CoverageRepairRow {
            id: row.get(0)?,
            source_url: row.get(1)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn update_coverage_repaired(
    conn: &Connection,
    id: i64,
    candidate: &OnDemandFetchCandidate,
) -> Result<()> {
    conn.execute(
        "
        UPDATE coverage_reports
        SET canonical_path = ?1,
            source_url = ?2,
            status = 'indexed',
            reason = NULL,
            http_status = NULL,
            checked_at = ?3
        WHERE id = ?4
        ",
        params![
            candidate.canonical_path,
            candidate.source_url,
            now_iso(),
            id
        ],
    )?;
    Ok(())
}

fn update_coverage_failed(conn: &Connection, id: i64, reason: &str) -> Result<()> {
    conn.execute(
        "
        UPDATE coverage_reports
        SET status = 'failed',
            reason = ?1,
            checked_at = ?2
        WHERE id = ?3
        ",
        params![reason, now_iso(), id],
    )?;
    Ok(())
}

fn insert_concept(conn: &Connection, concept: &ConceptRecord) -> Result<()> {
    conn.execute(
        "
        INSERT INTO concepts (
          id, kind, name, version, defined_in_path, deprecated, deprecated_since,
          deprecation_reason, replaced_by, kind_metadata
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ON CONFLICT(id) DO UPDATE SET
          kind=excluded.kind,
          name=excluded.name,
          version=excluded.version,
          defined_in_path=excluded.defined_in_path,
          deprecated=excluded.deprecated,
          deprecated_since=excluded.deprecated_since,
          deprecation_reason=excluded.deprecation_reason,
          replaced_by=excluded.replaced_by,
          kind_metadata=excluded.kind_metadata
        ",
        params![
            concept.id,
            concept.kind,
            concept.name,
            concept.version,
            concept.defined_in_path,
            i64::from(concept.deprecated),
            concept.deprecated_since,
            concept.deprecation_reason,
            concept.replaced_by,
            concept.kind_metadata,
        ],
    )?;
    Ok(())
}

fn insert_edge(conn: &Connection, edge: &GraphEdgeRecord) -> Result<()> {
    conn.execute(
        "
        INSERT INTO edges (
          from_type, from_id, to_type, to_id, kind, weight, source_path, extracted_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        ",
        params![
            edge.from_type,
            edge.from_id,
            edge.to_type,
            edge.to_id,
            edge.kind,
            edge.weight,
            edge.source_path,
            now_iso(),
        ],
    )?;
    Ok(())
}

fn upsert_doc(conn: &Connection, doc: &DocRecord) -> Result<()> {
    conn.execute(
        "
        INSERT INTO docs (
          path, title, url, version, doc_type, api_surface, content_class, content_sha,
          last_verified, last_changed, freshness, references_deprecated, deprecated_refs,
          summary_raw, reading_time_min, raw_path, source
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
        ON CONFLICT(path) DO UPDATE SET
          title=excluded.title,
          url=excluded.url,
          version=excluded.version,
          doc_type=excluded.doc_type,
          api_surface=excluded.api_surface,
          content_class=excluded.content_class,
          content_sha=excluded.content_sha,
          last_verified=excluded.last_verified,
          last_changed=CASE
            WHEN docs.content_sha = excluded.content_sha THEN docs.last_changed
            ELSE excluded.last_changed
          END,
          freshness=excluded.freshness,
          references_deprecated=CASE
            WHEN excluded.references_deprecated != 0 THEN 1
            ELSE docs.references_deprecated
          END,
          deprecated_refs=CASE
            WHEN excluded.deprecated_refs IS NOT NULL AND excluded.deprecated_refs != '[]'
              THEN excluded.deprecated_refs
            ELSE docs.deprecated_refs
          END,
          summary_raw=excluded.summary_raw,
          reading_time_min=excluded.reading_time_min,
          raw_path=excluded.raw_path,
          source=CASE
            WHEN docs.source = 'llms' AND excluded.source IN ('sitemap', 'on_demand', 'fixture') THEN docs.source
            WHEN docs.source = 'sitemap' AND excluded.source IN ('on_demand', 'fixture') THEN docs.source
            WHEN docs.source = 'on_demand' AND excluded.source = 'fixture' THEN docs.source
            ELSE excluded.source
          END
        ",
        params![
            doc.path,
            doc.title,
            doc.url,
            doc.version,
            doc.doc_type,
            doc.api_surface,
            doc.content_class,
            doc.content_sha,
            doc.last_verified,
            doc.last_changed,
            doc.freshness,
            i64::from(doc.references_deprecated),
            serde_json::to_string(&doc.deprecated_refs)?,
            doc.summary_raw,
            doc.reading_time_min,
            doc.raw_path,
            doc.source,
        ],
    )?;
    Ok(())
}

fn get_doc(conn: &Connection, path: &str) -> Result<Option<DocRecord>> {
    conn.query_row(
        "SELECT path, title, url, version, doc_type, api_surface, content_class, content_sha,
                last_verified, last_changed, freshness, references_deprecated, deprecated_refs,
                summary_raw, reading_time_min, raw_path, source
         FROM docs WHERE path = ?1",
        params![path],
        doc_from_row,
    )
    .optional()
    .map_err(Into::into)
}

fn doc_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DocRecord> {
    Ok(DocRecord {
        path: row.get(0)?,
        title: row.get(1)?,
        url: row.get(2)?,
        version: row.get(3)?,
        doc_type: row.get(4)?,
        api_surface: row.get(5)?,
        content_class: row.get(6)?,
        content_sha: row.get(7)?,
        last_verified: row.get(8)?,
        last_changed: row.get(9)?,
        freshness: row.get(10)?,
        references_deprecated: row.get::<_, i64>(11)? != 0,
        deprecated_refs: parse_json_string_vec(row.get::<_, Option<String>>(12)?.as_deref()),
        summary_raw: row.get(13)?,
        reading_time_min: row.get(14)?,
        raw_path: row.get(15)?,
        source: row.get(16)?,
    })
}

fn parse_json_string_vec(value: Option<&str>) -> Vec<String> {
    value
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .unwrap_or_default()
}

fn staleness(doc: &DocRecord) -> Staleness {
    let verified = DateTime::parse_from_rfc3339(&doc.last_verified)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    let age_days = (Utc::now() - verified).num_days();
    Staleness {
        age_days,
        freshness: match age_days {
            0..=7 => "fresh",
            8..=30 => "aging",
            _ => "stale",
        }
        .to_string(),
        content_verified_at: doc.last_verified.clone(),
        schema_version: doc.version.clone(),
        references_deprecated: doc.references_deprecated || !doc.deprecated_refs.is_empty(),
        deprecated_refs: doc.deprecated_refs.clone(),
        upcoming_changes: Vec::new(),
    }
}

fn staleness_for_doc(conn: &Connection, doc: &DocRecord) -> Result<Staleness> {
    let mut staleness = staleness(doc);
    staleness.upcoming_changes = scheduled_changes_for_refs(conn, &doc.deprecated_refs)?;
    if !staleness.upcoming_changes.is_empty() {
        staleness.references_deprecated = true;
    }
    Ok(staleness)
}

fn scheduled_changes_for_concept(conn: &Connection, concept: &ConceptRecord) -> Result<Vec<Value>> {
    let mut refs = vec![concept.id.clone(), concept.name.clone()];
    refs.sort();
    refs.dedup();
    scheduled_changes_for_refs(conn, &refs)
}

fn scheduled_changes_for_refs(conn: &Connection, refs: &[String]) -> Result<Vec<Value>> {
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let today = Utc::now().date_naive().to_string();
    let mut changes = Vec::new();
    let mut stmt = conn.prepare(
        "
        SELECT type_name, change, effective_date, migration_hint, source_changelog_id
        FROM scheduled_changes
        WHERE type_name = ?1
          AND (effective_date IS NULL OR effective_date >= ?2)
        ORDER BY effective_date, type_name, source_changelog_id
        ",
    )?;
    let mut seen = HashSet::new();
    for reference in refs {
        let rows = stmt.query_map(params![reference, today], |row| {
            Ok(json!({
                "type_name": row.get::<_, String>(0)?,
                "change": row.get::<_, String>(1)?,
                "effective_date": row.get::<_, Option<String>>(2)?,
                "migration_hint": row.get::<_, Option<String>>(3)?,
                "source_changelog_id": row.get::<_, Option<String>>(4)?,
            }))
        })?;
        for row in rows {
            let value = row?;
            let key = serde_json::to_string(&value).unwrap_or_default();
            if seen.insert(key) {
                changes.push(value);
            }
        }
    }
    Ok(changes)
}

#[derive(Clone, Copy)]
struct SearchFields {
    path: Field,
    title: Field,
    url: Field,
    version: Field,
    api_surface: Field,
    doc_type: Field,
    content_en: Field,
    content_ja: Field,
}

impl SearchFields {
    fn from_schema(schema: &Schema) -> Result<Self> {
        Ok(Self {
            path: schema.get_field("path")?,
            title: schema.get_field("title")?,
            url: schema.get_field("url")?,
            version: schema.get_field("version")?,
            api_surface: schema.get_field("api_surface")?,
            doc_type: schema.get_field("doc_type")?,
            content_en: schema.get_field("content_en")?,
            content_ja: schema.get_field("content_ja")?,
        })
    }

    fn content_fields(&self) -> [Field; 2] {
        [self.content_en, self.content_ja]
    }
}

fn search_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("path", STRING | STORED);
    builder.add_text_field("title", TEXT | STORED);
    builder.add_text_field("url", STRING | STORED);
    builder.add_text_field("version", STRING | STORED);
    builder.add_text_field("api_surface", STRING | STORED);
    builder.add_text_field("doc_type", STRING | STORED);
    builder.add_text_field("content_en", TEXT);
    let japanese_text = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("lindera_ipadic")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    builder.add_text_field("content_ja", japanese_text);
    builder.build()
}

fn register_japanese_tokenizer(index: &Index) -> Result<()> {
    index.tokenizers().register(
        "lindera_ipadic",
        LinderaTokenizer::from_segmenter(japanese_segmenter()?),
    );
    Ok(())
}

fn japanese_segmenter() -> Result<Segmenter> {
    static JAPANESE_SEGMENTER: OnceLock<std::result::Result<Segmenter, String>> = OnceLock::new();
    match JAPANESE_SEGMENTER.get_or_init(|| {
        load_dictionary("embedded://ipadic")
            .map_err(|error| error.to_string())
            .map(|dictionary| Segmenter::new(Mode::Normal, dictionary, None))
    }) {
        Ok(segmenter) => Ok(segmenter.clone()),
        Err(error) => {
            bail!("load Japanese tokenizer dictionary: {error}")
        }
    }
}

fn create_or_reset_index(paths: &Paths, schema: Schema, reset: bool) -> Result<Index> {
    if reset && paths.tantivy.exists() {
        fs::remove_dir_all(&paths.tantivy)?;
    }
    fs::create_dir_all(&paths.tantivy)?;
    if paths.tantivy.join("meta.json").exists() {
        let index = Index::open_in_dir(&paths.tantivy)?;
        if SearchFields::from_schema(&index.schema()).is_ok() {
            Ok(index)
        } else {
            fs::remove_dir_all(&paths.tantivy)?;
            fs::create_dir_all(&paths.tantivy)?;
            Index::create_in_dir(&paths.tantivy, schema).map_err(Into::into)
        }
    } else {
        Index::create_in_dir(&paths.tantivy, schema).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockTextSource {
        texts: std::collections::HashMap<String, String>,
    }

    impl MockTextSource {
        fn new(entries: &[(&str, &str)]) -> Self {
            Self {
                texts: entries
                    .iter()
                    .map(|(url, body)| ((*url).to_string(), (*body).to_string()))
                    .collect(),
            }
        }
    }

    impl TextSource for MockTextSource {
        async fn fetch_text(&self, url: &str) -> std::result::Result<String, SourceFetchError> {
            self.texts
                .get(url)
                .cloned()
                .ok_or_else(|| SourceFetchError {
                    status: "skipped".to_string(),
                    reason: "mock_not_found".to_string(),
                    http_status: Some(404),
                })
        }
    }

    fn fixture_sources() -> MockTextSource {
        MockTextSource::new(&[
            (
                SHOPIFY_LLMS_URL,
                "[Admin GraphQL](/docs/api/admin-graphql)\n",
            ),
            (
                SHOPIFY_SITEMAP_URL,
                r#"
                <urlset>
                  <url><loc>https://shopify.dev/docs/api/admin-graphql</loc></url>
                  <url><loc>https://shopify.dev/docs/apps/build/access-scopes</loc></url>
                </urlset>
                "#,
            ),
            (
                "https://shopify.dev/docs/api/admin-graphql.md",
                "# Admin GraphQL API\nReference docs for the Admin GraphQL API.\n",
            ),
            (
                "https://shopify.dev/docs/apps/build/access-scopes.md",
                "# Access scopes\nOptional access scopes and managed access scopes let apps request protected permissions.\n",
            ),
        ])
    }

    #[test]
    fn daemon_identity_uses_canonical_home() {
        let dir = tempfile::tempdir().unwrap();
        let dotted = Paths::new(Some(dir.path().join("."))).unwrap();
        let canonical = Paths::new(Some(fs::canonicalize(dir.path()).unwrap())).unwrap();

        let dotted_identity = DaemonIdentity::for_paths(&dotted).unwrap();
        let canonical_identity = DaemonIdentity::for_paths(&canonical).unwrap();

        assert_eq!(dotted_identity.hash(), canonical_identity.hash());
    }

    #[test]
    fn daemon_identity_separates_different_homes_and_config() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let paths_a = Paths::new(Some(dir_a.path().to_path_buf())).unwrap();
        let paths_b = Paths::new(Some(dir_b.path().to_path_buf())).unwrap();

        let hash_a = DaemonIdentity::for_paths(&paths_a).unwrap().hash();
        let hash_b = DaemonIdentity::for_paths(&paths_b).unwrap().hash();
        assert_ne!(hash_a, hash_b);

        fs::write(
            paths_a.config_file(),
            "[index]\nenable_on_demand_fetch = true\n",
        )
        .unwrap();
        let hash_a_with_config = DaemonIdentity::for_paths(&paths_a).unwrap().hash();
        assert_ne!(hash_a, hash_a_with_config);
    }

    #[test]
    fn daemon_socket_path_uses_bounded_hashed_filename() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let daemon_paths = DaemonPaths::for_paths(&paths).unwrap();
        let filename = daemon_paths
            .socket
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap();

        assert!(filename.ends_with(".sock"));
        assert!(filename.len() <= 69);
        assert!(daemon_paths.socket.as_os_str().len() < 100);
    }

    const ADMIN_GRAPHQL_DIRECT_PROXY_2026_04: &str =
        "https://shopify.dev/admin-graphql-direct-proxy/2026-04";

    fn admin_graphql_graph_sources() -> MockTextSource {
        MockTextSource::new(&[
            (
                SHOPIFY_LLMS_URL,
                "[Product](/docs/api/admin-graphql/2026-04/objects/Product)\n\
                 [Products guide](/docs/apps/build/products)\n\
                 [Cart level discount functions](/docs/apps/build/discounts/cart-level)\n\
                 [Discount overview](/docs/apps/build/discounts/overview)\n",
            ),
            (
                SHOPIFY_SITEMAP_URL,
                r#"
                <urlset>
                  <url><loc>https://shopify.dev/docs/api/admin-graphql/2026-04/objects/Product</loc></url>
                  <url><loc>https://shopify.dev/docs/apps/build/products</loc></url>
                  <url><loc>https://shopify.dev/docs/apps/build/discounts/cart-level</loc></url>
                  <url><loc>https://shopify.dev/docs/apps/build/discounts/overview</loc></url>
                </urlset>
                "#,
            ),
            (
                "https://shopify.dev/docs/api/admin-graphql/2026-04/objects/Product.md",
                "# Product\nThe `Product` object represents goods that a merchant can sell.\n",
            ),
            (
                "https://shopify.dev/docs/apps/build/products.md",
                "# Products guide\nUse Product when building product workflows.\n\n```graphql\nquery ProductGuide {\n  product(id: \"gid://shopify/Product/1\") {\n    id\n    title\n    variants(first: 10) { nodes { id } }\n  }\n}\n```\n",
            ),
            (
                "https://shopify.dev/docs/apps/build/discounts/cart-level.md",
                "# Cart level discount functions\nBuild a discount function cart level workflow that reads Product data before applying discounts. 割引クーポンの組み合わせを確認する。\n\n```graphql\nquery DiscountProducts {\n  products(first: 5) { nodes { id title } }\n}\n```\n",
            ),
            (
                "https://shopify.dev/docs/apps/build/discounts/overview.md",
                "# Discount overview\nUse this unpromoted discount overview when a cart level discount function does not mention schema types directly.\n",
            ),
            (
                ADMIN_GRAPHQL_DIRECT_PROXY_2026_04,
                r#"
                {
                  "data": {
                    "__schema": {
                      "types": [
                        {
                          "kind": "OBJECT",
                          "name": "Product",
                          "description": "A product that a merchant can sell.",
                          "fields": [
                            {
                              "name": "id",
                              "description": "The product ID.",
                              "args": [],
                              "type": {
                                "kind": "NON_NULL",
                                "name": null,
                                "ofType": { "kind": "SCALAR", "name": "ID", "ofType": null }
                              },
                              "isDeprecated": false,
                              "deprecationReason": null
                            },
                            {
                              "name": "variants",
                              "description": "The product variants.",
                              "args": [],
                              "type": {
                                "kind": "OBJECT",
                                "name": "ProductVariantConnection",
                                "ofType": null
                              },
                              "isDeprecated": false,
                              "deprecationReason": null
                            }
                          ],
                          "inputFields": null,
                          "interfaces": [],
                          "enumValues": null,
                          "possibleTypes": null
                        },
                        {
                          "kind": "OBJECT",
                          "name": "ProductVariant",
                          "description": "A product variant.",
                          "fields": [
                            {
                              "name": "id",
                              "description": "The variant ID.",
                              "args": [],
                              "type": {
                                "kind": "NON_NULL",
                                "name": null,
                                "ofType": { "kind": "SCALAR", "name": "ID", "ofType": null }
                              },
                              "isDeprecated": false,
                              "deprecationReason": null
                            }
                          ],
                          "inputFields": null,
                          "interfaces": [],
                          "enumValues": null,
                          "possibleTypes": null
                        },
                        {
                          "kind": "INPUT_OBJECT",
                          "name": "ProductInput",
                          "description": "Input fields for a product.",
                          "fields": null,
                          "inputFields": [
                            {
                              "name": "title",
                              "description": "The product title.",
                              "type": { "kind": "SCALAR", "name": "String", "ofType": null },
                              "defaultValue": null,
                              "isDeprecated": false,
                              "deprecationReason": null
                            }
                          ],
                          "interfaces": null,
                          "enumValues": null,
                          "possibleTypes": null
                        }
                      ]
                    }
                  }
                }
                "#,
            ),
        ])
    }

    fn seed_draft_order_graph(paths: &Paths) -> Connection {
        fs::create_dir_all(&paths.raw).unwrap();
        let conn = open_db(paths).unwrap();
        init_db(&conn).unwrap();
        let source = SourceDoc {
            url: "https://shopify.dev/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem.md"
                .to_string(),
            title_hint: Some("DraftOrderLineItem".to_string()),
            content:
                "# DraftOrderLineItem\nThe DraftOrderLineItem object includes the grams field.\n"
                    .to_string(),
            source: "sitemap".to_string(),
        };
        let record = store_source_doc(paths, &source).unwrap();
        upsert_doc(&conn, &record).unwrap();
        insert_concept(
            &conn,
            &ConceptRecord {
                id: "admin_graphql.2026-04.DraftOrderLineItem".to_string(),
                kind: "graphql_type".to_string(),
                name: "DraftOrderLineItem".to_string(),
                version: Some("2026-04".to_string()),
                defined_in_path: Some(
                    "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem".to_string(),
                ),
                deprecated: false,
                deprecated_since: None,
                deprecation_reason: None,
                replaced_by: None,
                kind_metadata: None,
            },
        )
        .unwrap();
        insert_concept(
            &conn,
            &ConceptRecord {
                id: "admin_graphql.2026-04.DraftOrderLineItem.grams".to_string(),
                kind: "graphql_field".to_string(),
                name: "DraftOrderLineItem.grams".to_string(),
                version: Some("2026-04".to_string()),
                defined_in_path: Some(
                    "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem".to_string(),
                ),
                deprecated: false,
                deprecated_since: None,
                deprecation_reason: None,
                replaced_by: None,
                kind_metadata: None,
            },
        )
        .unwrap();
        insert_edge(
            &conn,
            &GraphEdgeRecord {
                from_type: "concept".to_string(),
                from_id: "admin_graphql.2026-04.DraftOrderLineItem.grams".to_string(),
                to_type: "doc".to_string(),
                to_id: "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem".to_string(),
                kind: "defined_in".to_string(),
                weight: 1.0,
                source_path: Some(
                    "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem".to_string(),
                ),
            },
        )
        .unwrap();
        insert_edge(
            &conn,
            &GraphEdgeRecord {
                from_type: "concept".to_string(),
                from_id: "admin_graphql.2026-04.DraftOrderLineItem".to_string(),
                to_type: "concept".to_string(),
                to_id: "admin_graphql.2026-04.DraftOrderLineItem.grams".to_string(),
                kind: "has_field".to_string(),
                weight: 1.0,
                source_path: Some(
                    "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem".to_string(),
                ),
            },
        )
        .unwrap();
        rebuild_tantivy_from_db(paths).unwrap();
        conn
    }

    fn changelog_feed(title: &str, body: &str, link: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <rss version="2.0">
              <channel>
                <title>Shopify Developer changelog</title>
                <item>
                  <guid>{link}</guid>
                  <title>{title}</title>
                  <link>{link}</link>
                  <description><![CDATA[{body}]]></description>
                  <pubDate>Fri, 17 Apr 2026 00:00:00 GMT</pubDate>
                  <category>Admin GraphQL API</category>
                </item>
              </channel>
            </rss>"#
        )
    }

    fn minimal_introspection() -> &'static str {
        r#"{"data":{"__schema":{"types":[{"kind":"OBJECT","name":"Product"}]}}}"#
    }

    #[test]
    fn parses_shopify_markdown_links() {
        let links =
            parse_markdown_links("[Product](/docs/api/admin-graphql/latest/objects/Product)");
        assert_eq!(links.len(), 1);
        assert_eq!(
            links[0].url,
            "https://shopify.dev/docs/api/admin-graphql/latest/objects/Product"
        );
    }

    #[test]
    fn init_db_creates_v030_changelog_tables() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn).unwrap();

        let changelog_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM changelog_entries", [], |row| {
                row.get(0)
            })
            .unwrap();
        let scheduled_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM scheduled_changes", [], |row| {
                row.get(0)
            })
            .unwrap();

        assert_eq!(changelog_count, 0);
        assert_eq!(scheduled_count, 0);
        assert_eq!(count_where(&conn, "indexed_versions", "1=1").unwrap(), 0);
        assert_eq!(
            count_where(&conn, "version_rebuild_queue", "1=1").unwrap(),
            0
        );
    }

    #[test]
    fn parses_changelog_rss_entry() {
        let feed = changelog_feed(
            "DraftOrderLineItem.grams field removed in 2026-07",
            "Migrate away from grams.",
            "https://shopify.dev/changelog/draft-order-line-item-grams",
        );

        let entries = parse_changelog_feed(&feed).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].title,
            "DraftOrderLineItem.grams field removed in 2026-07"
        );
        assert_eq!(
            entries[0].link,
            "https://shopify.dev/changelog/draft-order-line-item-grams"
        );
        assert!(
            entries[0]
                .categories
                .contains(&"Admin GraphQL API".to_string())
        );
    }

    #[test]
    fn builds_raw_candidates() {
        let candidates =
            raw_doc_candidates("https://shopify.dev/docs/api/admin-graphql/latest").unwrap();
        assert_eq!(
            candidates[0],
            "https://shopify.dev/docs/api/admin-graphql/latest.md"
        );
        assert_eq!(
            candidates[1],
            "https://shopify.dev/docs/api/admin-graphql/latest.txt"
        );
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn classifies_admin_graphql_surface() {
        assert_eq!(
            classify_api_surface("/docs/api/admin-graphql/latest/objects/Product").as_deref(),
            Some("admin_graphql")
        );
        assert_eq!(
            classify_content_class("/docs/api/admin-graphql/latest/objects/Product"),
            "schema_ref"
        );
    }

    #[test]
    fn classifies_root_api_pages() {
        assert_eq!(
            classify_api_surface("/docs/api/admin-graphql").as_deref(),
            Some("admin_graphql")
        );
        assert_eq!(
            classify_api_surface("/docs/api/storefront").as_deref(),
            Some("storefront")
        );
        assert_eq!(classify_content_class("/docs/api/admin-graphql"), "api_ref");
        assert_eq!(classify_doc_type("/docs/api/storefront"), "reference");
    }

    #[test]
    fn canonical_path_strips_raw_suffix() {
        let path = canonical_doc_path(
            "https://shopify.dev/docs/api/admin-graphql/latest/queries/product.txt",
        )
        .unwrap();
        assert_eq!(path, "/docs/api/admin-graphql/latest/queries/product");
    }

    #[test]
    fn canonical_path_keeps_llms_txt() {
        let path = canonical_doc_path("https://shopify.dev/llms.txt").unwrap();
        assert_eq!(path, "/llms.txt");
    }

    fn write_on_demand_config(paths: &Paths, enabled: bool) {
        fs::create_dir_all(paths.config_file().parent().unwrap()).unwrap();
        fs::write(
            paths.config_file(),
            format!("[index]\nenable_on_demand_fetch = {enabled}\n"),
        )
        .unwrap();
    }

    #[test]
    fn on_demand_config_defaults_to_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();

        let config = load_config(&paths).unwrap();

        assert!(!config.index.enable_on_demand_fetch);
    }

    #[test]
    fn on_demand_policy_normalizes_allowed_urls_and_paths() {
        let from_url = OnDemandFetchPolicy::candidate_from_input(
            "https://shopify.dev/docs/apps/build/access-scopes/?utm=1#managed-access-scopes",
        )
        .unwrap();
        assert_eq!(from_url.canonical_path, "/docs/apps/build/access-scopes");
        assert_eq!(
            from_url.source_url,
            "https://shopify.dev/docs/apps/build/access-scopes"
        );

        let from_path =
            OnDemandFetchPolicy::candidate_from_input("/changelog/managed-access-scopes/").unwrap();
        assert_eq!(from_path.canonical_path, "/changelog/managed-access-scopes");
        assert_eq!(
            from_path.source_url,
            "https://shopify.dev/changelog/managed-access-scopes"
        );
    }

    #[test]
    fn on_demand_policy_rejects_outside_scope_without_network_candidate() {
        assert!(
            OnDemandFetchPolicy::candidate_from_input(
                "http://shopify.dev/docs/apps/build/app-home"
            )
            .is_err()
        );
        assert!(
            OnDemandFetchPolicy::candidate_from_input(
                "https://example.com/docs/apps/build/app-home"
            )
            .is_err()
        );
        assert!(OnDemandFetchPolicy::candidate_from_input("https://shopify.dev/partners").is_err());
    }

    #[test]
    fn parses_sitemap_links_for_docs_and_changelog() {
        let links = parse_sitemap_links(
            r#"
            <urlset>
              <url><loc>https://shopify.dev/docs/apps/build/access-scopes</loc></url>
              <url><loc>https://shopify.dev/changelog/managed-access-scopes</loc></url>
              <url><loc>https://shopify.dev/partners</loc></url>
            </urlset>
            "#,
        );
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].source, "sitemap");
        assert_eq!(
            links[0].url,
            "https://shopify.dev/docs/apps/build/access-scopes"
        );
        assert_eq!(
            canonical_doc_path(&links[1].url).unwrap(),
            "/changelog/managed-access-scopes"
        );
    }

    #[test]
    fn extracts_anchor_section_and_removes_code_blocks() {
        let markdown = "# Intro\nKeep\n\n## Managed access scopes\nText\n```graphql\nquery { shop { name } }\n```\nMore\n\n## Next\nDone\n";
        let sections = extract_sections(markdown);
        let scoped = section_content(markdown, &sections, "managed-access-scopes").unwrap();
        assert!(scoped.contains("## Managed access scopes"));
        assert!(!scoped.contains("## Next"));

        let without_code = remove_fenced_code_blocks(&scoped);
        assert!(without_code.contains("Text"));
        assert!(!without_code.contains("query { shop"));
        assert!(without_code.contains("More"));
    }

    #[tokio::test]
    async fn mcp_fetch_url_disabled_returns_v05_error_contract_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let response = handle_mcp_request(
            &paths,
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":1,
                "method":"tools/call",
                "params":{
                    "name":"shopify_fetch",
                    "arguments":{
                        "url":"https://shopify.dev/docs/apps/build/access-scopes"
                    }
                }
            }),
        )
        .await;

        assert_eq!(response["error"]["code"], -32007);
        assert_eq!(
            response["error"]["data"]["candidate_url"],
            "https://shopify.dev/docs/apps/build/access-scopes"
        );
        assert_eq!(
            response["error"]["data"]["canonical_path"],
            "/docs/apps/build/access-scopes"
        );
        assert_eq!(response["error"]["data"]["enable_on_demand_fetch"], false);
    }

    #[tokio::test]
    async fn mcp_fetch_url_outside_scope_returns_policy_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        write_on_demand_config(&paths, true);

        let response = handle_mcp_request(
            &paths,
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":1,
                "method":"tools/call",
                "params":{
                    "name":"shopify_fetch",
                    "arguments":{
                        "url":"https://shopify.dev/partners"
                    }
                }
            }),
        )
        .await;

        assert_eq!(response["error"]["code"], -32008);
        assert!(
            response["error"]["data"]["allowed_scope"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        );
    }

    #[tokio::test]
    async fn on_demand_fetch_url_stores_raw_doc_upserts_and_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        write_on_demand_config(&paths, true);
        let source = MockTextSource::new(&[(
            "https://shopify.dev/docs/apps/build/on-demand.md",
            "# On demand\nRecovered optional access scopes content.\n",
        )]);

        let response = shopify_fetch_from_source(
            &paths,
            &FetchArgs {
                path: None,
                url: Some(
                    "https://shopify.dev/docs/apps/build/on-demand?from=test#top".to_string(),
                ),
                anchor: None,
                include_code_blocks: None,
                max_chars: None,
            },
            &source,
        )
        .await
        .unwrap();

        assert_eq!(response.path, "/docs/apps/build/on-demand");
        assert!(
            response
                .content
                .contains("Recovered optional access scopes")
        );
        let conn = open_db(&paths).unwrap();
        let stored = get_doc(&conn, "/docs/apps/build/on-demand")
            .unwrap()
            .unwrap();
        assert_eq!(stored.source, "on_demand");
        assert!(paths.raw_file(&stored.raw_path).exists());
        let results = search_docs(&paths, "Recovered optional scopes", None, 5).unwrap();
        assert_eq!(results[0].path, "/docs/apps/build/on-demand");

        let fetched_by_path = shopify_fetch_from_source(
            &paths,
            &FetchArgs {
                path: Some("/docs/apps/build/on-demand".to_string()),
                url: None,
                anchor: None,
                include_code_blocks: None,
                max_chars: None,
            },
            &source,
        )
        .await
        .unwrap();
        assert_eq!(fetched_by_path.path, "/docs/apps/build/on-demand");
    }

    #[tokio::test]
    async fn on_demand_fetch_unindexed_path_derives_official_url() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        write_on_demand_config(&paths, true);
        let source = MockTextSource::new(&[(
            "https://shopify.dev/changelog/managed-access-scopes.md",
            "# Managed access scopes\nChangelog text.\n",
        )]);

        let response = shopify_fetch_from_source(
            &paths,
            &FetchArgs {
                path: Some("/changelog/managed-access-scopes".to_string()),
                url: None,
                anchor: None,
                include_code_blocks: None,
                max_chars: None,
            },
            &source,
        )
        .await
        .unwrap();

        assert_eq!(
            response.url,
            "https://shopify.dev/changelog/managed-access-scopes.md"
        );
        assert_eq!(response.path, "/changelog/managed-access-scopes");
    }

    #[tokio::test]
    async fn on_demand_refresh_preserves_existing_higher_precedence_source() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        write_on_demand_config(&paths, true);
        fs::create_dir_all(&paths.raw).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn).unwrap();
        let source_doc = SourceDoc {
            url: "https://shopify.dev/docs/apps/build/access-scopes.md".to_string(),
            title_hint: Some("Access scopes".to_string()),
            content: "# Access scopes\nIndexed by sitemap.\n".to_string(),
            source: "sitemap".to_string(),
        };
        let record = store_source_doc(&paths, &source_doc).unwrap();
        upsert_doc(&conn, &record).unwrap();
        rebuild_tantivy_from_db(&paths).unwrap();
        let source = MockTextSource::new(&[(
            "https://shopify.dev/docs/apps/build/access-scopes.md",
            "# Access scopes\nRefreshed by on-demand fetch.\n",
        )]);

        refresh_url_from_source(
            &paths,
            "https://shopify.dev/docs/apps/build/access-scopes",
            &source,
        )
        .await
        .unwrap();

        let refreshed = get_doc(&conn, "/docs/apps/build/access-scopes")
            .unwrap()
            .unwrap();
        assert_eq!(refreshed.source, "sitemap");
        assert_ne!(refreshed.content_sha, record.content_sha);
    }

    #[tokio::test]
    async fn on_demand_refetch_removes_stale_tantivy_terms_for_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        write_on_demand_config(&paths, true);
        let source = MockTextSource::new(&[(
            "https://shopify.dev/docs/apps/build/delta.md",
            "# Delta\nFresh replacement term.\n",
        )]);
        let conn = open_db(&paths).unwrap();
        init_db(&conn).unwrap();
        let old = store_source_doc(
            &paths,
            &SourceDoc {
                url: "https://shopify.dev/docs/apps/build/delta.md".to_string(),
                title_hint: Some("Delta".to_string()),
                content: "# Delta\nObsoleteUniqueTerm.\n".to_string(),
                source: "on_demand".to_string(),
            },
        )
        .unwrap();
        upsert_doc(&conn, &old).unwrap();
        rebuild_tantivy_from_db(&paths).unwrap();

        refresh_url_from_source(&paths, "https://shopify.dev/docs/apps/build/delta", &source)
            .await
            .unwrap();

        assert!(
            search_docs(&paths, "Fresh replacement", None, 5)
                .unwrap()
                .iter()
                .any(|doc| doc.path == "/docs/apps/build/delta")
        );
        assert!(
            search_docs(&paths, "ObsoleteUniqueTerm", None, 5)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn map_zero_results_suggests_allowed_on_demand_candidate_without_fetching() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "https://shopify.dev/docs/apps/build/missing".to_string(),
                radius: None,
                lens: None,
                version: None,
                max_nodes: Some(5),
            },
        )
        .unwrap();

        let candidate = response
            .meta
            .on_demand_candidate
            .expect("allowed URL-like zero result should produce candidate metadata");
        assert_eq!(candidate.url, "https://shopify.dev/docs/apps/build/missing");
        assert!(!candidate.enabled);
        let conn = open_db(&paths).unwrap();
        init_db(&conn).unwrap();
        assert_eq!(count_docs(&conn).unwrap(), 0);
    }

    #[tokio::test]
    async fn map_zero_results_does_not_suggest_disallowed_on_demand_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "https://shopify.dev/partners".to_string(),
                radius: None,
                lens: None,
                version: None,
                max_nodes: Some(5),
            },
        )
        .unwrap();

        assert!(response.meta.on_demand_candidate.is_none());
    }

    #[tokio::test]
    async fn map_nonzero_results_do_not_add_on_demand_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        fs::create_dir_all(&paths.raw).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn).unwrap();
        let source = SourceDoc {
            url: "https://shopify.dev/docs/apps/build/access-scopes.md".to_string(),
            title_hint: Some("Access scopes".to_string()),
            content: "# Access scopes\nLocal indexed text.\n".to_string(),
            source: "sitemap".to_string(),
        };
        let record = store_source_doc(&paths, &source).unwrap();
        upsert_doc(&conn, &record).unwrap();
        rebuild_tantivy_from_db(&paths).unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "/docs/apps/build/access-scopes".to_string(),
                radius: None,
                lens: None,
                version: None,
                max_nodes: Some(5),
            },
        )
        .unwrap();

        assert!(!response.nodes.is_empty());
        assert!(response.meta.on_demand_candidate.is_none());
    }

    #[tokio::test]
    async fn coverage_repair_retries_failed_allowed_rows_and_skips_policy_rows() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        write_on_demand_config(&paths, true);
        let conn = open_db(&paths).unwrap();
        init_db(&conn).unwrap();
        insert_coverage_event(
            &conn,
            &CoverageEvent {
                source: "sitemap".to_string(),
                canonical_path: Some("/docs/apps/build/repairable".to_string()),
                source_url: "https://shopify.dev/docs/apps/build/repairable".to_string(),
                status: "failed".to_string(),
                reason: Some("network_error".to_string()),
                http_status: None,
                checked_at: "2026-04-17T00:00:00Z".to_string(),
            },
        )
        .unwrap();
        insert_coverage_event(
            &conn,
            &CoverageEvent {
                source: "sitemap".to_string(),
                canonical_path: None,
                source_url: "https://shopify.dev/partners".to_string(),
                status: "failed".to_string(),
                reason: Some("outside_scope".to_string()),
                http_status: None,
                checked_at: "2026-04-17T00:00:00Z".to_string(),
            },
        )
        .unwrap();
        let source = MockTextSource::new(&[(
            "https://shopify.dev/docs/apps/build/repairable.md",
            "# Repairable\nRecovered through coverage repair.\n",
        )]);

        let summary = coverage_repair_from_source(&paths, &source).await.unwrap();

        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.repaired, 1);
        assert_eq!(summary.skipped_policy, 1);
        assert_eq!(
            get_doc(&conn, "/docs/apps/build/repairable")
                .unwrap()
                .unwrap()
                .source,
            "on_demand"
        );
        assert_eq!(
            count_where(
                &conn,
                "coverage_reports",
                "source_url = 'https://shopify.dev/docs/apps/build/repairable' AND status = 'indexed'"
            )
            .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn coverage_repair_disabled_skips_allowed_rows_without_fetching() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn).unwrap();
        insert_coverage_event(
            &conn,
            &CoverageEvent {
                source: "sitemap".to_string(),
                canonical_path: Some("/docs/apps/build/disabled-repair".to_string()),
                source_url: "https://shopify.dev/docs/apps/build/disabled-repair".to_string(),
                status: "failed".to_string(),
                reason: Some("network_error".to_string()),
                http_status: None,
                checked_at: "2026-04-17T00:00:00Z".to_string(),
            },
        )
        .unwrap();
        let source = MockTextSource::new(&[(
            "https://shopify.dev/docs/apps/build/disabled-repair.md",
            "# Should not fetch\n",
        )]);

        let summary = coverage_repair_from_source(&paths, &source).await.unwrap();

        assert_eq!(summary.attempted, 0);
        assert_eq!(summary.skipped_disabled, 1);
        assert!(
            get_doc(&conn, "/docs/apps/build/disabled-repair")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn coverage_status_counts_report_rows() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn).unwrap();
        insert_coverage_event(
            &conn,
            &CoverageEvent {
                source: "sitemap".to_string(),
                canonical_path: Some("/docs/apps/build/access-scopes".to_string()),
                source_url: "https://shopify.dev/docs/apps/build/access-scopes".to_string(),
                status: "skipped".to_string(),
                reason: Some("markdown_not_found".to_string()),
                http_status: Some(404),
                checked_at: "2026-04-17T00:00:00Z".to_string(),
            },
        )
        .unwrap();

        let coverage = coverage_status(&conn).unwrap();
        assert_eq!(coverage.discovered_count, 1);
        assert_eq!(coverage.skipped_count, 1);
        assert_eq!(coverage.sources.sitemap, 1);
    }

    #[test]
    fn sitemap_sourced_optional_scopes_doc_is_searchable() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        fs::create_dir_all(&paths.raw).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn).unwrap();
        let source = SourceDoc {
            url: "https://shopify.dev/docs/apps/build/access-scopes.md".to_string(),
            title_hint: Some("Access scopes".to_string()),
            content: "# Access scopes\nOptional access scopes and managed access scopes let apps request protected permissions.\n".to_string(),
            source: "sitemap".to_string(),
        };
        let record = store_source_doc(&paths, &source).unwrap();
        upsert_doc(&conn, &record).unwrap();
        rebuild_tantivy_from_db(&paths).unwrap();

        let results = search_docs(&paths, "optional scopes", None, 5).unwrap();
        assert_eq!(results[0].path, "/docs/apps/build/access-scopes");
        assert_eq!(results[0].source, "sitemap");
    }

    #[tokio::test]
    async fn changelog_resolved_concept_marks_connected_doc_stale() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let conn = seed_draft_order_graph(&paths);
        let feed = changelog_feed(
            "DraftOrderLineItem.grams field removed in 2026-07",
            "The DraftOrderLineItem.grams field will be removed. Migrate to weight.",
            "https://shopify.dev/changelog/draft-order-line-item-grams",
        );
        let source = MockTextSource::new(&[(SHOPIFY_CHANGELOG_FEED_URL, &feed)]);

        let report = poll_changelog_from_source(&paths, SHOPIFY_CHANGELOG_FEED_URL, &source)
            .await
            .unwrap();

        assert_eq!(report.entries_inserted, 1);
        assert_eq!(report.entries_seen, 1);
        assert!(report.warnings.is_empty());
        assert!(report.scheduled_changes > 0);
        let doc = get_doc(
            &conn,
            "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem",
        )
        .unwrap()
        .unwrap();
        assert!(doc.references_deprecated);
        assert!(
            doc.deprecated_refs
                .iter()
                .any(|reference| reference == "DraftOrderLineItem.grams")
        );

        let fetched = shopify_fetch(
            &paths,
            &FetchArgs {
                path: Some(
                    "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem".to_string(),
                ),
                url: None,
                anchor: None,
                include_code_blocks: None,
                max_chars: None,
            },
        )
        .await
        .unwrap();
        assert!(fetched.staleness.references_deprecated);
        assert!(
            fetched
                .staleness
                .upcoming_changes
                .iter()
                .any(|change| change["type_name"] == "DraftOrderLineItem.grams")
        );
    }

    #[tokio::test]
    async fn map_doc_node_includes_scheduled_change_staleness() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        seed_draft_order_graph(&paths);
        let feed = changelog_feed(
            "DraftOrderLineItem.grams field removed in 2026-07",
            "The DraftOrderLineItem.grams field will be removed. Migrate to weight.",
            "https://shopify.dev/changelog/draft-order-line-item-grams-map",
        );
        let source = MockTextSource::new(&[(SHOPIFY_CHANGELOG_FEED_URL, &feed)]);
        poll_changelog_from_source(&paths, SHOPIFY_CHANGELOG_FEED_URL, &source)
            .await
            .unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem".to_string(),
                radius: Some(1),
                lens: Some("doc".to_string()),
                version: Some("2026-04".to_string()),
                max_nodes: Some(10),
            },
        )
        .unwrap();

        let doc_node = response
            .nodes
            .iter()
            .find(|node| node.path == "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem")
            .expect("map response should include the affected doc node");
        assert!(doc_node.staleness.references_deprecated);
        assert!(doc_node.staleness.upcoming_changes.iter().any(|change| {
            change["type_name"] == "DraftOrderLineItem.grams"
                && change["change"] == "removal"
                && change["effective_date"] == "2026-07"
                && change["migration_hint"]
                    .as_str()
                    .is_some_and(|hint| hint.contains("Migrate to weight"))
        }));
    }

    #[tokio::test]
    async fn changelog_unknown_symbol_is_unresolved_and_does_not_mark_docs() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let conn = seed_draft_order_graph(&paths);
        let feed = changelog_feed(
            "UnknownType.foo field removed in 2026-07",
            "The UnknownType.foo field will be removed.",
            "https://shopify.dev/changelog/unknown-type-field",
        );
        let source = MockTextSource::new(&[(SHOPIFY_CHANGELOG_FEED_URL, &feed)]);

        poll_changelog_from_source(&paths, SHOPIFY_CHANGELOG_FEED_URL, &source)
            .await
            .unwrap();

        let unresolved: String = conn
            .query_row(
                "SELECT unresolved_affected_refs FROM changelog_entries",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            parse_json_string_vec(Some(&unresolved))
                .iter()
                .any(|reference| reference == "UnknownType.foo")
        );
        assert_eq!(count_where(&conn, "scheduled_changes", "1=1").unwrap(), 0);
        let doc = get_doc(
            &conn,
            "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem",
        )
        .unwrap()
        .unwrap();
        assert!(!doc.references_deprecated);
    }

    #[tokio::test]
    async fn changelog_doc_link_resolves_through_docs_and_edges() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let conn = seed_draft_order_graph(&paths);
        let feed = changelog_feed(
            "Draft order docs updated for a removal in 2026-07",
            "See https://shopify.dev/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem for the removal.",
            "https://shopify.dev/changelog/draft-order-doc-removal",
        );
        let source = MockTextSource::new(&[(SHOPIFY_CHANGELOG_FEED_URL, &feed)]);

        poll_changelog_from_source(&paths, SHOPIFY_CHANGELOG_FEED_URL, &source)
            .await
            .unwrap();

        let affected: String = conn
            .query_row("SELECT affected_types FROM changelog_entries", [], |row| {
                row.get(0)
            })
            .unwrap();
        let affected = parse_json_string_vec(Some(&affected));
        assert!(affected.iter().any(|reference| {
            reference == "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem"
        }));
        assert!(
            affected
                .iter()
                .any(|reference| reference == "DraftOrderLineItem.grams")
        );
        let doc = get_doc(
            &conn,
            "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem",
        )
        .unwrap()
        .unwrap();
        assert!(doc.references_deprecated);
    }

    #[tokio::test]
    async fn status_reports_freshness_workers_and_changelog_counts() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let _conn = seed_draft_order_graph(&paths);
        let feed = changelog_feed(
            "DraftOrderLineItem.grams field removed in 2026-07",
            "The DraftOrderLineItem.grams field will be removed.",
            "https://shopify.dev/changelog/draft-order-status",
        );
        let source = MockTextSource::new(&[(SHOPIFY_CHANGELOG_FEED_URL, &feed)]);

        poll_changelog_from_source(&paths, SHOPIFY_CHANGELOG_FEED_URL, &source)
            .await
            .unwrap();
        refresh_stale_docs_from_source(&paths, &source)
            .await
            .unwrap();
        let status = status(&paths).unwrap();

        assert_eq!(status.changelog.entry_count, 1);
        assert!(status.changelog.scheduled_change_count > 0);
        assert!(status.workers.last_changelog_at.is_some());
        assert!(status.workers.last_aging_sweep_at.is_some());
        assert_eq!(status.freshness.fresh_count, 1);
    }

    #[tokio::test]
    async fn status_reports_changelog_polling_warning() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = MockTextSource::new(&[(SHOPIFY_CHANGELOG_FEED_URL, "not an rss feed")]);

        let report = poll_changelog_from_source(&paths, SHOPIFY_CHANGELOG_FEED_URL, &source)
            .await
            .unwrap();

        assert_eq!(report.entries_inserted, 0);
        assert_eq!(report.warnings.len(), 1);
        let status = status(&paths).unwrap();
        assert!(status.changelog.last_warning.is_some());
        assert!(
            status
                .warnings
                .iter()
                .any(|warning| warning.contains("Changelog polling warning"))
        );
    }

    #[tokio::test]
    async fn version_watcher_enqueues_validated_unindexed_latest_version() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn).unwrap();
        let source = MockTextSource::new(&[
            (
                SHOPIFY_VERSIONING_URL,
                "Stable version Release date Supported until 2026-04 April 1 2026 2026-07 July 1 2026",
            ),
            (
                &admin_graphql_direct_proxy_url("2026-07"),
                minimal_introspection(),
            ),
        ]);

        let report = check_new_versions_from_source(&paths, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        assert_eq!(report.latest_candidate.as_deref(), Some("2026-07"));
        assert!(report.enqueued);
        assert!(get_meta(&conn, "last_version_check_at").unwrap().is_some());
        assert_eq!(
            count_where(
                &conn,
                "version_rebuild_queue",
                "version = '2026-07' AND api_surface = 'admin_graphql' AND status = 'pending'"
            )
            .unwrap(),
            1
        );
        let status = status(&paths).unwrap();
        assert!(
            status
                .warnings
                .iter()
                .any(|warning| warning.contains("version rebuild request"))
        );
    }

    #[tokio::test]
    async fn version_watcher_does_not_enqueue_already_indexed_latest_version() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        fs::create_dir_all(&paths.raw).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn).unwrap();
        let source_doc = SourceDoc {
            url: "https://shopify.dev/docs/api/admin-graphql/2026-07/objects/Product.md"
                .to_string(),
            title_hint: Some("Product".to_string()),
            content: "# Product\nCurrent product reference.\n".to_string(),
            source: "sitemap".to_string(),
        };
        let record = store_source_doc(&paths, &source_doc).unwrap();
        upsert_doc(&conn, &record).unwrap();
        refresh_indexed_versions(&conn, &[record]).unwrap();
        let source = MockTextSource::new(&[(
            SHOPIFY_VERSIONING_URL,
            "Stable version Release date Supported until 2026-04 April 1 2026 2026-07 July 1 2026",
        )]);

        let report = check_new_versions_from_source(&paths, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        assert_eq!(report.latest_candidate.as_deref(), Some("2026-07"));
        assert!(report.already_indexed);
        assert!(!report.enqueued);
        assert_eq!(
            count_where(&conn, "version_rebuild_queue", "1=1").unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn version_watcher_rejects_html_only_candidate_without_schema_validation() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn).unwrap();
        let source = MockTextSource::new(&[(
            SHOPIFY_VERSIONING_URL,
            "Stable version Release date Supported until 2026-10 October 1 2026",
        )]);

        let report = check_new_versions_from_source(&paths, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        assert_eq!(report.latest_candidate, None);
        assert!(!report.enqueued);
        assert!(report.warning.is_some());
        assert_eq!(
            count_where(&conn, "version_rebuild_queue", "1=1").unwrap(),
            0
        );
    }

    #[test]
    fn reads_newline_delimited_mcp_message() {
        let input = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}
"#;
        let mut reader = std::io::BufReader::new(&input[..]);
        let message = read_mcp_message(&mut reader).unwrap().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&message).unwrap();
        assert_eq!(value["method"], "tools/list");
    }

    #[test]
    fn writes_newline_delimited_mcp_message() {
        let mut output = Vec::new();
        write_mcp_message(
            &mut output,
            &serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}}),
        )
        .unwrap();
        assert!(output.ends_with(b"\n"));
        assert!(!output.starts_with(b"Content-Length:"));
    }

    #[tokio::test]
    async fn mcp_initialize_tools_status_sequence_is_fast() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let started = std::time::Instant::now();
        let initialize = handle_mcp_request(
            &paths,
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        )
        .await;
        let elapsed = started.elapsed();
        assert!(elapsed.as_millis() < 20);
        assert_eq!(
            initialize["result"]["serverInfo"]["name"],
            "shopify-rextant"
        );

        let tools = handle_mcp_request(
            &paths,
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await;
        assert_eq!(tools["result"]["tools"][0]["name"], "shopify_map");

        let status = handle_mcp_request(
            &paths,
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":3,
                "method":"tools/call",
                "params":{"name":"shopify_status","arguments":{}}
            }),
        )
        .await;
        assert_eq!(
            status["result"]["structuredContent"]["schema_version"],
            SCHEMA_VERSION
        );
    }

    #[tokio::test]
    async fn full_build_uses_llms_and_sitemap_union_from_mock_sources() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = fixture_sources();

        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let conn = open_db(&paths).unwrap();
        assert_eq!(count_docs(&conn).unwrap(), 3);
        let admin_root = get_doc(&conn, "/docs/api/admin-graphql").unwrap().unwrap();
        assert_eq!(admin_root.source, "llms");
        let access_scopes = get_doc(&conn, "/docs/apps/build/access-scopes")
            .unwrap()
            .unwrap();
        assert_eq!(access_scopes.source, "sitemap");

        let coverage = coverage_status(&conn).unwrap();
        assert_eq!(coverage.discovered_count, 2);
        assert_eq!(coverage.indexed_count, 2);
        assert_eq!(coverage.sources.llms, 1);
        assert_eq!(coverage.sources.sitemap, 1);

        let results = search_docs(&paths, "optional scopes", None, 5).unwrap();
        assert_eq!(results[0].path, "/docs/apps/build/access-scopes");
    }

    #[tokio::test]
    async fn search_docs_uses_lindera_for_japanese_queries_without_regressing_english() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = admin_graphql_graph_sources();

        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let japanese_results = search_docs(&paths, "割引 クーポン", Some("2026-04"), 5).unwrap();
        assert!(
            japanese_results
                .iter()
                .any(|doc| doc.path == "/docs/apps/build/discounts/cart-level"),
            "Japanese discount/coupon query should reach the cart-level discount doc: {:?}",
            japanese_results
                .iter()
                .map(|doc| doc.path.as_str())
                .collect::<Vec<_>>()
        );

        let english_results = search_docs(&paths, "Product", Some("2026-04"), 5).unwrap();
        assert!(
            english_results
                .iter()
                .any(|doc| doc.path == "/docs/api/admin-graphql/2026-04/objects/Product"),
            "English type-name search should continue to reach Product docs: {:?}",
            english_results
                .iter()
                .map(|doc| doc.path.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn build_recreates_legacy_tantivy_index_when_search_schema_changes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        fs::create_dir_all(&paths.tantivy).unwrap();
        let mut legacy_builder = Schema::builder();
        legacy_builder.add_text_field("path", STRING | STORED);
        legacy_builder.add_text_field("title", TEXT | STORED);
        legacy_builder.add_text_field("url", STRING | STORED);
        legacy_builder.add_text_field("version", STRING | STORED);
        legacy_builder.add_text_field("api_surface", STRING | STORED);
        legacy_builder.add_text_field("doc_type", STRING | STORED);
        legacy_builder.add_text_field("content", TEXT);
        let legacy_index = Index::create_in_dir(&paths.tantivy, legacy_builder.build()).unwrap();
        legacy_index
            .writer::<TantivyDocument>(50_000_000)
            .unwrap()
            .commit()
            .unwrap();
        assert!(
            SearchFields::from_schema(&legacy_index.schema()).is_err(),
            "legacy index fixture should not contain v0.4 content_en/content_ja fields"
        );

        let source = admin_graphql_graph_sources();
        build_index_from_sources(&paths, false, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let index = Index::open_in_dir(&paths.tantivy).unwrap();
        assert!(
            SearchFields::from_schema(&index.schema()).is_ok(),
            "build should recreate old Tantivy indexes with the current search schema"
        );
        let japanese_results = search_docs(&paths, "割引 クーポン", Some("2026-04"), 5).unwrap();
        assert!(
            japanese_results
                .iter()
                .any(|doc| doc.path == "/docs/apps/build/discounts/cart-level"),
            "recreated index should support Japanese search"
        );
    }

    #[tokio::test]
    async fn full_build_indexes_admin_graphql_concepts_edges_and_status_graph_counts() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = admin_graphql_graph_sources();

        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let schema_snapshot = paths
            .data
            .join("schemas/admin-graphql/2026-04.introspection.json");
        assert!(
            schema_snapshot.exists(),
            "v0.2.0 build must persist the Admin GraphQL introspection snapshot at {}",
            schema_snapshot.display()
        );

        let conn = open_db(&paths).unwrap();
        let concept_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM concepts", [], |row| row.get(0))
            .unwrap();
        let edge_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))
            .unwrap();
        assert!(
            concept_count >= 3,
            "expected Product, ProductVariant, and ProductInput concepts, got {concept_count}"
        );
        assert!(
            edge_count >= 3,
            "expected defined_in/has_field/returns edges, got {edge_count}"
        );

        let status_json = to_json_value(status(&paths).unwrap());
        assert!(
            status_json
                .pointer("/index/concept_count")
                .and_then(Value::as_i64)
                .is_some_and(|count| count > 0),
            "shopify_status must expose index.concept_count: {status_json:#}"
        );
        assert!(
            status_json
                .pointer("/index/edge_count")
                .and_then(Value::as_i64)
                .is_some_and(|count| count > 0),
            "shopify_status must expose index.edge_count: {status_json:#}"
        );
    }

    #[tokio::test]
    async fn full_build_records_missing_raw_markdown_as_skipped_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = MockTextSource::new(&[
            (SHOPIFY_LLMS_URL, ""),
            (
                SHOPIFY_SITEMAP_URL,
                r#"
                <urlset>
                  <url><loc>https://shopify.dev/docs/apps/build/html-only</loc></url>
                </urlset>
                "#,
            ),
        ]);

        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let response = status(&paths).unwrap();
        assert_eq!(response.doc_count, 1);
        assert_eq!(response.coverage.discovered_count, 1);
        assert_eq!(response.coverage.skipped_count, 1);
        assert_eq!(response.coverage.failed_count, 0);
        assert_eq!(response.coverage.sources.sitemap, 1);
        assert!(response.coverage.last_sitemap_at.is_some());
        assert!(
            response
                .warnings
                .iter()
                .any(|warning| warning.contains("skipped because raw markdown was unavailable"))
        );
    }

    #[tokio::test]
    async fn refresh_without_path_sweeps_only_aging_or_stale_docs() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        fs::create_dir_all(&paths.raw).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn).unwrap();
        let fresh_source = SourceDoc {
            url: "https://shopify.dev/docs/apps/build/fresh.md".to_string(),
            title_hint: Some("Fresh doc".to_string()),
            content: "# Fresh doc\nCurrent content.\n".to_string(),
            source: "sitemap".to_string(),
        };
        let stale_source = SourceDoc {
            url: "https://shopify.dev/docs/apps/build/stale.md".to_string(),
            title_hint: Some("Stale doc".to_string()),
            content: "# Stale doc\nOld content.\n".to_string(),
            source: "sitemap".to_string(),
        };
        let fresh = store_source_doc(&paths, &fresh_source).unwrap();
        let stale = store_source_doc(&paths, &stale_source).unwrap();
        let fresh_sha = fresh.content_sha.clone();
        upsert_doc(&conn, &fresh).unwrap();
        upsert_doc(&conn, &stale).unwrap();
        let old_verified = (Utc::now() - chrono::Duration::days(40)).to_rfc3339();
        conn.execute(
            "UPDATE docs SET last_verified = ?1, freshness = 'stale' WHERE path = ?2",
            params![old_verified, stale.path],
        )
        .unwrap();
        rebuild_tantivy_from_db(&paths).unwrap();
        let source = MockTextSource::new(&[(
            "https://shopify.dev/docs/apps/build/stale.md",
            "# Stale doc\nRefreshed content.\n",
        )]);

        refresh_stale_docs_from_source(&paths, &source)
            .await
            .unwrap();

        let fresh_after = get_doc(&conn, "/docs/apps/build/fresh").unwrap().unwrap();
        let stale_after = get_doc(&conn, "/docs/apps/build/stale").unwrap().unwrap();
        assert_eq!(fresh_after.content_sha, fresh_sha);
        assert_ne!(stale_after.content_sha, stale.content_sha);
        assert!(get_meta(&conn, "last_aging_sweep_at").unwrap().is_some());
    }

    #[tokio::test]
    async fn map_response_exposes_fts_contract_for_zero_results() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = fixture_sources();
        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "no-local-doc-should-match-this".to_string(),
                radius: None,
                lens: None,
                version: None,
                max_nodes: Some(5),
            },
        )
        .unwrap();

        assert!(!response.meta.graph_available);
        assert_eq!(response.meta.query_interpretation.resolved_as, "free_text");
        assert_eq!(response.meta.query_interpretation.confidence, "low");
        assert!(response.meta.query_interpretation.entry_points.is_empty());
        assert!(response.nodes.is_empty());
        assert!(response.edges.is_empty());
        assert_eq!(response.query_plan[0].action, "inspect_status");
        assert_eq!(response.query_plan[1].action, "refresh");
    }

    #[tokio::test]
    async fn map_product_returns_graph_backed_concept_map() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = admin_graphql_graph_sources();
        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "Product".to_string(),
                radius: Some(2),
                lens: Some("concept".to_string()),
                version: Some("2026-04".to_string()),
                max_nodes: Some(20),
            },
        )
        .unwrap();

        assert!(
            response.meta.graph_available,
            "v0.2.0 Product map must be graph-backed"
        );
        assert_eq!(
            response.meta.query_interpretation.resolved_as,
            "concept_name"
        );
        assert_eq!(response.center.kind, "concept");
        assert!(
            response.center.id.contains("Product"),
            "center should be the Product concept, got {:?}",
            response.center.id
        );
        assert!(
            !response.edges.is_empty(),
            "Product concept map should include graph edges"
        );
        assert!(
            response
                .suggested_reading_order
                .iter()
                .any(|path| path == "/docs/api/admin-graphql/2026-04/objects/Product"),
            "reading order must include the Product reference doc: {:?}",
            response.suggested_reading_order
        );
    }

    #[tokio::test]
    async fn map_doc_path_reaches_product_concept() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = admin_graphql_graph_sources();
        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "/docs/api/admin-graphql/2026-04/objects/Product".to_string(),
                radius: Some(2),
                lens: Some("doc".to_string()),
                version: Some("2026-04".to_string()),
                max_nodes: Some(20),
            },
        )
        .unwrap();

        assert!(
            response.meta.graph_available,
            "doc path maps should use the graph when Admin GraphQL concepts are indexed"
        );
        assert_eq!(response.meta.query_interpretation.resolved_as, "doc_path");
        assert_eq!(response.center.kind, "doc");
        let product_concept = response
            .nodes
            .iter()
            .find(|node| node.kind == "concept" && node.id.contains("Product"))
            .expect("Product reference doc should reach the Product concept");
        let product_doc_path = "/docs/api/admin-graphql/2026-04/objects/Product";
        assert!(response.edges.iter().any(|edge| {
            let from = edge.get("from").and_then(Value::as_str);
            let to = edge.get("to").and_then(Value::as_str);
            (from == Some(product_doc_path) && to == Some(product_concept.id.as_str()))
                || (from == Some(product_concept.id.as_str()) && to == Some(product_doc_path))
        }));
    }

    #[tokio::test]
    async fn map_free_text_promotes_docs_to_graph_entry_points() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = admin_graphql_graph_sources();
        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "discount function cart level".to_string(),
                radius: Some(2),
                lens: Some("auto".to_string()),
                version: Some("2026-04".to_string()),
                max_nodes: Some(20),
            },
        )
        .unwrap();

        assert!(
            response.meta.graph_available,
            "free text maps should promote FTS docs into graph entry points when possible"
        );
        assert_eq!(response.meta.query_interpretation.resolved_as, "free_text");
        assert!(
            !response.meta.query_interpretation.entry_points.is_empty(),
            "free text maps should report the FTS entry points used for graph expansion"
        );
        assert!(
            response
                .nodes
                .iter()
                .any(|node| node.path == "/docs/apps/build/discounts/cart-level"),
            "promoted discount cart-level doc should remain in the result"
        );
        assert!(
            response
                .nodes
                .iter()
                .any(|node| node.path == "/docs/apps/build/discounts/overview"),
            "unpromoted FTS docs should not be dropped from the result"
        );
        assert!(
            !response.edges.is_empty(),
            "promoted free text maps should include graph edges"
        );
    }

    #[tokio::test]
    async fn map_without_schema_falls_back_to_fts_with_graph_warning() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = fixture_sources();
        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "Admin GraphQL".to_string(),
                radius: Some(2),
                lens: Some("auto".to_string()),
                version: None,
                max_nodes: Some(5),
            },
        )
        .unwrap();

        assert!(!response.meta.graph_available);
        assert!(
            response.meta.coverage_warning.is_some(),
            "fallback maps should explain that graph coverage or schema data is missing"
        );
        assert_eq!(
            response.query_plan.first().map(|step| step.action.as_str()),
            Some("inspect_status"),
            "fallback maps should ask the agent to inspect status before trusting graph coverage"
        );
    }

    #[tokio::test]
    async fn graph_edges_only_reference_returned_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = admin_graphql_graph_sources();
        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "Product".to_string(),
                radius: Some(1),
                lens: Some("concept".to_string()),
                version: Some("2026-04".to_string()),
                max_nodes: Some(5),
            },
        )
        .unwrap();

        assert!(
            !response.edges.is_empty(),
            "edge closure can only be checked when graph edges are returned"
        );
        let returned_ids = response
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        for edge in &response.edges {
            let from = edge
                .get("from")
                .and_then(Value::as_str)
                .expect("edge.from must be a string");
            let to = edge
                .get("to")
                .and_then(Value::as_str)
                .expect("edge.to must be a string");
            assert!(
                returned_ids.contains(from),
                "edge.from must reference a returned node: {edge:#}"
            );
            assert!(
                returned_ids.contains(to),
                "edge.to must reference a returned node: {edge:#}"
            );
        }
    }

    #[tokio::test]
    async fn reading_order_contains_only_doc_paths() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = admin_graphql_graph_sources();
        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "Product".to_string(),
                radius: Some(2),
                lens: Some("concept".to_string()),
                version: Some("2026-04".to_string()),
                max_nodes: Some(20),
            },
        )
        .unwrap();

        assert!(
            response.meta.graph_available,
            "reading order should be checked on a graph-backed map"
        );
        assert!(
            !response.suggested_reading_order.is_empty(),
            "graph-backed maps should suggest source docs to read"
        );
        assert!(
            response
                .suggested_reading_order
                .iter()
                .all(|path| path.starts_with("/docs/") || path.starts_with("/changelog/")),
            "suggested_reading_order should contain only doc paths: {:?}",
            response.suggested_reading_order
        );
    }

    #[tokio::test]
    async fn build_persists_and_reuses_graph_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = admin_graphql_graph_sources();
        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let graph_snapshot = paths.data.join("graph.msgpack");
        assert!(
            graph_snapshot.exists(),
            "graph-backed builds must persist {}",
            graph_snapshot.display()
        );

        let started = std::time::Instant::now();
        let initialize = handle_mcp_request(
            &paths,
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        )
        .await;
        let tools = handle_mcp_request(
            &paths,
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await;
        let status = handle_mcp_request(
            &paths,
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":3,
                "method":"tools/call",
                "params":{"name":"shopify_status","arguments":{}}
            }),
        )
        .await;
        let elapsed = started.elapsed();

        assert_eq!(
            initialize["result"]["serverInfo"]["name"],
            "shopify-rextant"
        );
        assert_eq!(tools["result"]["tools"][0]["name"], "shopify_map");
        assert!(
            status["result"]["structuredContent"]
                .pointer("/index/edge_count")
                .and_then(Value::as_i64)
                .is_some_and(|count| count > 0),
            "status should expose graph counts after loading the graph snapshot: {status:#}"
        );
        assert!(
            elapsed.as_millis() < 20,
            "MCP initialize/tools/status should stay fast with graph snapshot present; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn fetch_response_applies_anchor_code_filter_and_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        fs::create_dir_all(&paths.raw).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn).unwrap();
        let source = SourceDoc {
            url: "https://shopify.dev/docs/apps/build/access-scopes.md".to_string(),
            title_hint: Some("Access scopes".to_string()),
            content: "# Access scopes\nIntro\n\n## Managed access scopes\nText before code.\n```graphql\nquery { shop { name } }\n```\nMore text after code that keeps this section long enough to truncate.\n\n## Next\nDone\n".to_string(),
            source: "sitemap".to_string(),
        };
        let record = store_source_doc(&paths, &source).unwrap();
        upsert_doc(&conn, &record).unwrap();

        let response = shopify_fetch(
            &paths,
            &FetchArgs {
                path: Some("/docs/apps/build/access-scopes".to_string()),
                url: None,
                anchor: Some("managed-access-scopes".to_string()),
                include_code_blocks: Some(false),
                max_chars: Some(80),
            },
        )
        .await
        .unwrap();

        assert!(response.sections.iter().any(|section| {
            section.anchor == "managed-access-scopes" && section.title == "Managed access scopes"
        }));
        assert!(response.content.contains("## Managed access scopes"));
        assert!(response.content.contains("Text before code."));
        assert!(!response.content.contains("query { shop"));
        assert!(!response.content.contains("## Next"));
        assert!(response.truncated);
        assert!(response.content.chars().count() <= 80);
    }

    #[test]
    fn reads_content_length_framed_mcp_message() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let mut input = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        input.extend_from_slice(body);
        let mut reader = std::io::BufReader::new(&input[..]);

        let message = read_mcp_message(&mut reader).unwrap().unwrap();
        assert_eq!(message, body);
        let value: serde_json::Value = serde_json::from_slice(&message).unwrap();
        assert_eq!(value["method"], "initialize");
    }
}
