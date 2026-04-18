use serde::Serialize;

#[derive(Debug, Serialize)]
pub(crate) struct StatusResponse {
    pub(crate) schema_version: String,
    pub(crate) data_dir: String,
    pub(crate) index_built: bool,
    pub(crate) doc_count: i64,
    pub(crate) last_full_build: Option<String>,
    pub(crate) index: GraphIndexStatus,
    pub(crate) coverage: CoverageStatus,
    pub(crate) freshness: FreshnessStatus,
    pub(crate) workers: WorkerStatus,
    pub(crate) changelog: ChangelogStatus,
    pub(crate) warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphIndexStatus {
    pub(crate) concept_count: i64,
    pub(crate) edge_count: i64,
    pub(crate) graph_snapshot: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CoverageStatus {
    pub(crate) last_sitemap_at: Option<String>,
    pub(crate) discovered_count: i64,
    pub(crate) indexed_count: i64,
    pub(crate) skipped_count: i64,
    pub(crate) failed_count: i64,
    pub(crate) classified_unknown_count: i64,
    pub(crate) sources: CoverageSources,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CoverageSources {
    pub(crate) llms: i64,
    pub(crate) sitemap: i64,
    pub(crate) on_demand: i64,
    pub(crate) manual: i64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct FreshnessStatus {
    pub(crate) fresh_count: i64,
    pub(crate) aging_count: i64,
    pub(crate) stale_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkerStatus {
    pub(crate) last_changelog_at: Option<String>,
    pub(crate) last_aging_sweep_at: Option<String>,
    pub(crate) last_version_check_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ChangelogStatus {
    pub(crate) entry_count: i64,
    pub(crate) scheduled_change_count: i64,
    pub(crate) unresolved_ref_count: i64,
    pub(crate) last_warning: Option<String>,
}

impl CoverageStatus {
    pub(crate) fn empty() -> Self {
        Self {
            last_sitemap_at: None,
            discovered_count: 0,
            indexed_count: 0,
            skipped_count: 0,
            failed_count: 0,
            classified_unknown_count: 0,
            sources: CoverageSources {
                llms: 0,
                sitemap: 0,
                on_demand: 0,
                manual: 0,
            },
        }
    }
}

impl FreshnessStatus {
    pub(crate) fn empty() -> Self {
        Self {
            fresh_count: 0,
            aging_count: 0,
            stale_count: 0,
        }
    }
}

impl WorkerStatus {
    pub(crate) fn empty() -> Self {
        Self {
            last_changelog_at: None,
            last_aging_sweep_at: None,
            last_version_check_at: None,
        }
    }
}

impl ChangelogStatus {
    pub(crate) fn empty() -> Self {
        Self {
            entry_count: 0,
            scheduled_change_count: 0,
            unresolved_ref_count: 0,
            last_warning: None,
        }
    }
}
