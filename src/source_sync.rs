use std::fs;

use anyhow::{Result, anyhow};

use super::domain::docs::DocRecord;
use super::domain::source::{SourceDoc, SourceFetchError};
use super::markdown::{MarkdownLink, title_from_markdown};
use super::source::text_source::TextSource;
use super::url_policy::{
    canonical_doc_path, classify_api_surface, classify_content_class, classify_doc_type,
    extract_version, raw_doc_candidates, raw_path_for, reading_time_min,
};
use super::util::hash::hex_sha256;
use super::util::time::now_iso;
use super::Paths;

pub(crate) async fn fetch_source_doc<S: TextSource>(
    source: &S,
    link: &MarkdownLink,
) -> std::result::Result<SourceDoc, SourceFetchError> {
    let candidates = raw_doc_candidates(&link.url).map_err(|e| SourceFetchError {
        status: "failed".to_string(),
        reason: format!("invalid_url: {e}"),
        http_status: None,
    })?;
    let mut last_status = None;
    for url in candidates {
        match source.fetch_text(&url).await {
            Ok(content) => {
                return std::result::Result::Ok(SourceDoc {
                    url,
                    title_hint: Some(link.title.clone()),
                    content,
                    source: link.source.clone(),
                });
            }
            Err(error) if error.status == "skipped" => {
                last_status = error.http_status;
            }
            Err(error) => return Err(error),
        }
    }
    Err(SourceFetchError {
        status: "skipped".to_string(),
        reason: "markdown_not_found".to_string(),
        http_status: last_status,
    })
}

pub(crate) async fn fetch_required_text<S: TextSource>(source: &S, url: &str) -> Result<String> {
    source.fetch_text(url).await.map_err(|error| {
        anyhow!(
            "GET {url} failed: {} (status={}, http_status={:?})",
            error.reason,
            error.status,
            error.http_status
        )
    })
}

pub(crate) fn store_source_doc(paths: &Paths, source: &SourceDoc) -> Result<DocRecord> {
    let path = canonical_doc_path(&source.url)?;
    let title = title_from_markdown(&source.content)
        .or_else(|| source.title_hint.clone())
        .unwrap_or_else(|| path.clone());
    let sha = hex_sha256(&source.content);
    let raw_path = raw_path_for(&path);
    let raw_file = paths.raw_file(&raw_path);
    if let Some(parent) = raw_file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&raw_file, &source.content)?;
    let now = now_iso();
    Ok(DocRecord {
        path: path.clone(),
        title,
        url: source.url.clone(),
        version: extract_version(&path),
        doc_type: classify_doc_type(&path),
        api_surface: classify_api_surface(&path),
        content_class: classify_content_class(&path),
        content_sha: sha,
        last_verified: now.clone(),
        last_changed: now,
        freshness: "fresh".to_string(),
        references_deprecated: false,
        deprecated_refs: Vec::new(),
        summary_raw: source.content.chars().take(400).collect(),
        reading_time_min: Some(reading_time_min(&source.content)),
        raw_path,
        source: source.source.clone(),
    })
}
