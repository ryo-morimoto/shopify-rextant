use serde::Serialize;
use std::collections::HashSet;

use super::super::DocRecord;

#[derive(Debug, Serialize)]
pub(crate) struct QueryPlanStep {
    pub(crate) step: usize,
    pub(crate) action: String,
    pub(crate) path: Option<String>,
    pub(crate) reason: String,
}

pub(crate) fn is_doc_like_query(value: &str) -> bool {
    value.starts_with("/docs/") || value.starts_with("/changelog") || value == "/llms.txt"
}

pub(crate) fn graph_query_plan(paths: &[String], lens: &str, radius: usize) -> Vec<QueryPlanStep> {
    paths
        .iter()
        .take(3)
        .enumerate()
        .map(|(i, path)| QueryPlanStep {
            step: i + 1,
            action: "fetch".to_string(),
            path: Some(path.clone()),
            reason: if i == 0 {
                format!("Primary graph source for lens={lens}, radius={radius}; fetch raw source before answering.")
            } else {
                "Related graph source to compare against the primary source.".to_string()
            },
        })
        .collect()
}

pub(crate) fn node_kind_rank(kind: &str) -> usize {
    match kind {
        "doc" => 0,
        "concept" => 1,
        _ => 2,
    }
}

pub(crate) fn doc_type_rank(doc_type: &str) -> usize {
    match doc_type {
        "reference" => 0,
        "how-to" => 1,
        "tutorial" => 2,
        "explanation" => 3,
        "migration" => 4,
        _ => 9,
    }
}

pub(crate) fn dedupe_docs_by_path(docs: Vec<DocRecord>) -> Vec<DocRecord> {
    let mut seen = HashSet::new();
    docs.into_iter()
        .filter(|doc| seen.insert(doc.path.clone()))
        .collect()
}
