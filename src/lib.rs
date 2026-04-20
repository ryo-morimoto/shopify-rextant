mod build;
mod changelog;
pub mod cli;
mod config;
mod coverage;
mod db;
mod doc_freshness;
mod domain;
mod fetch;
mod graphql;
mod map;
mod map_runtime;
mod markdown;
mod mcp;
mod mcp_framing;
mod on_demand;
mod refresh;
mod search;
mod source;
mod source_sync;
pub(crate) mod status;
mod url_policy;
mod util;

pub use cli::run;

pub(crate) use build::build_index;
#[cfg(test)]
pub(crate) use build::build_index_from_sources;
pub(crate) use changelog::poll::check_new_versions_from_source;
#[cfg(test)]
use changelog::poll::poll_changelog_from_source;
#[cfg(test)]
use config::load_config;
pub(crate) use coverage::coverage_repair;
#[cfg(test)]
use coverage::coverage_repair_from_source;
pub(crate) use fetch::shopify_fetch;
#[cfg(test)]
use fetch::shopify_fetch_from_source;
#[allow(unused_imports)]
pub(crate) use map_runtime::{shopify_map, shopify_map_with_runtime};
pub(crate) use refresh::refresh;
#[cfg(test)]
use refresh::{refresh_stale_docs_from_source, refresh_url_from_source};
pub(crate) use search::runtime::{search_docs, search_docs_with_runtime};
#[cfg(test)]
use source_sync::store_source_doc;
pub(crate) use status::status;

#[cfg(test)]
use db::concepts::insert_concept;
#[cfg(test)]
use db::coverage::insert_coverage_event;
#[cfg(test)]
use db::docs::{count_docs, refresh_indexed_versions, upsert_doc};
#[cfg(test)]
use db::docs::{count_where, get_doc, parse_json_string_vec};
#[cfg(test)]
use db::graph::insert_edge;
#[cfg(test)]
use db::meta::get_meta;
#[cfg(test)]
use db::schema::{init_db, open_db};
#[cfg(test)]
use domain::concepts::ConceptRecord;
#[cfg(test)]
use domain::coverage::CoverageEvent;
#[cfg(test)]
use domain::graph::GraphEdgeRecord;
#[cfg(test)]
use domain::source::SourceDoc;
#[cfg(test)]
use status::coverage_status;

pub(crate) use domain::docs::DocRecord;
pub(crate) use domain::source::SourceFetchError;
pub(crate) use domain::status::StatusResponse;
#[cfg(test)]
use graphql::schema_urls::admin_graphql_direct_proxy_url;
#[cfg(test)]
use markdown::{
    extract_sections, parse_markdown_links, parse_sitemap_links, remove_fenced_code_blocks,
    section_content,
};
#[cfg(test)]
use url_policy::{
    canonical_doc_path, classify_api_surface, classify_content_class, classify_doc_type,
    raw_doc_candidates,
};
#[cfg(test)]
use util::json::to_json_value;

use anyhow::{Result, anyhow};
#[cfg(test)]
use chrono::Utc;
#[cfg(test)]
use mcp::daemon::{DaemonIdentity, DaemonPaths};
#[cfg(test)]
use mcp::protocol::handle_mcp_request;
use mcp::protocol::json_rpc_error;
#[cfg(test)]
use mcp_framing::{read_message as read_mcp_message, write_json as write_mcp_message};
use on_demand::FetchCandidate as OnDemandFetchCandidate;
#[cfg(test)]
use on_demand::FetchPolicy as OnDemandFetchPolicy;
#[cfg(test)]
use rusqlite::Connection;
#[cfg(test)]
use rusqlite::params;
#[cfg(test)]
use search::index_io::rebuild_tantivy_from_db;
#[cfg(test)]
use search::schema::SearchFields;
use serde::Deserialize;
use serde_json::{Value, json};
#[allow(unused_imports)]
pub(crate) use source::text_source::TextSource;
#[cfg(test)]
use std::fs;
use std::path::PathBuf;

