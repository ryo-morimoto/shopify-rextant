use anyhow::{Result, bail};
use lindera::dictionary::load_dictionary;
use lindera::mode::Mode;
use lindera::segmenter::Segmenter;
use lindera_tantivy::tokenizer::LinderaTokenizer;
use std::sync::OnceLock;
use tantivy::Index;

pub(crate) fn register_japanese_tokenizer(index: &Index) -> Result<()> {
    index.tokenizers().register(
        "lindera_ipadic",
        LinderaTokenizer::from_segmenter(japanese_segmenter()?),
    );
    Ok(())
}

pub(crate) fn query_needs_japanese_tokenizer(query: &str) -> bool {
    query.chars().any(|ch| !ch.is_ascii())
}

pub(crate) fn japanese_segmenter() -> Result<Segmenter> {
    static JAPANESE_SEGMENTER: OnceLock<std::result::Result<Segmenter, String>> = OnceLock::new();
    match JAPANESE_SEGMENTER.get_or_init(|| {
        load_dictionary("embedded://ipadic")
            .map_err(|error| error.to_string())
            .map(|dictionary| Segmenter::new(Mode::Normal, dictionary, None))
    }) {
        Ok(segmenter) => Ok(segmenter.clone()),
        Err(error) => {
            bail!("load Japanese tokenizer dictionary: {error}")
        }
    }
}
