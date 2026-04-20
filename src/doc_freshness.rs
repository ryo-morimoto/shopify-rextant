use std::collections::HashSet;

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::domain::concepts::ConceptRecord;
use super::domain::docs::DocRecord;
use super::domain::map::Staleness;

pub(crate) fn staleness(doc: &DocRecord) -> Staleness {
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

pub(crate) fn staleness_for_doc(conn: &Connection, doc: &DocRecord) -> Result<Staleness> {
    let mut staleness = staleness(doc);
    staleness.upcoming_changes = scheduled_changes_for_refs(conn, &doc.deprecated_refs)?;
    if !staleness.upcoming_changes.is_empty() {
        staleness.references_deprecated = true;
    }
    Ok(staleness)
}

pub(crate) fn scheduled_changes_for_concept(
    conn: &Connection,
    concept: &ConceptRecord,
) -> Result<Vec<Value>> {
    let mut refs = vec![concept.id.clone(), concept.name.clone()];
    refs.sort();
    refs.dedup();
    scheduled_changes_for_refs(conn, &refs)
}

pub(crate) fn scheduled_changes_for_refs(conn: &Connection, refs: &[String]) -> Result<Vec<Value>> {
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
