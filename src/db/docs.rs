use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;

use super::super::DocRecord;
use super::super::util::time::now_iso;

pub(crate) fn upsert_doc(conn: &Connection, doc: &DocRecord) -> Result<()> {
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

pub(crate) fn get_doc(conn: &Connection, path: &str) -> Result<Option<DocRecord>> {
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

pub(crate) fn all_docs(conn: &Connection) -> Result<Vec<DocRecord>> {
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

pub(crate) fn count_docs(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM docs", [], |row| row.get(0))
        .map_err(Into::into)
}

pub(crate) fn count_where(conn: &Connection, table: &str, where_clause: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {where_clause}");
    conn.query_row(&sql, [], |row| row.get(0))
        .map_err(Into::into)
}

pub(crate) fn stale_refresh_candidates(
    conn: &Connection,
    limit: usize,
) -> Result<Vec<DocRecord>> {
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

pub(crate) fn mark_docs_deprecated(
    conn: &Connection,
    doc_paths: &[String],
    refs: &[String],
) -> Result<()> {
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

pub(crate) fn refresh_indexed_versions(conn: &Connection, docs: &[DocRecord]) -> Result<()> {
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

pub(crate) fn indexed_version_exists(
    conn: &Connection,
    version: &str,
    api_surface: &str,
) -> Result<bool> {
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

pub(crate) fn enqueue_version_rebuild(
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

pub(crate) fn doc_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DocRecord> {
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

pub(crate) fn parse_json_string_vec(value: Option<&str>) -> Vec<String> {
    value
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .unwrap_or_default()
}
