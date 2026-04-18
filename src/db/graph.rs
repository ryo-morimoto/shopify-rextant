use anyhow::Result;
use rusqlite::{Connection, params};

use super::super::domain::graph::GraphEdgeRecord;
use super::super::util::time::now_iso;

pub(crate) fn insert_edge(conn: &Connection, edge: &GraphEdgeRecord) -> Result<()> {
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

pub(crate) fn load_edges(conn: &Connection) -> Result<Vec<GraphEdgeRecord>> {
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
