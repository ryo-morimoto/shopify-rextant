use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use super::super::domain::concepts::ConceptRecord;

pub(crate) fn get_concept(conn: &Connection, id: &str) -> Result<Option<ConceptRecord>> {
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

pub(crate) fn find_concept_by_name(
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

pub(crate) fn insert_concept(conn: &Connection, concept: &ConceptRecord) -> Result<()> {
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

pub(crate) fn concept_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConceptRecord> {
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
