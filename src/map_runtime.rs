use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Value, json};

use super::config::load_config;
use super::db::concepts::{find_concept_by_name, get_concept};
use super::db::docs::get_doc;
use super::db::graph::load_edges;
use super::db::schema::{init_db, open_db};
use super::doc_freshness::{scheduled_changes_for_concept, staleness, staleness_for_doc};
use super::domain::concepts::ConceptRecord;
use super::domain::docs::DocRecord;
use super::domain::graph::GraphNodeKey;
use super::domain::map::{
    GraphExpansion, MapCenter, MapIndexStatus, MapMeta, MapNode, MapResponse, OnDemandCandidate,
    QueryInterpretation, Staleness,
};
use super::map::plan::{
    QueryPlanStep, dedupe_docs_by_path, doc_type_rank, graph_query_plan, is_doc_like_query,
    node_kind_rank,
};
use super::map::warnings::{index_age_days, map_coverage_warning};
use super::on_demand::FetchPolicy as OnDemandFetchPolicy;
use super::search::runtime::SearchRuntime;
use super::status::{status, versions_available};
use super::util::json::merge_json_arrays;
use super::util::time::now_iso;
use super::{MapArgs, Paths, SCHEMA_VERSION, search_docs_with_runtime};

#[allow(dead_code)]
pub(crate) fn shopify_map(paths: &Paths, args: &MapArgs) -> Result<MapResponse> {
    shopify_map_with_runtime(paths, args, None)
}

