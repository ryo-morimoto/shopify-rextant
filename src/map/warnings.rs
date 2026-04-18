use chrono::{DateTime, Utc};

use super::super::StatusResponse;

pub(crate) fn index_age_days(last_full_build: Option<&str>) -> i64 {
    last_full_build
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|dt| (Utc::now() - dt.with_timezone(&Utc)).num_days())
        .unwrap_or(0)
}

pub(crate) fn map_coverage_warning(status: &StatusResponse) -> Option<String> {
    if status.doc_count == 0 {
        Some("Index is empty; run shopify-rextant build.".to_string())
    } else if status.coverage.failed_count > 0 || status.coverage.skipped_count > 0 {
        Some(format!(
            "Coverage has {} skipped and {} failed URLs from the last build.",
            status.coverage.skipped_count, status.coverage.failed_count
        ))
    } else if index_age_days(status.last_full_build.as_deref()) > 14 {
        Some("Index is older than 14 days; run shopify-rextant refresh.".to_string())
    } else {
        None
    }
}
