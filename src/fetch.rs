use std::fs;

use anyhow::{Result, anyhow};

use super::config::{ensure_on_demand_enabled, load_config, policy_from_config};
use super::db::coverage::insert_coverage_event;
use super::db::docs::{count_docs, get_doc, upsert_doc};
use super::db::schema::{init_db, open_db};
use super::doc_freshness::staleness_for_doc;
use super::domain::coverage::CoverageEvent;
use super::domain::docs::DocRecord;
use super::domain::fetch::FetchResponse;
use super::markdown::{MarkdownLink, extract_sections, remove_fenced_code_blocks, section_content};
use super::on_demand::{
    FetchCandidate as OnDemandFetchCandidate, FetchPolicy as OnDemandFetchPolicy,
    is_allowed_path as is_on_demand_allowed_path,
};
use super::search::index_io::upsert_tantivy_doc;
use super::source::reqwest_source::ReqwestTextSource;
use super::source::text_source::TextSource;
use super::source_sync::{fetch_source_doc, store_source_doc};
use super::{FetchArgs, Paths, SCHEMA_VERSION, ToolError};

pub(crate) async fn shopify_fetch(
    paths: &Paths,
    args: &FetchArgs,
) -> std::result::Result<FetchResponse, ToolError> {
    let source = ReqwestTextSource::new()?;
    shopify_fetch_from_source(paths, args, &source).await
}

pub(crate) async fn shopify_fetch_from_source<S: TextSource>(
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

pub(crate) async fn on_demand_fetch_from_input<S: TextSource>(
    paths: &Paths,
    input: &str,
    source: &S,
) -> std::result::Result<DocRecord, ToolError> {
    let candidate = OnDemandFetchPolicy::candidate_from_input(input)
        .map_err(|_| ToolError::outside_scope(input))?;
    let config = load_config(paths)?;
    let policy = policy_from_config(&config);
    ensure_on_demand_enabled(&policy, &candidate)?;
    on_demand_fetch_candidate(paths, candidate, source, true).await
}

pub(crate) async fn on_demand_fetch_candidate<S: TextSource>(
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
