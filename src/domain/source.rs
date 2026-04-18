#[derive(Debug)]
pub(crate) struct SourceDoc {
    pub(crate) url: String,
    pub(crate) title_hint: Option<String>,
    pub(crate) content: String,
    pub(crate) source: String,
}

#[derive(Debug)]
pub(crate) struct SourceFetchError {
    pub(crate) status: String,
    pub(crate) reason: String,
    pub(crate) http_status: Option<u16>,
}
