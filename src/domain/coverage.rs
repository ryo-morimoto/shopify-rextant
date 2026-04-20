use super::super::markdown::MarkdownLink;
use super::super::url_policy::canonical_doc_path;
use super::super::util::time::now_iso;
use super::source::SourceFetchError;

#[derive(Debug)]
pub(crate) struct CoverageEvent {
    pub(crate) source: String,
    pub(crate) canonical_path: Option<String>,
    pub(crate) source_url: String,
    pub(crate) status: String,
    pub(crate) reason: Option<String>,
    pub(crate) http_status: Option<u16>,
    pub(crate) checked_at: String,
}

impl CoverageEvent {
    pub(crate) fn indexed(link: &MarkdownLink) -> Self {
        Self {
            source: link.source.clone(),
            canonical_path: canonical_doc_path(&link.url).ok(),
            source_url: link.url.clone(),
            status: "indexed".to_string(),
            reason: None,
            http_status: None,
            checked_at: now_iso(),
        }
    }

    pub(crate) fn from_fetch_error(link: &MarkdownLink, error: SourceFetchError) -> Self {
        Self {
            source: link.source.clone(),
            canonical_path: canonical_doc_path(&link.url).ok(),
            source_url: link.url.clone(),
            status: error.status,
            reason: Some(error.reason),
            http_status: error.http_status,
            checked_at: now_iso(),
        }
    }
}
