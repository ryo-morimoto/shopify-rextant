use serde::Serialize;

use super::map::Staleness;
use super::super::markdown::SectionInfo;

#[derive(Debug, Serialize)]
pub(crate) struct FetchResponse {
    pub(crate) path: String,
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) source_url: String,
    pub(crate) content: String,
    pub(crate) sections: Vec<SectionInfo>,
    pub(crate) truncated: bool,
    pub(crate) staleness: Staleness,
}
