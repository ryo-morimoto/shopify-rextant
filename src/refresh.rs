use anyhow::{Result, anyhow, bail};

use super::db::docs::{get_doc, stale_refresh_candidates, upsert_doc};
use super::db::meta::set_meta;
use super::db::schema::{init_db, open_db};
use super::domain::source::SourceDoc;
use super::fetch::on_demand_fetch_from_input;
use super::search::index_io::rebuild_tantivy_from_db;
use super::source::reqwest_source::ReqwestTextSource;
use super::source::text_source::TextSource;
use super::source_sync::{fetch_required_text, store_source_doc};
use super::status::update_doc_freshness_states;
use super::util::time::now_iso;
use super::{Paths, SCHEMA_VERSION};

pub(crate) async fn refresh(
    paths: &Paths,
    path: Option<String>,
    url: Option<String>,
) -> Result<()> {
    let source = ReqwestTextSource::new()?;
    match (path, url) {
        (Some(_), Some(_)) => bail!("refresh accepts either PATH or --url, not both"),
        (Some(path), None) => refresh_doc_from_source(paths, &path, &source).await,
        (None, Some(url)) => refresh_url_from_source(paths, &url, &source).await,
        (None, None) => refresh_stale_docs_from_source(paths, &source).await,
    }
}

pub(crate) async fn refresh_url_from_source<S: TextSource>(
    paths: &Paths,
    url: &str,
    source: &S,
) -> Result<()> {
    on_demand_fetch_from_input(paths, url, source)
        .await
        .map(|_| ())
        .map_err(|error| anyhow!(error.to_string()))
}

pub(crate) async fn refresh_doc_from_source<S: TextSource>(
    paths: &Paths,
    path: &str,
    source: &S,
) -> Result<()> {
    let conn = open_db(paths)?;
    init_db(&conn, SCHEMA_VERSION)?;
    let doc = get_doc(&conn, path)?.ok_or_else(|| anyhow!("path not found: {path}"))?;
    let content = fetch_required_text(source, &doc.url).await?;
    let source_doc = SourceDoc {
        url: doc.url,
        title_hint: Some(doc.title),
        content,
        source: doc.source,
    };
    let record = store_source_doc(paths, &source_doc)?;
    upsert_doc(&conn, &record)?;
    rebuild_tantivy_from_db(paths)?;
    Ok(())
}

pub(crate) async fn refresh_stale_docs_from_source<S: TextSource>(
    paths: &Paths,
    source: &S,
) -> Result<()> {
    let conn = open_db(paths)?;
    init_db(&conn, SCHEMA_VERSION)?;
    update_doc_freshness_states(&conn)?;
    let docs = stale_refresh_candidates(&conn, 100)?;
    let mut refreshed = 0usize;
    let mut warnings = Vec::new();
    for doc in docs {
        match source.fetch_text(&doc.url).await {
            Ok(content) => {
                let source_doc = SourceDoc {
                    url: doc.url,
                    title_hint: Some(doc.title),
                    content,
                    source: doc.source,
                };
                let record = store_source_doc(paths, &source_doc)?;
                upsert_doc(&conn, &record)?;
                refreshed += 1;
            }
            Err(error) => warnings.push(format!("{}: {}", doc.path, error.reason)),
        }
    }
    set_meta(&conn, "last_aging_sweep_at", &now_iso())?;
    if !warnings.is_empty() {
        set_meta(&conn, "last_aging_sweep_warning", &warnings.join("; "))?;
    }
    if refreshed > 0 {
        rebuild_tantivy_from_db(paths)?;
    }
    Ok(())
}
