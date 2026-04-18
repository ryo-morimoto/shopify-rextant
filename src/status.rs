use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use super::db::docs::{all_docs, count_docs, count_where, parse_json_string_vec};
use super::db::meta::get_meta;
use super::db::schema::{init_db, open_db};
use super::doc_freshness::staleness;
use super::domain::status::{
    ChangelogStatus, CoverageSources, CoverageStatus, FreshnessStatus, GraphIndexStatus,
    StatusResponse, WorkerStatus,
};
use super::{Paths, SCHEMA_VERSION};

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

pub(crate) fn coverage_status(conn: &Connection) -> Result<CoverageStatus> {
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

pub(crate) fn update_doc_freshness_states(conn: &Connection) -> Result<()> {
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

pub(crate) fn versions_available(paths: &Paths) -> Result<Vec<String>> {
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
