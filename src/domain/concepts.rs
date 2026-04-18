use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ConceptRecord {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) name: String,
    pub(crate) version: Option<String>,
    pub(crate) defined_in_path: Option<String>,
    pub(crate) deprecated: bool,
    pub(crate) deprecated_since: Option<String>,
    pub(crate) deprecation_reason: Option<String>,
    pub(crate) replaced_by: Option<String>,
    pub(crate) kind_metadata: Option<String>,
}
