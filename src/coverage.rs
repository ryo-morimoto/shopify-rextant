use anyhow::Result;
use serde::Serialize;

use super::config::{load_config, policy_from_config};
use super::db::coverage::{failed_coverage_rows, update_coverage_failed, update_coverage_repaired};
use super::db::schema::{init_db, open_db};
use super::fetch::on_demand_fetch_candidate;
use super::on_demand::FetchPolicy as OnDemandFetchPolicy;
use super::source::reqwest_source::ReqwestTextSource;
use super::source::text_source::TextSource;
use super::{Paths, SCHEMA_VERSION};

#[derive(Debug, Serialize)]
pub(crate) struct CoverageRepairSummary {
    pub(crate) attempted: usize,
    pub(crate) repaired: usize,
    pub(crate) still_failed: usize,
    pub(crate) skipped_policy: usize,
    pub(crate) skipped_disabled: usize,
}

pub(crate) async fn coverage_repair(paths: &Paths) -> Result<CoverageRepairSummary> {
    let source = ReqwestTextSource::new()?;
    coverage_repair_from_source(paths, &source).await
}

pub(crate) async fn coverage_repair_from_source<S: TextSource>(
    paths: &Paths,
    source: &S,
) -> Result<CoverageRepairSummary> {
    let conn = open_db(paths)?;
    init_db(&conn, SCHEMA_VERSION)?;
    let rows = failed_coverage_rows(&conn)?;
    let config = load_config(paths)?;
    let policy = policy_from_config(&config);
    let mut summary = CoverageRepairSummary {
        attempted: 0,
        repaired: 0,
        still_failed: 0,
        skipped_policy: 0,
        skipped_disabled: 0,
    };
    for row in rows {
        let candidate = match OnDemandFetchPolicy::candidate_from_input(&row.source_url) {
            Ok(candidate) => candidate,
            Err(_) => {
                summary.skipped_policy += 1;
                continue;
            }
        };
        if !policy.is_enabled() {
            summary.skipped_disabled += 1;
            continue;
        }
        summary.attempted += 1;
        match on_demand_fetch_candidate(paths, candidate.clone(), source, false).await {
            Ok(_) => {
                update_coverage_repaired(&conn, row.id, &candidate)?;
                summary.repaired += 1;
            }
            Err(error) => {
                update_coverage_failed(&conn, row.id, &error.to_string())?;
                summary.still_failed += 1;
            }
        }
    }
    Ok(summary)
}
