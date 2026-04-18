use anyhow::Result;
use rusqlite::{Connection, params};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::TantivyDocument;
use tantivy::{Document, Index};

use super::super::DocRecord;
use super::super::Paths;
use super::super::db::docs::{doc_from_row, get_doc};
use super::super::db::schema::open_db;
use super::super::util::json::{doc_json_field, escape_query};
use super::schema::SearchFields;
use super::tokenizer::{query_needs_japanese_tokenizer, register_japanese_tokenizer};

pub(crate) struct SearchRuntime {
    index: Index,
    fields: SearchFields,
    reader: tantivy::IndexReader,
}

impl SearchRuntime {
    pub(crate) fn open(paths: &Paths) -> Result<Option<Self>> {
        if !paths.tantivy.join("meta.json").exists() {
            return Ok(None);
        }
        let index = Index::open_in_dir(&paths.tantivy)?;
        let fields = match SearchFields::from_schema(&index.schema()) {
            Ok(fields) => fields,
            Err(_) => return Ok(None),
        };
        let reader = index.reader()?;
        Ok(Some(Self {
            index,
            fields,
            reader,
        }))
    }

    pub(crate) fn search(
        &self,
        conn: &Connection,
        query: &str,
        version: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DocRecord>> {
        let searcher = self.reader.searcher();
        let mut query_fields = vec![self.fields.title, self.fields.path];
        if query_needs_japanese_tokenizer(query) {
            register_japanese_tokenizer(&self.index)?;
            query_fields.extend(self.fields.content_fields());
        } else {
            query_fields.push(self.fields.content_en);
        }
        let parser = QueryParser::for_index(&self.index, query_fields);
        let parsed = parser
            .parse_query(query)
            .or_else(|_| parser.parse_query(&escape_query(query)))?;
        let top_docs = searcher.search(&parsed, &TopDocs::with_limit(limit))?;
        let schema = self.index.schema();
        let mut records = Vec::new();
        for (_score, address) in top_docs {
            let retrieved = searcher.doc::<TantivyDocument>(address)?;
            let Some(path) = doc_json_field(&retrieved.to_json(&schema), "path") else {
                continue;
            };
            if let Some(record) = get_doc(conn, &path)? {
                if version.is_none_or(|v| {
                    record.version.is_none() || record.version.as_deref() == Some(v)
                }) {
                    records.push(record);
                }
            }
        }
        Ok(records)
    }
}

#[allow(dead_code)]
pub(crate) fn search_docs(
    paths: &Paths,
    query: &str,
    version: Option<&str>,
    limit: usize,
) -> Result<Vec<DocRecord>> {
    search_docs_with_runtime(paths, None, query, version, limit)
}

pub(crate) fn search_docs_with_runtime(
    paths: &Paths,
    runtime: Option<&SearchRuntime>,
    query: &str,
    version: Option<&str>,
    limit: usize,
) -> Result<Vec<DocRecord>> {
    let conn = open_db(paths)?;
    if let Some(runtime) = runtime {
        return runtime.search(&conn, query, version, limit);
    }
    if !paths.tantivy.join("meta.json").exists() {
        return sqlite_like_search(&conn, query, version, limit);
    }
    let Some(runtime) = SearchRuntime::open(paths)? else {
        return sqlite_like_search(&conn, query, version, limit);
    };
    runtime.search(&conn, query, version, limit)
}

pub(crate) fn sqlite_like_search(
    conn: &Connection,
    query: &str,
    version: Option<&str>,
    limit: usize,
) -> Result<Vec<DocRecord>> {
    let like = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
    let mut stmt = conn.prepare(
        "SELECT path, title, url, version, doc_type, api_surface, content_class, content_sha,
                last_verified, last_changed, freshness, references_deprecated, deprecated_refs,
                summary_raw, reading_time_min, raw_path, source
         FROM docs
         WHERE (title LIKE ?1 ESCAPE '\\' OR path LIKE ?1 ESCAPE '\\' OR summary_raw LIKE ?1 ESCAPE '\\')
           AND (?2 IS NULL OR version IS NULL OR version = ?2)
         ORDER BY hit_count DESC, title
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![like, version, limit as i64], doc_from_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}
