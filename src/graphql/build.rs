use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;

use anyhow::{Context, Result};
use serde_json::Value;

use super::super::domain::concepts::ConceptRecord;
use super::super::domain::graph::{GraphBuild, GraphEdgeRecord};
use super::super::domain::source::SourceDoc;
use super::super::markdown::parse_markdown_links;
use super::super::source::text_source::TextSource;
use super::super::url_policy::{
    canonical_doc_path, classify_api_surface, extract_version,
};
use super::super::Paths;
use super::resolve::{extract_named_type, markdown_mentions_type, resolve_concept_id};
use super::schema_urls::{
    admin_graphql_direct_proxy_url, concept_id, graphql_concept_kind, graphql_reference_path,
};

pub(crate) async fn build_admin_graphql_graph<S: TextSource>(
    paths: &Paths,
    docs: &[SourceDoc],
    source: &S,
) -> Result<GraphBuild> {
    let doc_paths = docs
        .iter()
        .filter_map(|doc| canonical_doc_path(&doc.url).ok())
        .collect::<HashSet<_>>();
    let doc_contents = docs
        .iter()
        .filter_map(|doc| {
            canonical_doc_path(&doc.url)
                .ok()
                .map(|path| (path, doc.content.as_str()))
        })
        .collect::<HashMap<_, _>>();
    let versions = admin_graphql_versions(docs);
    let mut graph = GraphBuild::default();
    let mut concept_ids = HashSet::new();
    let mut edge_keys = HashSet::new();

    for version in versions {
        let url = admin_graphql_direct_proxy_url(&version);
        let Ok(snapshot) = source.fetch_admin_graphql_introspection(&url).await else {
            continue;
        };
        persist_schema_snapshot(paths, &version, &snapshot)?;
        let schema_json: Value = serde_json::from_str(&snapshot)
            .with_context(|| format!("parse Admin GraphQL introspection for {version}"))?;
        ingest_introspection_schema(
            &schema_json,
            &version,
            &doc_paths,
            &mut graph,
            &mut concept_ids,
            &mut edge_keys,
        )?;
    }

    add_doc_graph_edges(
        &doc_paths,
        &doc_contents,
        &concept_ids,
        &mut graph,
        &mut edge_keys,
    )?;
    Ok(graph)
}

