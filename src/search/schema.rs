use anyhow::Result;
use tantivy::schema::{
    Field, IndexRecordOption, STORED, STRING, Schema, TEXT, TextFieldIndexing, TextOptions,
};

#[derive(Clone, Copy)]
pub(crate) struct SearchFields {
    pub(crate) path: Field,
    pub(crate) title: Field,
    pub(crate) url: Field,
    pub(crate) version: Field,
    pub(crate) api_surface: Field,
    pub(crate) doc_type: Field,
    pub(crate) content_en: Field,
    pub(crate) content_ja: Field,
}

impl SearchFields {
    pub(crate) fn from_schema(schema: &Schema) -> Result<Self> {
        Ok(Self {
            path: schema.get_field("path")?,
            title: schema.get_field("title")?,
            url: schema.get_field("url")?,
            version: schema.get_field("version")?,
            api_surface: schema.get_field("api_surface")?,
            doc_type: schema.get_field("doc_type")?,
            content_en: schema.get_field("content_en")?,
            content_ja: schema.get_field("content_ja")?,
        })
    }

    pub(crate) fn content_fields(&self) -> [Field; 2] {
        [self.content_en, self.content_ja]
    }
}

pub(crate) fn search_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("path", STRING | STORED);
    builder.add_text_field("title", TEXT | STORED);
    builder.add_text_field("url", STRING | STORED);
    builder.add_text_field("version", STRING | STORED);
    builder.add_text_field("api_surface", STRING | STORED);
    builder.add_text_field("doc_type", STRING | STORED);
    builder.add_text_field("content_en", TEXT);
    let japanese_text = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("lindera_ipadic")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    builder.add_text_field("content_ja", japanese_text);
    builder.build()
}