pub(crate) fn shopify_map_with_runtime(
    paths: &Paths,
    args: &MapArgs,
    search_runtime: Option<&SearchRuntime>,
) -> Result<MapResponse> {
    let limit = args.max_nodes.unwrap_or(30).clamp(1, 100);
    let radius = args.radius.unwrap_or(2).clamp(1, 3);
    let lens = args.lens.as_deref().unwrap_or("auto");
    let conn = open_db(paths)?;
    init_db(&conn, SCHEMA_VERSION)?;
    let index_status = status(paths)?;
    let versions_available = versions_available(paths).unwrap_or_default();
    let version_used = args
        .version
        .clone()
        .or_else(|| versions_available.first().cloned())
        .unwrap_or_else(|| "evergreen".to_string());
    let mut docs = Vec::new();
    let mut start_nodes = Vec::new();
    let mut resolved_as = "free_text";
    let confidence;
    let graph_has_edges = index_status.index.concept_count > 0 && index_status.index.edge_count > 0;

    if is_doc_like_query(&args.from) {
        docs = get_doc(&conn, &args.from)?.into_iter().collect();
        if !docs.is_empty() {
            start_nodes.push(GraphNodeKey {
                node_type: "doc".to_string(),
                id: args.from.clone(),
            });
        }
        resolved_as = "doc_path";
        confidence = if docs.is_empty() { "low" } else { "exact" };
    } else if graph_has_edges {
        if let Some(concept) = find_concept_by_name(&conn, &args.from, args.version.as_deref())? {
            start_nodes.push(GraphNodeKey {
                node_type: "concept".to_string(),
                id: concept.id,
            });
            resolved_as = "concept_name";
            confidence = "exact";
        } else {
            docs = search_docs_with_runtime(
                paths,
                search_runtime,
                &args.from,
                args.version.as_deref(),
                limit,
            )?;
            docs = dedupe_docs_by_path(docs);
            docs.truncate(limit);
            start_nodes.extend(docs.iter().map(|doc| GraphNodeKey {
                node_type: "doc".to_string(),
                id: doc.path.clone(),
            }));
            confidence = if docs.is_empty() { "low" } else { "medium" };
        }
    } else {
        docs = search_docs_with_runtime(
            paths,
            search_runtime,
            &args.from,
            args.version.as_deref(),
            limit,
        )?;
        docs = dedupe_docs_by_path(docs);
        docs.truncate(limit);
        confidence = if docs.is_empty() { "low" } else { "medium" };
    }

    let graph_expansion = if graph_has_edges && !start_nodes.is_empty() {
        Some(expand_graph(
            &conn,
            &start_nodes,
            radius as usize,
            limit,
            &docs,
        )?)
    } else {
        None
    };
    let graph_available = graph_expansion
        .as_ref()
        .is_some_and(|expansion| !expansion.edges.is_empty());
    let entry_points = start_nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    let mut coverage_warning = map_coverage_warning(&index_status);
    if !graph_available {
        coverage_warning.get_or_insert_with(|| {
            "Graph coverage is unavailable for this query; using v0.1 FTS fallback.".to_string()
        });
    }
    let on_demand_candidate = if docs.is_empty() {
        OnDemandFetchPolicy::candidate_from_input(&args.from)
            .ok()
            .map(|candidate| OnDemandCandidate {
                url: candidate.source_url,
                enabled: load_config(paths)
                    .map(|config| config.index.enable_on_demand_fetch)
                    .unwrap_or(false),
                reason: "No local docs matched. shopify_fetch can recover this official Shopify docs URL only when local on-demand fetch is enabled.".to_string(),
            })
    } else {
        None
    };
    let meta = MapMeta {
        generated_at: now_iso(),
        index_age_days: index_age_days(index_status.last_full_build.as_deref()),
        versions_available,
        version_used,
        coverage_warning: coverage_warning.clone(),
        graph_available,
        index_status: MapIndexStatus {
            doc_count: index_status.doc_count,
            skipped_count: index_status.coverage.skipped_count,
            failed_count: index_status.coverage.failed_count,
        },
        on_demand_candidate,
        query_interpretation: QueryInterpretation {
            resolved_as: resolved_as.to_string(),
            entry_points,
            confidence: confidence.to_string(),
        },
    };

    if graph_available {
        let expansion = graph_expansion.expect("checked graph expansion");
        let center = center_for_key(&conn, start_nodes.first(), &args.from)?;
        let query_plan =
            graph_query_plan(&expansion.suggested_reading_order, lens, radius as usize);
        return Ok(MapResponse {
            center,
            nodes: expansion.nodes,
            edges: expansion.edges,
            suggested_reading_order: expansion.suggested_reading_order,
            query_plan,
            index_status,
            meta,
        });
    }

    let Some(center_doc) = docs.first() else {
        return Ok(MapResponse {
            center: MapCenter {
                id: args.from.clone(),
                kind: "doc".to_string(),
                path: None,
                title: args.from.clone(),
            },
            nodes: Vec::new(),
            edges: Vec::new(),
            suggested_reading_order: Vec::new(),
            query_plan: vec![
                QueryPlanStep {
                    step: 1,
                    action: "inspect_status".to_string(),
                    path: None,
                    reason: "No local docs matched; inspect index and coverage before using web fallback."
                        .to_string(),
                },
                QueryPlanStep {
                    step: 2,
                    action: "refresh".to_string(),
                    path: None,
                    reason: "Rebuild or refresh if the index is empty, stale, or has coverage failures."
                        .to_string(),
                },
            ],
            index_status,
            meta,
        });
    };
    let center = MapCenter {
        id: center_doc.path.clone(),
        kind: "doc".to_string(),
        path: Some(center_doc.path.clone()),
        title: center_doc.title.clone(),
    };
    let nodes = docs
        .iter()
        .enumerate()
        .map(|(i, doc)| MapNode {
            id: doc.path.clone(),
            kind: "doc".to_string(),
            subkind: doc.content_class.clone(),
            path: doc.path.clone(),
            title: doc.title.clone(),
            summary_from_source: doc.summary_raw.clone(),
            version: doc.version.clone(),
            api_surface: doc.api_surface.clone(),
            doc_type: doc.doc_type.clone(),
            reading_time_min: doc.reading_time_min,
            staleness: staleness_for_doc(&conn, doc).unwrap_or_else(|_| staleness(doc)),
            distance_from_center: usize::from(i > 0),
        })
        .collect::<Vec<_>>();
    let suggested_reading_order = nodes.iter().map(|node| node.path.clone()).collect();
    let mut query_plan = Vec::new();
    if !graph_available {
        query_plan.push(QueryPlanStep {
            step: 1,
            action: "inspect_status".to_string(),
            path: None,
            reason: "Graph data is unavailable or empty; inspect status before trusting coverage."
                .to_string(),
        });
    }
    let step_offset = query_plan.len();
    query_plan.extend(nodes
        .iter()
        .take(3)
        .enumerate()
        .map(|(i, node)| QueryPlanStep {
            step: step_offset + i + 1,
            action: "fetch".to_string(),
            path: Some(node.path.clone()),
            reason: if i == 0 {
                format!(
                    "Highest-ranked local FTS candidate for lens={lens}, radius={radius}; fetch raw source before answering."
                )
            } else {
                "Secondary local FTS candidate to compare against the primary source.".to_string()
            },
        }));
    Ok(MapResponse {
        center,
        nodes,
        edges: Vec::new(),
        suggested_reading_order,
        query_plan,
        index_status,
        meta,
    })
}

