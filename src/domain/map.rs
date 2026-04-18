use serde::Serialize;
use serde_json::Value;

use super::super::map::plan::QueryPlanStep;
use super::status::StatusResponse;

#[derive(Debug, Serialize)]
pub(crate) struct MapResponse {
    pub(crate) center: MapCenter,
    pub(crate) nodes: Vec<MapNode>,
    pub(crate) edges: Vec<Value>,
    pub(crate) suggested_reading_order: Vec<String>,
    pub(crate) query_plan: Vec<QueryPlanStep>,
    pub(crate) index_status: StatusResponse,
    pub(crate) meta: MapMeta,
}

#[derive(Debug, Serialize)]
pub(crate) struct MapMeta {
    pub(crate) generated_at: String,
    pub(crate) index_age_days: i64,
    pub(crate) versions_available: Vec<String>,
    pub(crate) version_used: String,
    pub(crate) coverage_warning: Option<String>,
    pub(crate) graph_available: bool,
    pub(crate) index_status: MapIndexStatus,
    pub(crate) on_demand_candidate: Option<OnDemandCandidate>,
    pub(crate) query_interpretation: QueryInterpretation,
}

#[derive(Debug, Serialize)]
pub(crate) struct MapIndexStatus {
    pub(crate) doc_count: i64,
    pub(crate) skipped_count: i64,
    pub(crate) failed_count: i64,
}

#[derive(Debug, Serialize)]
pub(crate) struct OnDemandCandidate {
    pub(crate) url: String,
    pub(crate) enabled: bool,
    pub(crate) reason: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct QueryInterpretation {
    pub(crate) resolved_as: String,
    pub(crate) entry_points: Vec<String>,
    pub(crate) confidence: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct MapCenter {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) path: Option<String>,
    pub(crate) title: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct MapNode {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) subkind: String,
    pub(crate) path: String,
    pub(crate) title: String,
    pub(crate) summary_from_source: String,
    pub(crate) version: Option<String>,
    pub(crate) api_surface: Option<String>,
    pub(crate) doc_type: String,
    pub(crate) reading_time_min: Option<i64>,
    pub(crate) staleness: Staleness,
    pub(crate) distance_from_center: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct Staleness {
    pub(crate) age_days: i64,
    pub(crate) freshness: String,
    pub(crate) content_verified_at: String,
    pub(crate) schema_version: Option<String>,
    pub(crate) references_deprecated: bool,
    pub(crate) deprecated_refs: Vec<String>,
    pub(crate) upcoming_changes: Vec<Value>,
}

#[derive(Debug)]
pub(crate) struct GraphExpansion {
    pub(crate) nodes: Vec<MapNode>,
    pub(crate) edges: Vec<Value>,
    pub(crate) suggested_reading_order: Vec<String>,
}
