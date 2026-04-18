use anyhow::Result;
use rusqlite::{Connection, params};

use super::super::CoverageEvent;
use super::super::on_demand::FetchCandidate as OnDemandFetchCandidate;
use super::super::util::time::now_iso;

#[derive(Debug)]
pub(crate) struct CoverageRepairRow {
    pub(crate) id: i64,
    pub(crate) source_url: String,
}

pub(crate) fn insert_coverage_event(conn: &Connection, event: &CoverageEvent) -> Result<()> {
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

pub(crate) fn failed_coverage_rows(conn: &Connection) -> Result<Vec<CoverageRepairRow>> {
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

pub(crate) fn update_coverage_repaired(
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

pub(crate) fn update_coverage_failed(conn: &Connection, id: i64, reason: &str) -> Result<()> {
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