const SHOPIFY_LLMS_URL: &str = "https://shopify.dev/llms.txt";
const SHOPIFY_SITEMAP_URL: &str = "https://shopify.dev/sitemap.xml";
const SHOPIFY_CHANGELOG_FEED_URL: &str = "https://shopify.dev/changelog/feed.xml";
const SHOPIFY_VERSIONING_URL: &str = "https://shopify.dev/docs/api/usage/versioning";
pub(crate) const USER_AGENT: &str = concat!("shopify-rextant/", env!("CARGO_PKG_VERSION"));
pub(crate) const SCHEMA_VERSION: &str = "3";
pub(crate) const ADMIN_GRAPHQL_INTROSPECTION_QUERY: &str = r#"
query ShopifyRextantIntrospection {
  __schema {
    types {
      kind
      name
      description
      fields(includeDeprecated: true) {
        name
        description
        args {
          name
          description
          type { kind name ofType { kind name ofType { kind name ofType { kind name } } } }
          defaultValue
        }
        type { kind name ofType { kind name ofType { kind name ofType { kind name } } } }
        isDeprecated
        deprecationReason
      }
      inputFields {
        name
        description
        type { kind name ofType { kind name ofType { kind name ofType { kind name } } } }
        defaultValue
        isDeprecated
        deprecationReason
      }
      interfaces { kind name }
      enumValues(includeDeprecated: true) {
        name
        description
        isDeprecated
        deprecationReason
      }
      possibleTypes { kind name }
    }
  }
}
"#;

#[derive(Debug, Clone)]
pub(crate) struct Paths {
    pub(crate) home: PathBuf,
    pub(crate) data: PathBuf,
    pub(crate) raw: PathBuf,
    pub(crate) tantivy: PathBuf,
    pub(crate) db: PathBuf,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MapArgs {
    pub(crate) from: String,
    pub(crate) radius: Option<u8>,
    pub(crate) lens: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) max_nodes: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FetchArgs {
    pub(crate) path: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) anchor: Option<String>,
    pub(crate) include_code_blocks: Option<bool>,
    pub(crate) max_chars: Option<usize>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ToolError {
    #[error("{message}")]
    Rpc {
        code: i64,
        message: String,
        data: Value,
    },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl ToolError {
    pub(crate) fn json_rpc_error(&self) -> Value {
        match self {
            Self::Rpc {
                code,
                message,
                data,
            } => json_rpc_error(*code, message, data.clone()),
            Self::Internal(error) => json_rpc_error(-32000, &error.to_string(), Value::Null),
        }
    }

    pub(crate) fn disabled(candidate: &OnDemandFetchCandidate) -> Self {
        Self::Rpc {
            code: -32007,
            message: "On-demand fetch is disabled".to_string(),
            data: json!({
                "candidate_url": candidate.source_url,
                "canonical_path": candidate.canonical_path,
                "enable_on_demand_fetch": false,
            }),
        }
    }

    pub(crate) fn outside_scope(input: &str) -> Self {
        Self::Rpc {
            code: -32008,
            message: "URL outside allowed Shopify docs scope".to_string(),
            data: json!({
                "input": input,
                "allowed_scope": [
                    "https://shopify.dev/docs/**",
                    "https://shopify.dev/changelog/**"
                ],
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct SearchArgs {
    pub(crate) query: String,
    pub(crate) version: Option<String>,
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct IndexSourceUrls {
    pub(crate) llms: String,
    pub(crate) sitemap: String,
    pub(crate) changelog: String,
    pub(crate) versioning: String,
}

impl Default for IndexSourceUrls {
    fn default() -> Self {
        Self {
            llms: SHOPIFY_LLMS_URL.to_string(),
            sitemap: SHOPIFY_SITEMAP_URL.to_string(),
            changelog: SHOPIFY_CHANGELOG_FEED_URL.to_string(),
            versioning: SHOPIFY_VERSIONING_URL.to_string(),
        }
    }
}

impl Paths {
    pub(crate) fn new(home: Option<PathBuf>) -> Result<Self> {
        let home = match home {
            Some(path) => path,
            None => dirs::home_dir()
                .ok_or_else(|| anyhow!("could not resolve home directory"))?
                .join(".shopify-rextant"),
        };
        let data = home.join("data");
        Ok(Self {
            home,
            raw: data.join("raw"),
            tantivy: data.join("tantivy"),
            db: data.join("index.db"),
            data,
        })
    }

    pub(crate) fn raw_file(&self, raw_path: &str) -> PathBuf {
        self.raw.join(raw_path)
    }

    pub(crate) fn config_file(&self) -> PathBuf {
        self.home.join("config.toml")
    }
}

#[cfg(test)]
mod tests;