fn expand_graph(
    conn: &Connection,
    start_nodes: &[GraphNodeKey],
    radius: usize,
    limit: usize,
    extra_docs: &[DocRecord],
) -> Result<GraphExpansion> {
    let all_edges = load_edges(conn)?;
    let mut distances = HashMap::<String, usize>::new();
    let mut node_types = HashMap::<String, String>::new();
    let mut queue = VecDeque::new();

    for start in start_nodes {
        distances.insert(start.id.clone(), 0);
        node_types.insert(start.id.clone(), start.node_type.clone());
        queue.push_back(start.id.clone());
    }

    while let Some(current) = queue.pop_front() {
        let distance = *distances.get(&current).unwrap_or(&0);
        if distance >= radius || distances.len() >= limit {
            continue;
        }
        for edge in &all_edges {
            let neighbor = if edge.from_id == current {
                Some((edge.to_id.as_str(), edge.to_type.as_str()))
            } else if edge.to_id == current {
                Some((edge.from_id.as_str(), edge.from_type.as_str()))
            } else {
                None
            };
            if let Some((neighbor_id, neighbor_type)) = neighbor {
                if !distances.contains_key(neighbor_id) && distances.len() < limit {
                    distances.insert(neighbor_id.to_string(), distance + 1);
                    node_types.insert(neighbor_id.to_string(), neighbor_type.to_string());
                    queue.push_back(neighbor_id.to_string());
                }
            }
        }
    }

    for doc in extra_docs {
        if distances.len() >= limit {
            break;
        }
        distances.entry(doc.path.clone()).or_insert(usize::from(
            !start_nodes.iter().any(|node| node.id == doc.path),
        ));
        node_types
            .entry(doc.path.clone())
            .or_insert_with(|| "doc".to_string());
    }

    let mut nodes = distances
        .iter()
        .filter_map(|(id, distance)| {
            let node_type = node_types.get(id)?;
            graph_map_node(conn, node_type, id, *distance).transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    nodes.sort_by(|a, b| {
        a.distance_from_center
            .cmp(&b.distance_from_center)
            .then_with(|| node_kind_rank(&a.kind).cmp(&node_kind_rank(&b.kind)))
            .then_with(|| a.id.cmp(&b.id))
    });

    let returned_ids = nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<HashSet<_>>();
    let mut edges = all_edges
        .iter()
        .filter(|edge| returned_ids.contains(&edge.from_id) && returned_ids.contains(&edge.to_id))
        .map(|edge| {
            json!({
                "from": edge.from_id,
                "to": edge.to_id,
                "kind": edge.kind,
                "weight": edge.weight,
                "source_path": edge.source_path,
            })
        })
        .collect::<Vec<_>>();
    edges.sort_by_key(|edge| {
        format!(
            "{}:{}:{}",
            edge.get("from").and_then(Value::as_str).unwrap_or_default(),
            edge.get("kind").and_then(Value::as_str).unwrap_or_default(),
            edge.get("to").and_then(Value::as_str).unwrap_or_default()
        )
    });

    let mut suggested_reading_order = nodes
        .iter()
        .filter(|node| {
            node.kind == "doc"
                && (node.path.starts_with("/docs/") || node.path.starts_with("/changelog/"))
        })
        .map(|node| node.path.clone())
        .collect::<Vec<_>>();
    suggested_reading_order.sort_by(|a, b| {
        let a_rank = get_doc(conn, a)
            .ok()
            .flatten()
            .map(|doc| doc_type_rank(&doc.doc_type))
            .unwrap_or(99);
        let b_rank = get_doc(conn, b)
            .ok()
            .flatten()
            .map(|doc| doc_type_rank(&doc.doc_type))
            .unwrap_or(99);
        a_rank.cmp(&b_rank).then_with(|| a.cmp(b))
    });
    suggested_reading_order.dedup();

    Ok(GraphExpansion {
        nodes,
        edges,
        suggested_reading_order,
    })
}

fn graph_map_node(
    conn: &Connection,
    node_type: &str,
    id: &str,
    distance: usize,
) -> Result<Option<MapNode>> {
    match node_type {
        "doc" => Ok(get_doc(conn, id)?.map(|doc| doc_map_node(conn, &doc, distance))),
        "concept" => Ok(get_concept(conn, id)?.map(|concept| {
            let backing_doc = concept
                .defined_in_path
                .as_deref()
                .and_then(|path| get_doc(conn, path).ok().flatten());
            concept_map_node(conn, &concept, backing_doc.as_ref(), distance)
        })),
        _ => Ok(None),
    }
}

fn doc_map_node(conn: &Connection, doc: &DocRecord, distance: usize) -> MapNode {
    MapNode {
        id: doc.path.clone(),
        kind: "doc".to_string(),
        subkind: doc.content_class.clone(),
        path: doc.path.clone(),
        title: doc.title.clone(),
        summary_from_source: doc.summary_raw.clone(),
        version: doc.version.clone(),
        api_surface: doc.api_surface.clone(),
        doc_type: doc.doc_type.clone(),
        reading_time_min: doc.reading_time_min,
        staleness: staleness_for_doc(conn, doc).unwrap_or_else(|_| staleness(doc)),
        distance_from_center: distance,
    }
}

fn concept_map_node(
    conn: &Connection,
    concept: &ConceptRecord,
    backing_doc: Option<&DocRecord>,
    distance: usize,
) -> MapNode {
    let fallback_verified_at = now_iso();
    let fallback_staleness = || Staleness {
        age_days: 0,
        freshness: "fresh".to_string(),
        content_verified_at: fallback_verified_at.clone(),
        schema_version: concept.version.clone(),
        references_deprecated: concept.deprecated,
        deprecated_refs: Vec::new(),
        upcoming_changes: scheduled_changes_for_concept(conn, concept).unwrap_or_default(),
    };
    MapNode {
        id: concept.id.clone(),
        kind: "concept".to_string(),
        subkind: concept.kind.clone(),
        path: concept
            .defined_in_path
            .clone()
            .unwrap_or_else(|| concept.id.clone()),
        title: concept.name.clone(),
        summary_from_source: backing_doc
            .map(|doc| doc.summary_raw.clone())
            .or_else(|| concept.kind_metadata.clone())
            .unwrap_or_default()
            .chars()
            .take(400)
            .collect(),
        version: concept.version.clone(),
        api_surface: Some("admin_graphql".to_string()),
        doc_type: "concept".to_string(),
        reading_time_min: backing_doc.and_then(|doc| doc.reading_time_min),
        staleness: backing_doc
            .map(|doc| {
                let mut staleness = staleness_for_doc(conn, doc).unwrap_or_else(|_| staleness(doc));
                let concept_changes =
                    scheduled_changes_for_concept(conn, concept).unwrap_or_default();
                if !concept_changes.is_empty() {
                    staleness.upcoming_changes =
                        merge_json_arrays(staleness.upcoming_changes, concept_changes);
                    staleness.references_deprecated = true;
                }
                staleness
            })
            .unwrap_or_else(fallback_staleness),
        distance_from_center: distance,
    }
}

fn center_for_key(
    conn: &Connection,
    key: Option<&GraphNodeKey>,
    fallback: &str,
) -> Result<MapCenter> {
    let Some(key) = key else {
        return Ok(MapCenter {
            id: fallback.to_string(),
            kind: "doc".to_string(),
            path: None,
            title: fallback.to_string(),
        });
    };
    if key.node_type == "concept" {
        if let Some(concept) = get_concept(conn, &key.id)? {
            return Ok(MapCenter {
                id: concept.id,
                kind: "concept".to_string(),
                path: concept.defined_in_path,
                title: concept.name,
            });
        }
    }
    if let Some(doc) = get_doc(conn, &key.id)? {
        return Ok(MapCenter {
            id: doc.path.clone(),
            kind: "doc".to_string(),
            path: Some(doc.path),
            title: doc.title,
        });
    }
    Ok(MapCenter {
        id: fallback.to_string(),
        kind: key.node_type.clone(),
        path: None,
        title: fallback.to_string(),
    })
}
