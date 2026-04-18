use std::fs;

use anyhow::{Context, Result};
use rusqlite::params;

use super::changelog::poll::poll_changelog_from_source;
use super::db::concepts::insert_concept;
use super::db::coverage::insert_coverage_event;
use super::db::docs::{refresh_indexed_versions, upsert_doc};
use super::db::graph::insert_edge;
use super::db::meta::set_meta;
use super::db::schema::{clear_coverage_reports, clear_graph_tables, init_db, open_db};
use super::domain::coverage::CoverageEvent;
use super::domain::source::SourceDoc;
use super::graphql::build::{build_admin_graphql_graph, persist_graph_snapshot};
use super::markdown::{dedupe_links_by_path, parse_markdown_links, parse_sitemap_links};
use super::search::index_io::{add_tantivy_doc, create_or_reset_index};
use super::search::schema::{SearchFields, search_schema};
use super::search::tokenizer::register_japanese_tokenizer;
use super::source::reqwest_source::ReqwestTextSource;
use super::source::text_source::TextSource;
use super::source_sync::{fetch_required_text, fetch_source_doc, store_source_doc};
use super::url_policy::is_indexable_shopify_url;
use super::util::time::now_iso;
use super::{IndexSourceUrls, Paths, SCHEMA_VERSION};

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
