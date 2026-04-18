use anyhow::Result;
use rusqlite::{Connection, params};
use std::fs;

use super::super::Paths;

pub(crate) fn open_db(paths: &Paths) -> Result<Connection> {
    if let Some(parent) = paths.db.parent() {
        fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(&paths.db)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(conn)
}

pub(crate) fn init_db(conn: &Connection, schema_version: &str) -> Result<()> {
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
        params![schema_version],
    )?;
    Ok(())
}

pub(crate) fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
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

pub(crate) fn clear_coverage_reports(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM coverage_reports", [])?;
    Ok(())
}

pub(crate) fn clear_graph_tables(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM edges", [])?;
    conn.execute("DELETE FROM concepts", [])?;
    conn.execute("DELETE FROM tasks", [])?;
    Ok(())
}
