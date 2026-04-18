use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DocRecord {
    pub(crate) path: String,
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) version: Option<String>,
    pub(crate) doc_type: String,
    pub(crate) api_surface: Option<String>,
    pub(crate) content_class: String,
    pub(crate) content_sha: String,
    pub(crate) last_verified: String,
    pub(crate) last_changed: String,
    pub(crate) freshness: String,
    pub(crate) references_deprecated: bool,
    pub(crate) deprecated_refs: Vec<String>,
    pub(crate) summary_raw: String,
    pub(crate) reading_time_min: Option<i64>,
    pub(crate) raw_path: String,
    pub(crate) source: String,
}
