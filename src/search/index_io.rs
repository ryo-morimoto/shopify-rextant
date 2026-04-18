use anyhow::Result;
use std::fs;
use tantivy::schema::{Schema, TantivyDocument};
use tantivy::{Index, Term, doc};

use super::super::DocRecord;
use super::super::Paths;
use super::super::db::docs::doc_from_row;
use super::super::db::schema::open_db;
use super::schema::{SearchFields, search_schema};
use super::tokenizer::register_japanese_tokenizer;

pub(crate) fn create_or_reset_index(paths: &Paths, schema: Schema, reset: bool) -> Result<Index> {
    if reset && paths.tantivy.exists() {
        fs::remove_dir_all(&paths.tantivy)?;
    }
    fs::create_dir_all(&paths.tantivy)?;
    if paths.tantivy.join("meta.json").exists() {
        let index = Index::open_in_dir(&paths.tantivy)?;
        if SearchFields::from_schema(&index.schema()).is_ok() {
            Ok(index)
        } else {
            fs::remove_dir_all(&paths.tantivy)?;
            fs::create_dir_all(&paths.tantivy)?;
            Index::create_in_dir(&paths.tantivy, schema).map_err(Into::into)
        }
    } else {
        Index::create_in_dir(&paths.tantivy, schema).map_err(Into::into)
    }
}

pub(crate) fn rebuild_tantivy_from_db(paths: &Paths) -> Result<()> {
    let schema = search_schema();
    let index = create_or_reset_index(paths, schema.clone(), true)?;
    register_japanese_tokenizer(&index)?;
    let fields = SearchFields::from_schema(&schema)?;
    let conn = open_db(paths)?;
    let mut stmt = conn.prepare(
        "SELECT path, title, url, version, doc_type, api_surface, content_class, content_sha,
                last_verified, last_changed, freshness, references_deprecated, deprecated_refs,
                summary_raw, reading_time_min, raw_path, source
         FROM docs",
    )?;
    let docs = stmt.query_map([], doc_from_row)?;
    let mut writer = index.writer(50_000_000)?;
    for doc in docs {
        let doc = doc?;
        let content = fs::read_to_string(paths.raw_file(&doc.raw_path)).unwrap_or_default();
        add_tantivy_doc(&mut writer, fields, &doc, &content)?;
    }
    writer.commit()?;
    Ok(())
}

pub(crate) fn upsert_tantivy_doc(paths: &Paths, record: &DocRecord, content: &str) -> Result<()> {
    let schema = search_schema();
    let index = create_or_reset_index(paths, schema.clone(), false)?;
    register_japanese_tokenizer(&index)?;
    let fields = match SearchFields::from_schema(&index.schema()) {
        Ok(fields) => fields,
        Err(_) => {
            rebuild_tantivy_from_db(paths)?;
            return Ok(());
        }
    };
    let mut writer = index.writer(50_000_000)?;
    writer.delete_term(Term::from_field_text(fields.path, &record.path));
    add_tantivy_doc(&mut writer, fields, record, content)?;
    writer.commit()?;
    Ok(())
}

pub(crate) fn add_tantivy_doc(
    writer: &mut tantivy::IndexWriter<TantivyDocument>,
    fields: SearchFields,
    record: &DocRecord,
    content: &str,
) -> Result<()> {
    writer.add_document(doc!(
        fields.path => record.path.clone(),
        fields.title => record.title.clone(),
        fields.url => record.url.clone(),
        fields.version => record.version.clone().unwrap_or_else(|| "evergreen".to_string()),
        fields.api_surface => record.api_surface.clone().unwrap_or_else(|| "unknown".to_string()),
        fields.doc_type => record.doc_type.clone(),
        fields.content_en => content.chars().take(4_000).collect::<String>(),
        fields.content_ja => content.chars().take(4_000).collect::<String>(),
    ))?;
    Ok(())
}
