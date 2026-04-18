use serde::Serialize;

use super::concepts::ConceptRecord;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphEdgeRecord {
    pub(crate) from_type: String,
    pub(crate) from_id: String,
    pub(crate) to_type: String,
    pub(crate) to_id: String,
    pub(crate) kind: String,
    pub(crate) weight: f64,
    pub(crate) source_path: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct GraphBuild {
    pub(crate) concepts: Vec<ConceptRecord>,
    pub(crate) edges: Vec<GraphEdgeRecord>,
}

#[derive(Debug, Clone)]
pub(crate) struct GraphNodeKey {
    pub(crate) node_type: String,
    pub(crate) id: String,
}