pub(crate) fn admin_graphql_versions(docs: &[SourceDoc]) -> Vec<String> {
    docs.iter()
        .filter_map(|doc| canonical_doc_path(&doc.url).ok())
        .filter(|path| classify_api_surface(path).as_deref() == Some("admin_graphql"))
        .filter_map(|path| extract_version(&path))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(crate) fn persist_schema_snapshot(paths: &Paths, version: &str, snapshot: &str) -> Result<()> {
    let schema_dir = paths.data.join("schemas/admin-graphql");
    fs::create_dir_all(&schema_dir)?;
    fs::write(
        schema_dir.join(format!("{version}.introspection.json")),
        snapshot,
    )?;
    Ok(())
}

pub(crate) fn persist_graph_snapshot(paths: &Paths, graph: &GraphBuild) -> Result<()> {
    fs::create_dir_all(&paths.data)?;
    fs::write(
        paths.data.join("graph.msgpack"),
        serde_json::to_vec(graph).context("serialize graph snapshot")?,
    )?;
    Ok(())
}

fn ingest_introspection_schema(
    schema_json: &Value,
    version: &str,
    doc_paths: &HashSet<String>,
    graph: &mut GraphBuild,
    concept_ids: &mut HashSet<String>,
    edge_keys: &mut HashSet<String>,
) -> Result<()> {
    let Some(types) = schema_json
        .pointer("/data/__schema/types")
        .and_then(Value::as_array)
    else {
        return Ok(());
    };

    for gql_type in types {
        let Some(name) = gql_type.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name.starts_with("__") {
            continue;
        }
        let kind = gql_type
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("OBJECT");
        let concept_kind = graphql_concept_kind(kind);
        let id = concept_id(version, name);
        let defined_in_path = graphql_reference_path(version, kind, name);
        let stored_defined_in_path = defined_in_path.clone();
        insert_unique_concept(
            graph,
            concept_ids,
            ConceptRecord {
                id: id.clone(),
                kind: concept_kind.to_string(),
                name: name.to_string(),
                version: Some(version.to_string()),
                defined_in_path: stored_defined_in_path,
                deprecated: false,
                deprecated_since: None,
                deprecation_reason: None,
                replaced_by: None,
                kind_metadata: serde_json::to_string(gql_type).ok(),
            },
        );
        if let Some(path) = defined_in_path.filter(|path| doc_paths.contains(path)) {
            insert_unique_edge(
                graph,
                edge_keys,
                GraphEdgeRecord {
                    from_type: "concept".to_string(),
                    from_id: id.clone(),
                    to_type: "doc".to_string(),
                    to_id: path.clone(),
                    kind: "defined_in".to_string(),
                    weight: 1.0,
                    source_path: Some(path),
                },
            );
        }
    }

    for gql_type in types {
        let Some(parent_name) = gql_type.get("name").and_then(Value::as_str) else {
            continue;
        };
        if parent_name.starts_with("__") {
            continue;
        }
        let parent_id = concept_id(version, parent_name);
        let source_path = graphql_reference_path(
            version,
            gql_type
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("OBJECT"),
            parent_name,
        );
        if let Some(fields) = gql_type.get("fields").and_then(Value::as_array) {
            for field in fields {
                ingest_graphql_field(
                    version,
                    &parent_id,
                    parent_name,
                    field,
                    "graphql_field",
                    "has_field",
                    "returns",
                    source_path.as_deref(),
                    graph,
                    concept_ids,
                    edge_keys,
                )?;
            }
        }
        if let Some(input_fields) = gql_type.get("inputFields").and_then(Value::as_array) {
            for field in input_fields {
                ingest_graphql_field(
                    version,
                    &parent_id,
                    parent_name,
                    field,
                    "graphql_input_field",
                    "accepts_input",
                    "references_type",
                    source_path.as_deref(),
                    graph,
                    concept_ids,
                    edge_keys,
                )?;
            }
        }
        if let Some(interfaces) = gql_type.get("interfaces").and_then(Value::as_array) {
            for interface in interfaces {
                if let Some(interface_name) = interface.get("name").and_then(Value::as_str) {
                    let interface_id = concept_id(version, interface_name);
                    if concept_ids.contains(&interface_id) {
                        insert_unique_edge(
                            graph,
                            edge_keys,
                            GraphEdgeRecord {
                                from_type: "concept".to_string(),
                                from_id: parent_id.clone(),
                                to_type: "concept".to_string(),
                                to_id: interface_id,
                                kind: "implements".to_string(),
                                weight: 1.0,
                                source_path: source_path.clone(),
                            },
                        );
                    }
                }
            }
        }
        if let Some(enum_values) = gql_type.get("enumValues").and_then(Value::as_array) {
            for enum_value in enum_values {
                if let Some(value_name) = enum_value.get("name").and_then(Value::as_str) {
                    let value_id = format!("{parent_id}.{value_name}");
                    insert_unique_concept(
                        graph,
                        concept_ids,
                        ConceptRecord {
                            id: value_id.clone(),
                            kind: "graphql_enum_value".to_string(),
                            name: format!("{parent_name}.{value_name}"),
                            version: Some(version.to_string()),
                            defined_in_path: source_path.clone(),
                            deprecated: enum_value
                                .get("isDeprecated")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            deprecated_since: None,
                            deprecation_reason: enum_value
                                .get("deprecationReason")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            replaced_by: None,
                            kind_metadata: serde_json::to_string(enum_value).ok(),
                        },
                    );
                    insert_unique_edge(
                        graph,
                        edge_keys,
                        GraphEdgeRecord {
                            from_type: "concept".to_string(),
                            from_id: parent_id.clone(),
                            to_type: "concept".to_string(),
                            to_id: value_id,
                            kind: "member_of".to_string(),
                            weight: 1.0,
                            source_path: source_path.clone(),
                        },
                    );
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn ingest_graphql_field(
    version: &str,
    parent_id: &str,
    parent_name: &str,
    field: &Value,
    concept_kind: &str,
    parent_edge_kind: &str,
    target_edge_kind: &str,
    source_path: Option<&str>,
    graph: &mut GraphBuild,
    concept_ids: &mut HashSet<String>,
    edge_keys: &mut HashSet<String>,
) -> Result<()> {
    let Some(field_name) = field.get("name").and_then(Value::as_str) else {
        return Ok(());
    };
    let field_id = format!("{parent_id}.{field_name}");
    insert_unique_concept(
        graph,
        concept_ids,
        ConceptRecord {
            id: field_id.clone(),
            kind: concept_kind.to_string(),
            name: format!("{parent_name}.{field_name}"),
            version: Some(version.to_string()),
            defined_in_path: source_path.map(ToOwned::to_owned),
            deprecated: field
                .get("isDeprecated")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            deprecated_since: None,
            deprecation_reason: field
                .get("deprecationReason")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            replaced_by: None,
            kind_metadata: serde_json::to_string(field).ok(),
        },
    );
    insert_unique_edge(
        graph,
        edge_keys,
        GraphEdgeRecord {
            from_type: "concept".to_string(),
            from_id: parent_id.to_string(),
            to_type: "concept".to_string(),
            to_id: field_id.clone(),
            kind: parent_edge_kind.to_string(),
            weight: 1.0,
            source_path: source_path.map(ToOwned::to_owned),
        },
    );
    if let Some(target_name) = field.get("type").and_then(extract_named_type) {
        if let Some(target_id) = resolve_concept_id(version, &target_name, concept_ids) {
            insert_unique_edge(
                graph,
                edge_keys,
                GraphEdgeRecord {
                    from_type: "concept".to_string(),
                    from_id: field_id,
                    to_type: "concept".to_string(),
                    to_id: target_id,
                    kind: target_edge_kind.to_string(),
                    weight: 1.0,
                    source_path: source_path.map(ToOwned::to_owned),
                },
            );
        }
    }
    Ok(())
}

fn add_doc_graph_edges(
    doc_paths: &HashSet<String>,
    doc_contents: &HashMap<String, &str>,
    concept_ids: &HashSet<String>,
    graph: &mut GraphBuild,
    edge_keys: &mut HashSet<String>,
) -> Result<()> {
    let concept_names = concept_ids
        .iter()
        .filter_map(|id| {
            let mut parts = id.split('.');
            let surface = parts.next()?;
            let version = parts.next()?;
            let name = parts.next()?;
            if surface == "admin_graphql" && !name.contains('.') {
                Some((version.to_string(), name.to_string(), id.clone()))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    for (path, content) in doc_contents {
        for (_, name, id) in &concept_names {
            if markdown_mentions_type(content, name) {
                insert_unique_edge(
                    graph,
                    edge_keys,
                    GraphEdgeRecord {
                        from_type: "doc".to_string(),
                        from_id: path.clone(),
                        to_type: "concept".to_string(),
                        to_id: id.clone(),
                        kind: "references_type".to_string(),
                        weight: 1.0,
                        source_path: Some(path.clone()),
                    },
                );
            }
        }
        for link in parse_markdown_links(content) {
            if let Ok(target_path) = canonical_doc_path(&link.url) {
                if doc_paths.contains(&target_path) {
                    insert_unique_edge(
                        graph,
                        edge_keys,
                        GraphEdgeRecord {
                            from_type: "doc".to_string(),
                            from_id: path.clone(),
                            to_type: "doc".to_string(),
                            to_id: target_path,
                            kind: "see_also".to_string(),
                            weight: 1.0,
                            source_path: Some(path.clone()),
                        },
                    );
                }
            }
        }
    }

    let mut sorted_docs = doc_paths.iter().cloned().collect::<Vec<_>>();
    sorted_docs.sort();
    for pair in sorted_docs.windows(2) {
        if let [prev, next] = pair {
            insert_unique_edge(
                graph,
                edge_keys,
                GraphEdgeRecord {
                    from_type: "doc".to_string(),
                    from_id: prev.clone(),
                    to_type: "doc".to_string(),
                    to_id: next.clone(),
                    kind: "next".to_string(),
                    weight: 1.0,
                    source_path: None,
                },
            );
            insert_unique_edge(
                graph,
                edge_keys,
                GraphEdgeRecord {
                    from_type: "doc".to_string(),
                    from_id: next.clone(),
                    to_type: "doc".to_string(),
                    to_id: prev.clone(),
                    kind: "prev".to_string(),
                    weight: 1.0,
                    source_path: None,
                },
            );
        }
    }
    Ok(())
}

fn insert_unique_concept(
    graph: &mut GraphBuild,
    concept_ids: &mut HashSet<String>,
    concept: ConceptRecord,
) {
    if concept_ids.insert(concept.id.clone()) {
        graph.concepts.push(concept);
    }
}

fn insert_unique_edge(
    graph: &mut GraphBuild,
    edge_keys: &mut HashSet<String>,
    edge: GraphEdgeRecord,
) {
    let key = format!(
        "{}:{}:{}:{}:{}",
        edge.from_type, edge.from_id, edge.kind, edge.to_type, edge.to_id
    );
    if edge_keys.insert(key) {
        graph.edges.push(edge);
    }
}
