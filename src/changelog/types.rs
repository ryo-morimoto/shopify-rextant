#[derive(Debug, Clone)]
pub(crate) struct ChangelogEntryInput {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) link: String,
    pub(crate) body: String,
    pub(crate) posted_at: String,
    pub(crate) categories: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedImpact {
    pub(crate) refs: Vec<String>,
    pub(crate) doc_paths: Vec<String>,
    pub(crate) concept_ids: Vec<String>,
    pub(crate) surfaces: Vec<String>,
    pub(crate) unresolved_refs: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ScheduledChangeRecord {
    pub(crate) id: String,
    pub(crate) type_name: String,
    pub(crate) change: String,
    pub(crate) effective_date: Option<String>,
    pub(crate) migration_hint: Option<String>,
    pub(crate) source_changelog_id: String,
}
