use std::collections::{BTreeSet, HashSet, VecDeque};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde_json::Value;

use super::super::db::changelog::{changelog_entry_exists, insert_scheduled_change};
use super::super::db::concepts::{find_concept_by_name, get_concept};
use super::super::db::docs::{
    enqueue_version_rebuild, get_doc, indexed_version_exists, mark_docs_deprecated,
};
use super::super::db::graph::load_edges;
use super::super::db::meta::set_meta;
use super::super::db::schema::{init_db, open_db};
use super::super::domain::graph::GraphEdgeRecord;
use super::super::graphql::schema_urls::admin_graphql_direct_proxy_url;
use super::super::source::text_source::TextSource;
use super::super::util::time::now_iso;
use super::super::{IndexSourceUrls, Paths, SCHEMA_VERSION};
use super::feed::{parse_changelog_feed, version_candidates_desc};
use super::impact::{
    candidate_to_doc_path, extract_impact_candidates, impact_affected_types, is_api_version,
    looks_like_reference_candidate, scheduled_changes_from_entry, surface_from_category,
};
use super::types::{ChangelogEntryInput, ResolvedImpact};

#[derive(Debug, Default)]
pub(crate) struct ChangelogPollReport {
    pub(crate) entries_seen: usize,
    pub(crate) entries_inserted: usize,
    pub(crate) scheduled_changes: usize,
    pub(crate) warnings: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct VersionCheckReport {
    pub(crate) latest_candidate: Option<String>,
    pub(crate) already_indexed: bool,
    pub(crate) enqueued: bool,
    pub(crate) warning: Option<String>,
}

pub(crate) async fn poll_changelog_from_source<S: TextSource>(
    paths: &Paths,
    feed_url: &str,
    source: &S,
) -> Result<ChangelogPollReport> {
    let conn = open_db(paths)?;
    init_db(&conn, SCHEMA_VERSION)?;
    let feed = match source.fetch_text(feed_url).await {
        Ok(feed) => feed,
        Err(error) => {
            let warning = format!("GET {feed_url} failed: {}", error.reason);
            set_meta(&conn, "last_changelog_warning", &warning)?;
            return Ok(ChangelogPollReport {
                warnings: vec![warning],
                ..Default::default()
            });
        }
    };
    let entries = match parse_changelog_feed(&feed) {
        Ok(entries) => entries,
        Err(error) => {
            let warning = format!("parse changelog feed failed: {error}");
            set_meta(&conn, "last_changelog_warning", &warning)?;
            return Ok(ChangelogPollReport {
                warnings: vec![warning],
                ..Default::default()
            });
        }
    };
    let mut report = ChangelogPollReport {
        entries_seen: entries.len(),
        ..Default::default()
    };
    for entry in entries {
        if changelog_entry_exists(&conn, &entry.id)? {
            continue;
        }
        let impact = resolve_changelog_impact(&conn, &entry)?;
        let scheduled_changes = scheduled_changes_from_entry(&entry, &impact);
        insert_changelog_entry(&conn, &entry, &impact)?;
        for change in &scheduled_changes {
            insert_scheduled_change(&conn, change)?;
        }
        if !scheduled_changes.is_empty() {
            mark_docs_deprecated(&conn, &impact.doc_paths, &impact.refs)?;
        }
        report.entries_inserted += 1;
        report.scheduled_changes += scheduled_changes.len();
    }
    set_meta(&conn, "last_changelog_at", &now_iso())?;
    conn.execute(
        "DELETE FROM schema_meta WHERE key = 'last_changelog_warning'",
        [],
    )?;
    Ok(report)
}

pub(crate) async fn check_new_versions_from_source<S: TextSource>(
    paths: &Paths,
    source_urls: &IndexSourceUrls,
    source: &S,
) -> Result<VersionCheckReport> {
    let conn = open_db(paths)?;
    init_db(&conn, SCHEMA_VERSION)?;
    let mut report = VersionCheckReport::default();
    let versioning_page = match source.fetch_text(&source_urls.versioning).await {
        Ok(page) => page,
        Err(error) => {
            let warning = format!("GET {} failed: {}", source_urls.versioning, error.reason);
            set_meta(&conn, "last_version_check_at", &now_iso())?;
            set_meta(&conn, "last_version_check_warning", &warning)?;
            report.warning = Some(warning);
            return Ok(report);
        }
    };
    let candidates = version_candidates_desc(&versioning_page);
    if candidates.is_empty() {
        let warning = "no API version candidates found in versioning page".to_string();
        set_meta(&conn, "last_version_check_at", &now_iso())?;
        set_meta(&conn, "last_version_check_warning", &warning)?;
        report.warning = Some(warning);
        return Ok(report);
    };

    for candidate in candidates {
        if indexed_version_exists(&conn, &candidate, "admin_graphql")? {
            report.latest_candidate = Some(candidate);
            report.already_indexed = true;
            set_meta(&conn, "last_version_check_at", &now_iso())?;
            conn.execute(
                "DELETE FROM schema_meta WHERE key = 'last_version_check_warning'",
                [],
            )?;
            return Ok(report);
        }
        if validate_admin_graphql_version(source, &candidate).await? {
            enqueue_version_rebuild(
                &conn,
                &candidate,
                "admin_graphql",
                "latest validated Admin GraphQL version is not indexed",
            )?;
            report.latest_candidate = Some(candidate);
            report.enqueued = true;
            set_meta(&conn, "last_version_check_at", &now_iso())?;
            conn.execute(
                "DELETE FROM schema_meta WHERE key = 'last_version_check_warning'",
                [],
            )?;
            return Ok(report);
        }
    }

    let warning = "no API version candidates passed Admin GraphQL validation".to_string();
    set_meta(&conn, "last_version_check_warning", &warning)?;
    report.warning = Some(warning);
    set_meta(&conn, "last_version_check_at", &now_iso())?;
    Ok(report)
}

async fn validate_admin_graphql_version<S: TextSource>(source: &S, version: &str) -> Result<bool> {
    let url = admin_graphql_direct_proxy_url(version);
    let Ok(snapshot) = source.fetch_admin_graphql_introspection(&url).await else {
        return Ok(false);
    };
    let value: Value = serde_json::from_str(&snapshot)
        .with_context(|| format!("parse Admin GraphQL introspection for {version}"))?;
    Ok(value
        .pointer("/data/__schema/types")
        .and_then(Value::as_array)
        .is_some_and(|types| !types.is_empty()))
}

fn resolve_changelog_impact(
    conn: &Connection,
    entry: &ChangelogEntryInput,
) -> Result<ResolvedImpact> {
    let candidates = extract_impact_candidates(entry);
    let version_hint = candidates
        .iter()
        .find(|candidate| is_api_version(candidate))
        .cloned();
    let all_edges = load_edges(conn)?;
    let mut refs = BTreeSet::new();
    let mut doc_paths = BTreeSet::new();
    let mut concept_ids = BTreeSet::new();
    let mut surfaces = BTreeSet::new();
    let mut unresolved_refs = BTreeSet::new();

    for category in &entry.categories {
        if let Some(surface) = surface_from_category(category) {
            surfaces.insert(surface);
        }
    }

    for candidate in candidates {
        if is_api_version(&candidate) || surface_from_category(&candidate).is_some() {
            continue;
        }
        if let Some(path) = candidate_to_doc_path(&candidate)
            .filter(|path| get_doc(conn, path).ok().flatten().is_some())
        {
            refs.insert(path.clone());
            doc_paths.insert(path.clone());
            collect_graph_neighbors(
                conn,
                &all_edges,
                "doc",
                &path,
                &mut refs,
                &mut doc_paths,
                &mut concept_ids,
                &mut surfaces,
            )?;
            continue;
        }
        let concept = match version_hint.as_deref() {
            Some(version) => find_concept_by_name(conn, &candidate, Some(version))?
                .or_else(|| find_concept_by_name(conn, &candidate, None).ok().flatten()),
            None => find_concept_by_name(conn, &candidate, None)?,
        };
        if let Some(concept) = concept {
            refs.insert(concept.name.clone());
            concept_ids.insert(concept.id.clone());
            if let Some(path) = &concept.defined_in_path {
                doc_paths.insert(path.clone());
            }
            surfaces.insert("admin_graphql".to_string());
            collect_graph_neighbors(
                conn,
                &all_edges,
                "concept",
                &concept.id,
                &mut refs,
                &mut doc_paths,
                &mut concept_ids,
                &mut surfaces,
            )?;
            continue;
        }
        if looks_like_reference_candidate(&candidate) {
            unresolved_refs.insert(candidate);
        }
    }

    Ok(ResolvedImpact {
        refs: refs.into_iter().collect(),
        doc_paths: doc_paths.into_iter().collect(),
        concept_ids: concept_ids.into_iter().collect(),
        surfaces: surfaces.into_iter().collect(),
        unresolved_refs: unresolved_refs.into_iter().collect(),
    })
}

fn collect_graph_neighbors(
    conn: &Connection,
    edges: &[GraphEdgeRecord],
    start_type: &str,
    start_id: &str,
    refs: &mut BTreeSet<String>,
    doc_paths: &mut BTreeSet<String>,
    concept_ids: &mut BTreeSet<String>,
    surfaces: &mut BTreeSet<String>,
) -> Result<()> {
    let mut queue = VecDeque::from([(start_type.to_string(), start_id.to_string(), 0usize)]);
    let mut seen = HashSet::new();
    while let Some((node_type, node_id, distance)) = queue.pop_front() {
        if !seen.insert(format!("{node_type}:{node_id}")) || distance > 2 {
            continue;
        }
        match node_type.as_str() {
            "doc" => {
                if let Some(doc) = get_doc(conn, &node_id)? {
                    refs.insert(doc.path.clone());
                    doc_paths.insert(doc.path.clone());
                    if let Some(surface) = doc.api_surface {
                        surfaces.insert(surface);
                    }
                }
            }
            "concept" => {
                if let Some(concept) = get_concept(conn, &node_id)? {
                    refs.insert(concept.name.clone());
                    concept_ids.insert(concept.id.clone());
                    if let Some(path) = concept.defined_in_path {
                        doc_paths.insert(path);
                    }
                    surfaces.insert("admin_graphql".to_string());
                }
            }
            _ => {}
        }
        if distance == 2 {
            continue;
        }
        for edge in edges {
            let neighbor = if edge.from_type == node_type && edge.from_id == node_id {
                Some((edge.to_type.clone(), edge.to_id.clone()))
            } else if edge.to_type == node_type && edge.to_id == node_id {
                Some((edge.from_type.clone(), edge.from_id.clone()))
            } else {
                None
            };
            if let Some(neighbor) = neighbor {
                queue.push_back((neighbor.0, neighbor.1, distance + 1));
            }
        }
    }
    Ok(())
}

fn insert_changelog_entry(
    conn: &Connection,
    entry: &ChangelogEntryInput,
    impact: &ResolvedImpact,
) -> Result<()> {
    let affected_types = impact_affected_types(impact);
    conn.execute(
        "
        INSERT INTO changelog_entries (
          id, title, url, posted_at, body, categories, affected_types,
          affected_surfaces, unresolved_affected_refs, processed_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ON CONFLICT(id) DO NOTHING
        ",
        params![
            entry.id,
            entry.title,
            entry.link,
            entry.posted_at,
            entry.body,
            serde_json::to_string(&entry.categories)?,
            serde_json::to_string(&affected_types)?,
            serde_json::to_string(&impact.surfaces)?,
            serde_json::to_string(&impact.unresolved_refs)?,
            now_iso(),
        ],
    )?;
    Ok(())
}
