mod changelog;
pub mod cli;
mod db;
mod domain;
mod graphql;
mod map;
mod markdown;
mod mcp;
mod mcp_framing;
mod on_demand;
mod search;
mod source;
mod url_policy;
mod util;

pub use cli::run;

use changelog::poll::{check_new_versions_from_source, poll_changelog_from_source};
use db::concepts::{find_concept_by_name, get_concept, insert_concept};
use db::coverage::{
    failed_coverage_rows, insert_coverage_event, update_coverage_failed, update_coverage_repaired,
};
use db::docs::{
    all_docs, count_docs, count_where, get_doc, parse_json_string_vec, refresh_indexed_versions,
    stale_refresh_candidates, upsert_doc,
};
use db::graph::{insert_edge, load_edges};
use db::meta::{get_meta, set_meta};
use db::schema::{clear_coverage_reports, clear_graph_tables, init_db, open_db};
use domain::concepts::ConceptRecord;
use domain::coverage::CoverageEvent;
use domain::graph::{GraphEdgeRecord, GraphNodeKey};
use domain::map::{
    GraphExpansion, MapCenter, MapIndexStatus, MapMeta, MapNode, OnDemandCandidate,
    QueryInterpretation, Staleness,
};
use domain::source::SourceDoc;
use domain::status::{
    ChangelogStatus, CoverageSources, CoverageStatus, FreshnessStatus, GraphIndexStatus,
    WorkerStatus,
};

pub(crate) use domain::docs::DocRecord;
pub(crate) use domain::fetch::FetchResponse;
pub(crate) use domain::map::MapResponse;
pub(crate) use domain::source::SourceFetchError;
pub(crate) use domain::status::StatusResponse;
use graphql::build::{build_admin_graphql_graph, persist_graph_snapshot};
#[cfg(test)]
use graphql::schema_urls::admin_graphql_direct_proxy_url;
use map::plan::{
    QueryPlanStep, dedupe_docs_by_path, doc_type_rank, graph_query_plan, is_doc_like_query,
    node_kind_rank,
};
use map::warnings::{index_age_days, map_coverage_warning};
use markdown::{
    MarkdownLink, dedupe_links_by_path, extract_sections, parse_markdown_links, parse_sitemap_links,
    remove_fenced_code_blocks, section_content, title_from_markdown,
};
use url_policy::{
    canonical_doc_path, classify_api_surface, classify_content_class, classify_doc_type,
    extract_version, is_indexable_shopify_url, raw_doc_candidates, raw_path_for, reading_time_min,
};
use util::hash::hex_sha256;
use util::json::merge_json_arrays;
#[cfg(test)]
use util::json::to_json_value;
use util::time::now_iso;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
#[cfg(test)]
use mcp_framing::{read_message as read_mcp_message, write_json as write_mcp_message};
use on_demand::{
    FetchCandidate as OnDemandFetchCandidate, FetchPolicy as OnDemandFetchPolicy,
    is_allowed_path as is_on_demand_allowed_path,
};
#[cfg(test)]
use mcp::daemon::{DaemonIdentity, DaemonPaths};
use mcp::protocol::json_rpc_error;
#[cfg(test)]
use mcp::protocol::handle_mcp_request;
use search::index_io::{
    add_tantivy_doc, create_or_reset_index, rebuild_tantivy_from_db, upsert_tantivy_doc,
};
use search::runtime::{SearchRuntime, sqlite_like_search};
use search::schema::{SearchFields, search_schema};
use search::tokenizer::register_japanese_tokenizer;
use source::reqwest_source::ReqwestTextSource;
pub(crate) use source::text_source::TextSource;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use tantivy::doc;

const SHOPIFY_LLMS_URL: &str = "https://shopify.dev/llms.txt";
const SHOPIFY_SITEMAP_URL: &str = "https://shopify.dev/sitemap.xml";
const SHOPIFY_CHANGELOG_FEED_URL: &str = "https://shopify.dev/changelog/feed.xml";
const SHOPIFY_VERSIONING_URL: &str = "https://shopify.dev/docs/api/usage/versioning";
pub(crate) const USER_AGENT: &str = concat!("shopify-rextant/", env!("CARGO_PKG_VERSION"));
pub(crate) const SCHEMA_VERSION: &str = "3";
pub(crate) const ADMIN_GRAPHQL_INTROSPECTION_QUERY: &str = r#"
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

