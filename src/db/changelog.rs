use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use super::super::changelog::types::ScheduledChangeRecord;

pub(crate) fn changelog_entry_exists(conn: &Connection, id: &str) -> Result<bool> {
    conn.query_row(
        "SELECT 1 FROM changelog_entries WHERE id = ?1",
        params![id],
        |_| Ok(()),
    )
    .optional()
    .map(|value| value.is_some())
    .map_err(Into::into)
}

pub(crate) fn insert_scheduled_change(
    conn: &Connection,
    change: &ScheduledChangeRecord,
) -> Result<()> {
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