#[derive(Debug, Clone)]
pub(crate) struct Paths {
    pub(crate) home: PathBuf,
    pub(crate) data: PathBuf,
    pub(crate) raw: PathBuf,
    pub(crate) tantivy: PathBuf,
    pub(crate) db: PathBuf,
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
    pub(crate) fn json_rpc_error(&self) -> Value {
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

    pub(crate) fn outside_scope(input: &str) -> Self {
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
pub(crate) struct SearchArgs {
    pub(crate) query: String,
    pub(crate) version: Option<String>,
    pub(crate) limit: Option<usize>,
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

    pub(crate) fn raw_file(&self, raw_path: &str) -> PathBuf {
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

pub(crate) async fn build_index(paths: &Paths, force: bool, limit: Option<usize>) -> Result<()> {
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
    init_db(&conn, SCHEMA_VERSION)?;
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

pub(crate) async fn refresh(paths: &Paths, path: Option<String>, url: Option<String>) -> Result<()> {
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

pub(crate) async fn coverage_repair(paths: &Paths) -> Result<CoverageRepairSummary> {
    let source = ReqwestTextSource::new()?;
    coverage_repair_from_source(paths, &source).await
}

async fn coverage_repair_from_source<S: TextSource>(
    paths: &Paths,
    source: &S,
) -> Result<CoverageRepairSummary> {
    let conn = open_db(paths)?;
    init_db(&conn, SCHEMA_VERSION)?;
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
    init_db(&conn, SCHEMA_VERSION)?;
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
    init_db(&conn, SCHEMA_VERSION)?;
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
    init_db(&conn, SCHEMA_VERSION)?;
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
    init_db(&conn, SCHEMA_VERSION)?;
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

pub(crate) fn shopify_map_with_runtime(
    paths: &Paths,
    args: &MapArgs,
    search_runtime: Option<&SearchRuntime>,
) -> Result<MapResponse> {
    let limit = args.max_nodes.unwrap_or(30).clamp(1, 100);
    let radius = args.radius.unwrap_or(2).clamp(1, 3);
    let lens = args.lens.as_deref().unwrap_or("auto");
    let conn = open_db(paths)?;
    init_db(&conn, SCHEMA_VERSION)?;
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
    init_db(&conn, SCHEMA_VERSION)?;
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

fn versions_available(paths: &Paths) -> Result<Vec<String>> {
    if !paths.db.exists() {
        return Ok(Vec::new());
    }
    let conn = open_db(paths)?;
    init_db(&conn, SCHEMA_VERSION)?;
    let mut stmt = conn.prepare(
        "SELECT DISTINCT COALESCE(version, 'evergreen') FROM docs ORDER BY COALESCE(version, 'evergreen')",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub(crate) fn search_docs(
    paths: &Paths,
    query: &str,
    version: Option<&str>,
    limit: usize,
) -> Result<Vec<DocRecord>> {
    search_docs_with_runtime(paths, None, query, version, limit)
}

pub(crate) fn search_docs_with_runtime(
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

#[cfg(test)]
mod tests {
    use super::*;
    use super::changelog::feed::parse_changelog_feed;
    #[allow(unused_imports)]
    use tantivy::Index;
    #[allow(unused_imports)]
    use tantivy::schema::{STORED, STRING, Schema, TEXT, TantivyDocument};

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
        init_db(&conn, SCHEMA_VERSION).unwrap();
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
        init_db(&conn, SCHEMA_VERSION).unwrap();

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
        init_db(&conn, SCHEMA_VERSION).unwrap();
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
        init_db(&conn, SCHEMA_VERSION).unwrap();
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
        init_db(&conn, SCHEMA_VERSION).unwrap();
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
        init_db(&conn, SCHEMA_VERSION).unwrap();
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
        init_db(&conn, SCHEMA_VERSION).unwrap();
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
        init_db(&conn, SCHEMA_VERSION).unwrap();
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
        init_db(&conn, SCHEMA_VERSION).unwrap();
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
        init_db(&conn, SCHEMA_VERSION).unwrap();
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
        init_db(&conn, SCHEMA_VERSION).unwrap();
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
        init_db(&conn, SCHEMA_VERSION).unwrap();
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
        init_db(&conn, SCHEMA_VERSION).unwrap();
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
        init_db(&conn, SCHEMA_VERSION).unwrap();
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
        init_db(&conn, SCHEMA_VERSION).unwrap();
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
