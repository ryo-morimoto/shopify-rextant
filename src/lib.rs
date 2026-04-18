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
pub(crate) use changelog::poll::check_new_versions_from_source;
pub(crate) use coverage::coverage_repair;
pub(crate) use fetch::shopify_fetch;
#[allow(unused_imports)]
pub(crate) use map_runtime::{shopify_map, shopify_map_with_runtime};
pub(crate) use refresh::refresh;
pub(crate) use status::status;
#[cfg(test)]
use config::load_config;
#[cfg(test)]
pub(crate) use build::build_index_from_sources;
#[cfg(test)]
use changelog::poll::poll_changelog_from_source;
#[cfg(test)]
use coverage::coverage_repair_from_source;
#[cfg(test)]
use fetch::shopify_fetch_from_source;
#[cfg(test)]
use refresh::{refresh_stale_docs_from_source, refresh_url_from_source};
#[cfg(test)]
use source_sync::store_source_doc;

#[cfg(test)]
use status::coverage_status;
#[cfg(test)]
use db::coverage::insert_coverage_event;
#[cfg(test)]
use db::concepts::insert_concept;
#[cfg(test)]
use db::docs::{count_docs, refresh_indexed_versions, upsert_doc};
#[cfg(test)]
use db::docs::{count_where, get_doc, parse_json_string_vec};
#[cfg(test)]
use db::graph::insert_edge;
#[cfg(test)]
use db::meta::get_meta;
use db::schema::open_db;
#[cfg(test)]
use db::schema::init_db;
#[cfg(test)]
use domain::concepts::ConceptRecord;
#[cfg(test)]
use domain::coverage::CoverageEvent;
#[cfg(test)]
use domain::graph::GraphEdgeRecord;
#[cfg(test)]
use domain::source::SourceDoc;

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
use mcp_framing::{read_message as read_mcp_message, write_json as write_mcp_message};
use on_demand::FetchCandidate as OnDemandFetchCandidate;
#[cfg(test)]
use on_demand::FetchPolicy as OnDemandFetchPolicy;
#[cfg(test)]
use mcp::daemon::{DaemonIdentity, DaemonPaths};
use mcp::protocol::json_rpc_error;
#[cfg(test)]
use mcp::protocol::handle_mcp_request;
#[cfg(test)]
use search::index_io::rebuild_tantivy_from_db;
#[cfg(test)]
use rusqlite::Connection;
use search::runtime::{SearchRuntime, sqlite_like_search};
#[cfg(test)]
use search::schema::SearchFields;
#[allow(unused_imports)]
pub(crate) use source::text_source::TextSource;
#[cfg(test)]
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};
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


#[cfg(test)]
mod tests {
    use super::*;
    use super::changelog::feed::parse_changelog_feed;
    #[allow(unused_imports)]
    use tantivy::Index;
    #[allow(unused_imports)]
    use tantivy::schema::{STORED, STRING, Schema, TEXT, TantivyDocument};

    struct MockTextSource {
        texts: std::collections::HashMap<String, String>,
    }

    impl MockTextSource {
        fn new(entries: &[(&str, &str)]) -> Self {
            Self {
                texts: entries
                    .iter()
                    .map(|(url, body)| ((*url).to_string(), (*body).to_string()))
                    .collect(),
            }
        }
    }

    impl TextSource for MockTextSource {
        async fn fetch_text(&self, url: &str) -> std::result::Result<String, SourceFetchError> {
            self.texts
                .get(url)
                .cloned()
                .ok_or_else(|| SourceFetchError {
                    status: "skipped".to_string(),
                    reason: "mock_not_found".to_string(),
                    http_status: Some(404),
                })
        }
    }

    fn fixture_sources() -> MockTextSource {
        MockTextSource::new(&[
            (
                SHOPIFY_LLMS_URL,
                "[Admin GraphQL](/docs/api/admin-graphql)\n",
            ),
            (
                SHOPIFY_SITEMAP_URL,
                r#"
                <urlset>
                  <url><loc>https://shopify.dev/docs/api/admin-graphql</loc></url>
                  <url><loc>https://shopify.dev/docs/apps/build/access-scopes</loc></url>
                </urlset>
                "#,
            ),
            (
                "https://shopify.dev/docs/api/admin-graphql.md",
                "# Admin GraphQL API\nReference docs for the Admin GraphQL API.\n",
            ),
            (
                "https://shopify.dev/docs/apps/build/access-scopes.md",
                "# Access scopes\nOptional access scopes and managed access scopes let apps request protected permissions.\n",
            ),
        ])
    }

    #[test]
    fn daemon_identity_uses_canonical_home() {
        let dir = tempfile::tempdir().unwrap();
        let dotted = Paths::new(Some(dir.path().join("."))).unwrap();
        let canonical = Paths::new(Some(fs::canonicalize(dir.path()).unwrap())).unwrap();

        let dotted_identity = DaemonIdentity::for_paths(&dotted).unwrap();
        let canonical_identity = DaemonIdentity::for_paths(&canonical).unwrap();

        assert_eq!(dotted_identity.hash(), canonical_identity.hash());
    }

    #[test]
    fn daemon_identity_separates_different_homes_and_config() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let paths_a = Paths::new(Some(dir_a.path().to_path_buf())).unwrap();
        let paths_b = Paths::new(Some(dir_b.path().to_path_buf())).unwrap();

        let hash_a = DaemonIdentity::for_paths(&paths_a).unwrap().hash();
        let hash_b = DaemonIdentity::for_paths(&paths_b).unwrap().hash();
        assert_ne!(hash_a, hash_b);

        fs::write(
            paths_a.config_file(),
            "[index]\nenable_on_demand_fetch = true\n",
        )
        .unwrap();
        let hash_a_with_config = DaemonIdentity::for_paths(&paths_a).unwrap().hash();
        assert_ne!(hash_a, hash_a_with_config);
    }

    #[test]
    fn daemon_socket_path_uses_bounded_hashed_filename() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let daemon_paths = DaemonPaths::for_paths(&paths).unwrap();
        let filename = daemon_paths
            .socket
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap();

        assert!(filename.ends_with(".sock"));
        assert!(filename.len() <= 69);
        assert!(daemon_paths.socket.as_os_str().len() < 100);
    }

    const ADMIN_GRAPHQL_DIRECT_PROXY_2026_04: &str =
        "https://shopify.dev/admin-graphql-direct-proxy/2026-04";

    fn admin_graphql_graph_sources() -> MockTextSource {
        MockTextSource::new(&[
            (
                SHOPIFY_LLMS_URL,
                "[Product](/docs/api/admin-graphql/2026-04/objects/Product)\n\
                 [Products guide](/docs/apps/build/products)\n\
                 [Cart level discount functions](/docs/apps/build/discounts/cart-level)\n\
                 [Discount overview](/docs/apps/build/discounts/overview)\n",
            ),
            (
                SHOPIFY_SITEMAP_URL,
                r#"
                <urlset>
                  <url><loc>https://shopify.dev/docs/api/admin-graphql/2026-04/objects/Product</loc></url>
                  <url><loc>https://shopify.dev/docs/apps/build/products</loc></url>
                  <url><loc>https://shopify.dev/docs/apps/build/discounts/cart-level</loc></url>
                  <url><loc>https://shopify.dev/docs/apps/build/discounts/overview</loc></url>
                </urlset>
                "#,
            ),
            (
                "https://shopify.dev/docs/api/admin-graphql/2026-04/objects/Product.md",
                "# Product\nThe `Product` object represents goods that a merchant can sell.\n",
            ),
            (
                "https://shopify.dev/docs/apps/build/products.md",
                "# Products guide\nUse Product when building product workflows.\n\n```graphql\nquery ProductGuide {\n  product(id: \"gid://shopify/Product/1\") {\n    id\n    title\n    variants(first: 10) { nodes { id } }\n  }\n}\n```\n",
            ),
            (
                "https://shopify.dev/docs/apps/build/discounts/cart-level.md",
                "# Cart level discount functions\nBuild a discount function cart level workflow that reads Product data before applying discounts. 割引クーポンの組み合わせを確認する。\n\n```graphql\nquery DiscountProducts {\n  products(first: 5) { nodes { id title } }\n}\n```\n",
            ),
            (
                "https://shopify.dev/docs/apps/build/discounts/overview.md",
                "# Discount overview\nUse this unpromoted discount overview when a cart level discount function does not mention schema types directly.\n",
            ),
            (
                ADMIN_GRAPHQL_DIRECT_PROXY_2026_04,
                r#"
                {
                  "data": {
                    "__schema": {
                      "types": [
                        {
                          "kind": "OBJECT",
                          "name": "Product",
                          "description": "A product that a merchant can sell.",
                          "fields": [
                            {
                              "name": "id",
                              "description": "The product ID.",
                              "args": [],
                              "type": {
                                "kind": "NON_NULL",
                                "name": null,
                                "ofType": { "kind": "SCALAR", "name": "ID", "ofType": null }
                              },
                              "isDeprecated": false,
                              "deprecationReason": null
                            },
                            {
                              "name": "variants",
                              "description": "The product variants.",
                              "args": [],
                              "type": {
                                "kind": "OBJECT",
                                "name": "ProductVariantConnection",
                                "ofType": null
                              },
                              "isDeprecated": false,
                              "deprecationReason": null
                            }
                          ],
                          "inputFields": null,
                          "interfaces": [],
                          "enumValues": null,
                          "possibleTypes": null
                        },
                        {
                          "kind": "OBJECT",
                          "name": "ProductVariant",
                          "description": "A product variant.",
                          "fields": [
                            {
                              "name": "id",
                              "description": "The variant ID.",
                              "args": [],
                              "type": {
                                "kind": "NON_NULL",
                                "name": null,
                                "ofType": { "kind": "SCALAR", "name": "ID", "ofType": null }
                              },
                              "isDeprecated": false,
                              "deprecationReason": null
                            }
                          ],
                          "inputFields": null,
                          "interfaces": [],
                          "enumValues": null,
                          "possibleTypes": null
                        },
                        {
                          "kind": "INPUT_OBJECT",
                          "name": "ProductInput",
                          "description": "Input fields for a product.",
                          "fields": null,
                          "inputFields": [
                            {
                              "name": "title",
                              "description": "The product title.",
                              "type": { "kind": "SCALAR", "name": "String", "ofType": null },
                              "defaultValue": null,
                              "isDeprecated": false,
                              "deprecationReason": null
                            }
                          ],
                          "interfaces": null,
                          "enumValues": null,
                          "possibleTypes": null
                        }
                      ]
                    }
                  }
                }
                "#,
            ),
        ])
    }

    fn seed_draft_order_graph(paths: &Paths) -> Connection {
        fs::create_dir_all(&paths.raw).unwrap();
        let conn = open_db(paths).unwrap();
        init_db(&conn, SCHEMA_VERSION).unwrap();
        let source = SourceDoc {
            url: "https://shopify.dev/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem.md"
                .to_string(),
            title_hint: Some("DraftOrderLineItem".to_string()),
            content:
                "# DraftOrderLineItem\nThe DraftOrderLineItem object includes the grams field.\n"
                    .to_string(),
            source: "sitemap".to_string(),
        };
        let record = store_source_doc(paths, &source).unwrap();
        upsert_doc(&conn, &record).unwrap();
        insert_concept(
            &conn,
            &ConceptRecord {
                id: "admin_graphql.2026-04.DraftOrderLineItem".to_string(),
                kind: "graphql_type".to_string(),
                name: "DraftOrderLineItem".to_string(),
                version: Some("2026-04".to_string()),
                defined_in_path: Some(
                    "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem".to_string(),
                ),
                deprecated: false,
                deprecated_since: None,
                deprecation_reason: None,
                replaced_by: None,
                kind_metadata: None,
            },
        )
        .unwrap();
        insert_concept(
            &conn,
            &ConceptRecord {
                id: "admin_graphql.2026-04.DraftOrderLineItem.grams".to_string(),
                kind: "graphql_field".to_string(),
                name: "DraftOrderLineItem.grams".to_string(),
                version: Some("2026-04".to_string()),
                defined_in_path: Some(
                    "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem".to_string(),
                ),
                deprecated: false,
                deprecated_since: None,
                deprecation_reason: None,
                replaced_by: None,
                kind_metadata: None,
            },
        )
        .unwrap();
        insert_edge(
            &conn,
            &GraphEdgeRecord {
                from_type: "concept".to_string(),
                from_id: "admin_graphql.2026-04.DraftOrderLineItem.grams".to_string(),
                to_type: "doc".to_string(),
                to_id: "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem".to_string(),
                kind: "defined_in".to_string(),
                weight: 1.0,
                source_path: Some(
                    "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem".to_string(),
                ),
            },
        )
        .unwrap();
        insert_edge(
            &conn,
            &GraphEdgeRecord {
                from_type: "concept".to_string(),
                from_id: "admin_graphql.2026-04.DraftOrderLineItem".to_string(),
                to_type: "concept".to_string(),
                to_id: "admin_graphql.2026-04.DraftOrderLineItem.grams".to_string(),
                kind: "has_field".to_string(),
                weight: 1.0,
                source_path: Some(
                    "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem".to_string(),
                ),
            },
        )
        .unwrap();
        rebuild_tantivy_from_db(paths).unwrap();
        conn
    }

    fn changelog_feed(title: &str, body: &str, link: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <rss version="2.0">
              <channel>
                <title>Shopify Developer changelog</title>
                <item>
                  <guid>{link}</guid>
                  <title>{title}</title>
                  <link>{link}</link>
                  <description><![CDATA[{body}]]></description>
                  <pubDate>Fri, 17 Apr 2026 00:00:00 GMT</pubDate>
                  <category>Admin GraphQL API</category>
                </item>
              </channel>
            </rss>"#
        )
    }

    fn minimal_introspection() -> &'static str {
        r#"{"data":{"__schema":{"types":[{"kind":"OBJECT","name":"Product"}]}}}"#
    }

    #[test]
    fn parses_shopify_markdown_links() {
        let links =
            parse_markdown_links("[Product](/docs/api/admin-graphql/latest/objects/Product)");
        assert_eq!(links.len(), 1);
        assert_eq!(
            links[0].url,
            "https://shopify.dev/docs/api/admin-graphql/latest/objects/Product"
        );
    }

    #[test]
    fn init_db_creates_v030_changelog_tables() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn, SCHEMA_VERSION).unwrap();

        let changelog_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM changelog_entries", [], |row| {
                row.get(0)
            })
            .unwrap();
        let scheduled_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM scheduled_changes", [], |row| {
                row.get(0)
            })
            .unwrap();

        assert_eq!(changelog_count, 0);
        assert_eq!(scheduled_count, 0);
        assert_eq!(count_where(&conn, "indexed_versions", "1=1").unwrap(), 0);
        assert_eq!(
            count_where(&conn, "version_rebuild_queue", "1=1").unwrap(),
            0
        );
    }

    #[test]
    fn parses_changelog_rss_entry() {
        let feed = changelog_feed(
            "DraftOrderLineItem.grams field removed in 2026-07",
            "Migrate away from grams.",
            "https://shopify.dev/changelog/draft-order-line-item-grams",
        );

        let entries = parse_changelog_feed(&feed).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].title,
            "DraftOrderLineItem.grams field removed in 2026-07"
        );
        assert_eq!(
            entries[0].link,
            "https://shopify.dev/changelog/draft-order-line-item-grams"
        );
        assert!(
            entries[0]
                .categories
                .contains(&"Admin GraphQL API".to_string())
        );
    }

    #[test]
    fn builds_raw_candidates() {
        let candidates =
            raw_doc_candidates("https://shopify.dev/docs/api/admin-graphql/latest").unwrap();
        assert_eq!(
            candidates[0],
            "https://shopify.dev/docs/api/admin-graphql/latest.md"
        );
        assert_eq!(
            candidates[1],
            "https://shopify.dev/docs/api/admin-graphql/latest.txt"
        );
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn classifies_admin_graphql_surface() {
        assert_eq!(
            classify_api_surface("/docs/api/admin-graphql/latest/objects/Product").as_deref(),
            Some("admin_graphql")
        );
        assert_eq!(
            classify_content_class("/docs/api/admin-graphql/latest/objects/Product"),
            "schema_ref"
        );
    }

    #[test]
    fn classifies_root_api_pages() {
        assert_eq!(
            classify_api_surface("/docs/api/admin-graphql").as_deref(),
            Some("admin_graphql")
        );
        assert_eq!(
            classify_api_surface("/docs/api/storefront").as_deref(),
            Some("storefront")
        );
        assert_eq!(classify_content_class("/docs/api/admin-graphql"), "api_ref");
        assert_eq!(classify_doc_type("/docs/api/storefront"), "reference");
    }

    #[test]
    fn canonical_path_strips_raw_suffix() {
        let path = canonical_doc_path(
            "https://shopify.dev/docs/api/admin-graphql/latest/queries/product.txt",
        )
        .unwrap();
        assert_eq!(path, "/docs/api/admin-graphql/latest/queries/product");
    }

    #[test]
    fn canonical_path_keeps_llms_txt() {
        let path = canonical_doc_path("https://shopify.dev/llms.txt").unwrap();
        assert_eq!(path, "/llms.txt");
    }

    fn write_on_demand_config(paths: &Paths, enabled: bool) {
        fs::create_dir_all(paths.config_file().parent().unwrap()).unwrap();
        fs::write(
            paths.config_file(),
            format!("[index]\nenable_on_demand_fetch = {enabled}\n"),
        )
        .unwrap();
    }

    #[test]
    fn on_demand_config_defaults_to_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();

        let config = load_config(&paths).unwrap();

        assert!(!config.index.enable_on_demand_fetch);
    }

    #[test]
    fn on_demand_policy_normalizes_allowed_urls_and_paths() {
        let from_url = OnDemandFetchPolicy::candidate_from_input(
            "https://shopify.dev/docs/apps/build/access-scopes/?utm=1#managed-access-scopes",
        )
        .unwrap();
        assert_eq!(from_url.canonical_path, "/docs/apps/build/access-scopes");
        assert_eq!(
            from_url.source_url,
            "https://shopify.dev/docs/apps/build/access-scopes"
        );

        let from_path =
            OnDemandFetchPolicy::candidate_from_input("/changelog/managed-access-scopes/").unwrap();
        assert_eq!(from_path.canonical_path, "/changelog/managed-access-scopes");
        assert_eq!(
            from_path.source_url,
            "https://shopify.dev/changelog/managed-access-scopes"
        );
    }

    #[test]
    fn on_demand_policy_rejects_outside_scope_without_network_candidate() {
        assert!(
            OnDemandFetchPolicy::candidate_from_input(
                "http://shopify.dev/docs/apps/build/app-home"
            )
            .is_err()
        );
        assert!(
            OnDemandFetchPolicy::candidate_from_input(
                "https://example.com/docs/apps/build/app-home"
            )
            .is_err()
        );
        assert!(OnDemandFetchPolicy::candidate_from_input("https://shopify.dev/partners").is_err());
    }

    #[test]
    fn parses_sitemap_links_for_docs_and_changelog() {
        let links = parse_sitemap_links(
            r#"
            <urlset>
              <url><loc>https://shopify.dev/docs/apps/build/access-scopes</loc></url>
              <url><loc>https://shopify.dev/changelog/managed-access-scopes</loc></url>
              <url><loc>https://shopify.dev/partners</loc></url>
            </urlset>
            "#,
        );
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].source, "sitemap");
        assert_eq!(
            links[0].url,
            "https://shopify.dev/docs/apps/build/access-scopes"
        );
        assert_eq!(
            canonical_doc_path(&links[1].url).unwrap(),
            "/changelog/managed-access-scopes"
        );
    }

    #[test]
    fn extracts_anchor_section_and_removes_code_blocks() {
        let markdown = "# Intro\nKeep\n\n## Managed access scopes\nText\n```graphql\nquery { shop { name } }\n```\nMore\n\n## Next\nDone\n";
        let sections = extract_sections(markdown);
        let scoped = section_content(markdown, &sections, "managed-access-scopes").unwrap();
        assert!(scoped.contains("## Managed access scopes"));
        assert!(!scoped.contains("## Next"));

        let without_code = remove_fenced_code_blocks(&scoped);
        assert!(without_code.contains("Text"));
        assert!(!without_code.contains("query { shop"));
        assert!(without_code.contains("More"));
    }

    #[tokio::test]
    async fn mcp_fetch_url_disabled_returns_v05_error_contract_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let response = handle_mcp_request(
            &paths,
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":1,
                "method":"tools/call",
                "params":{
                    "name":"shopify_fetch",
                    "arguments":{
                        "url":"https://shopify.dev/docs/apps/build/access-scopes"
                    }
                }
            }),
        )
        .await;

        assert_eq!(response["error"]["code"], -32007);
        assert_eq!(
            response["error"]["data"]["candidate_url"],
            "https://shopify.dev/docs/apps/build/access-scopes"
        );
        assert_eq!(
            response["error"]["data"]["canonical_path"],
            "/docs/apps/build/access-scopes"
        );
        assert_eq!(response["error"]["data"]["enable_on_demand_fetch"], false);
    }

    #[tokio::test]
    async fn mcp_fetch_url_outside_scope_returns_policy_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        write_on_demand_config(&paths, true);

        let response = handle_mcp_request(
            &paths,
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":1,
                "method":"tools/call",
                "params":{
                    "name":"shopify_fetch",
                    "arguments":{
                        "url":"https://shopify.dev/partners"
                    }
                }
            }),
        )
        .await;

        assert_eq!(response["error"]["code"], -32008);
        assert!(
            response["error"]["data"]["allowed_scope"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        );
    }

    #[tokio::test]
    async fn on_demand_fetch_url_stores_raw_doc_upserts_and_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        write_on_demand_config(&paths, true);
        let source = MockTextSource::new(&[(
            "https://shopify.dev/docs/apps/build/on-demand.md",
            "# On demand\nRecovered optional access scopes content.\n",
        )]);

        let response = shopify_fetch_from_source(
            &paths,
            &FetchArgs {
                path: None,
                url: Some(
                    "https://shopify.dev/docs/apps/build/on-demand?from=test#top".to_string(),
                ),
                anchor: None,
                include_code_blocks: None,
                max_chars: None,
            },
            &source,
        )
        .await
        .unwrap();

        assert_eq!(response.path, "/docs/apps/build/on-demand");
        assert!(
            response
                .content
                .contains("Recovered optional access scopes")
        );
        let conn = open_db(&paths).unwrap();
        let stored = get_doc(&conn, "/docs/apps/build/on-demand")
            .unwrap()
            .unwrap();
        assert_eq!(stored.source, "on_demand");
        assert!(paths.raw_file(&stored.raw_path).exists());
        let results = search_docs(&paths, "Recovered optional scopes", None, 5).unwrap();
        assert_eq!(results[0].path, "/docs/apps/build/on-demand");

        let fetched_by_path = shopify_fetch_from_source(
            &paths,
            &FetchArgs {
                path: Some("/docs/apps/build/on-demand".to_string()),
                url: None,
                anchor: None,
                include_code_blocks: None,
                max_chars: None,
            },
            &source,
        )
        .await
        .unwrap();
        assert_eq!(fetched_by_path.path, "/docs/apps/build/on-demand");
    }

    #[tokio::test]
    async fn on_demand_fetch_unindexed_path_derives_official_url() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        write_on_demand_config(&paths, true);
        let source = MockTextSource::new(&[(
            "https://shopify.dev/changelog/managed-access-scopes.md",
            "# Managed access scopes\nChangelog text.\n",
        )]);

        let response = shopify_fetch_from_source(
            &paths,
            &FetchArgs {
                path: Some("/changelog/managed-access-scopes".to_string()),
                url: None,
                anchor: None,
                include_code_blocks: None,
                max_chars: None,
            },
            &source,
        )
        .await
        .unwrap();

        assert_eq!(
            response.url,
            "https://shopify.dev/changelog/managed-access-scopes.md"
        );
        assert_eq!(response.path, "/changelog/managed-access-scopes");
    }

    #[tokio::test]
    async fn on_demand_refresh_preserves_existing_higher_precedence_source() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        write_on_demand_config(&paths, true);
        fs::create_dir_all(&paths.raw).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn, SCHEMA_VERSION).unwrap();
        let source_doc = SourceDoc {
            url: "https://shopify.dev/docs/apps/build/access-scopes.md".to_string(),
            title_hint: Some("Access scopes".to_string()),
            content: "# Access scopes\nIndexed by sitemap.\n".to_string(),
            source: "sitemap".to_string(),
        };
        let record = store_source_doc(&paths, &source_doc).unwrap();
        upsert_doc(&conn, &record).unwrap();
        rebuild_tantivy_from_db(&paths).unwrap();
        let source = MockTextSource::new(&[(
            "https://shopify.dev/docs/apps/build/access-scopes.md",
            "# Access scopes\nRefreshed by on-demand fetch.\n",
        )]);

        refresh_url_from_source(
            &paths,
            "https://shopify.dev/docs/apps/build/access-scopes",
            &source,
        )
        .await
        .unwrap();

        let refreshed = get_doc(&conn, "/docs/apps/build/access-scopes")
            .unwrap()
            .unwrap();
        assert_eq!(refreshed.source, "sitemap");
        assert_ne!(refreshed.content_sha, record.content_sha);
    }

    #[tokio::test]
    async fn on_demand_refetch_removes_stale_tantivy_terms_for_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        write_on_demand_config(&paths, true);
        let source = MockTextSource::new(&[(
            "https://shopify.dev/docs/apps/build/delta.md",
            "# Delta\nFresh replacement term.\n",
        )]);
        let conn = open_db(&paths).unwrap();
        init_db(&conn, SCHEMA_VERSION).unwrap();
        let old = store_source_doc(
            &paths,
            &SourceDoc {
                url: "https://shopify.dev/docs/apps/build/delta.md".to_string(),
                title_hint: Some("Delta".to_string()),
                content: "# Delta\nObsoleteUniqueTerm.\n".to_string(),
                source: "on_demand".to_string(),
            },
        )
        .unwrap();
        upsert_doc(&conn, &old).unwrap();
        rebuild_tantivy_from_db(&paths).unwrap();

        refresh_url_from_source(&paths, "https://shopify.dev/docs/apps/build/delta", &source)
            .await
            .unwrap();

        assert!(
            search_docs(&paths, "Fresh replacement", None, 5)
                .unwrap()
                .iter()
                .any(|doc| doc.path == "/docs/apps/build/delta")
        );
        assert!(
            search_docs(&paths, "ObsoleteUniqueTerm", None, 5)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn map_zero_results_suggests_allowed_on_demand_candidate_without_fetching() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "https://shopify.dev/docs/apps/build/missing".to_string(),
                radius: None,
                lens: None,
                version: None,
                max_nodes: Some(5),
            },
        )
        .unwrap();

        let candidate = response
            .meta
            .on_demand_candidate
            .expect("allowed URL-like zero result should produce candidate metadata");
        assert_eq!(candidate.url, "https://shopify.dev/docs/apps/build/missing");
        assert!(!candidate.enabled);
        let conn = open_db(&paths).unwrap();
        init_db(&conn, SCHEMA_VERSION).unwrap();
        assert_eq!(count_docs(&conn).unwrap(), 0);
    }

    #[tokio::test]
    async fn map_zero_results_does_not_suggest_disallowed_on_demand_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "https://shopify.dev/partners".to_string(),
                radius: None,
                lens: None,
                version: None,
                max_nodes: Some(5),
            },
        )
        .unwrap();

        assert!(response.meta.on_demand_candidate.is_none());
    }

    #[tokio::test]
    async fn map_nonzero_results_do_not_add_on_demand_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        fs::create_dir_all(&paths.raw).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn, SCHEMA_VERSION).unwrap();
        let source = SourceDoc {
            url: "https://shopify.dev/docs/apps/build/access-scopes.md".to_string(),
            title_hint: Some("Access scopes".to_string()),
            content: "# Access scopes\nLocal indexed text.\n".to_string(),
            source: "sitemap".to_string(),
        };
        let record = store_source_doc(&paths, &source).unwrap();
        upsert_doc(&conn, &record).unwrap();
        rebuild_tantivy_from_db(&paths).unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "/docs/apps/build/access-scopes".to_string(),
                radius: None,
                lens: None,
                version: None,
                max_nodes: Some(5),
            },
        )
        .unwrap();

        assert!(!response.nodes.is_empty());
        assert!(response.meta.on_demand_candidate.is_none());
    }

    #[tokio::test]
    async fn coverage_repair_retries_failed_allowed_rows_and_skips_policy_rows() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        write_on_demand_config(&paths, true);
        let conn = open_db(&paths).unwrap();
        init_db(&conn, SCHEMA_VERSION).unwrap();
        insert_coverage_event(
            &conn,
            &CoverageEvent {
                source: "sitemap".to_string(),
                canonical_path: Some("/docs/apps/build/repairable".to_string()),
                source_url: "https://shopify.dev/docs/apps/build/repairable".to_string(),
                status: "failed".to_string(),
                reason: Some("network_error".to_string()),
                http_status: None,
                checked_at: "2026-04-17T00:00:00Z".to_string(),
            },
        )
        .unwrap();
        insert_coverage_event(
            &conn,
            &CoverageEvent {
                source: "sitemap".to_string(),
                canonical_path: None,
                source_url: "https://shopify.dev/partners".to_string(),
                status: "failed".to_string(),
                reason: Some("outside_scope".to_string()),
                http_status: None,
                checked_at: "2026-04-17T00:00:00Z".to_string(),
            },
        )
        .unwrap();
        let source = MockTextSource::new(&[(
            "https://shopify.dev/docs/apps/build/repairable.md",
            "# Repairable\nRecovered through coverage repair.\n",
        )]);

        let summary = coverage_repair_from_source(&paths, &source).await.unwrap();

        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.repaired, 1);
        assert_eq!(summary.skipped_policy, 1);
        assert_eq!(
            get_doc(&conn, "/docs/apps/build/repairable")
                .unwrap()
                .unwrap()
                .source,
            "on_demand"
        );
        assert_eq!(
            count_where(
                &conn,
                "coverage_reports",
                "source_url = 'https://shopify.dev/docs/apps/build/repairable' AND status = 'indexed'"
            )
            .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn coverage_repair_disabled_skips_allowed_rows_without_fetching() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn, SCHEMA_VERSION).unwrap();
        insert_coverage_event(
            &conn,
            &CoverageEvent {
                source: "sitemap".to_string(),
                canonical_path: Some("/docs/apps/build/disabled-repair".to_string()),
                source_url: "https://shopify.dev/docs/apps/build/disabled-repair".to_string(),
                status: "failed".to_string(),
                reason: Some("network_error".to_string()),
                http_status: None,
                checked_at: "2026-04-17T00:00:00Z".to_string(),
            },
        )
        .unwrap();
        let source = MockTextSource::new(&[(
            "https://shopify.dev/docs/apps/build/disabled-repair.md",
            "# Should not fetch\n",
        )]);

        let summary = coverage_repair_from_source(&paths, &source).await.unwrap();

        assert_eq!(summary.attempted, 0);
        assert_eq!(summary.skipped_disabled, 1);
        assert!(
            get_doc(&conn, "/docs/apps/build/disabled-repair")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn coverage_status_counts_report_rows() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn, SCHEMA_VERSION).unwrap();
        insert_coverage_event(
            &conn,
            &CoverageEvent {
                source: "sitemap".to_string(),
                canonical_path: Some("/docs/apps/build/access-scopes".to_string()),
                source_url: "https://shopify.dev/docs/apps/build/access-scopes".to_string(),
                status: "skipped".to_string(),
                reason: Some("markdown_not_found".to_string()),
                http_status: Some(404),
                checked_at: "2026-04-17T00:00:00Z".to_string(),
            },
        )
        .unwrap();

        let coverage = coverage_status(&conn).unwrap();
        assert_eq!(coverage.discovered_count, 1);
        assert_eq!(coverage.skipped_count, 1);
        assert_eq!(coverage.sources.sitemap, 1);
    }

    #[test]
    fn sitemap_sourced_optional_scopes_doc_is_searchable() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        fs::create_dir_all(&paths.raw).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn, SCHEMA_VERSION).unwrap();
        let source = SourceDoc {
            url: "https://shopify.dev/docs/apps/build/access-scopes.md".to_string(),
            title_hint: Some("Access scopes".to_string()),
            content: "# Access scopes\nOptional access scopes and managed access scopes let apps request protected permissions.\n".to_string(),
            source: "sitemap".to_string(),
        };
        let record = store_source_doc(&paths, &source).unwrap();
        upsert_doc(&conn, &record).unwrap();
        rebuild_tantivy_from_db(&paths).unwrap();

        let results = search_docs(&paths, "optional scopes", None, 5).unwrap();
        assert_eq!(results[0].path, "/docs/apps/build/access-scopes");
        assert_eq!(results[0].source, "sitemap");
    }

    #[tokio::test]
    async fn changelog_resolved_concept_marks_connected_doc_stale() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let conn = seed_draft_order_graph(&paths);
        let feed = changelog_feed(
            "DraftOrderLineItem.grams field removed in 2026-07",
            "The DraftOrderLineItem.grams field will be removed. Migrate to weight.",
            "https://shopify.dev/changelog/draft-order-line-item-grams",
        );
        let source = MockTextSource::new(&[(SHOPIFY_CHANGELOG_FEED_URL, &feed)]);

        let report = poll_changelog_from_source(&paths, SHOPIFY_CHANGELOG_FEED_URL, &source)
            .await
            .unwrap();

        assert_eq!(report.entries_inserted, 1);
        assert_eq!(report.entries_seen, 1);
        assert!(report.warnings.is_empty());
        assert!(report.scheduled_changes > 0);
        let doc = get_doc(
            &conn,
            "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem",
        )
        .unwrap()
        .unwrap();
        assert!(doc.references_deprecated);
        assert!(
            doc.deprecated_refs
                .iter()
                .any(|reference| reference == "DraftOrderLineItem.grams")
        );

        let fetched = shopify_fetch(
            &paths,
            &FetchArgs {
                path: Some(
                    "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem".to_string(),
                ),
                url: None,
                anchor: None,
                include_code_blocks: None,
                max_chars: None,
            },
        )
        .await
        .unwrap();
        assert!(fetched.staleness.references_deprecated);
        assert!(
            fetched
                .staleness
                .upcoming_changes
                .iter()
                .any(|change| change["type_name"] == "DraftOrderLineItem.grams")
        );
    }

    #[tokio::test]
    async fn map_doc_node_includes_scheduled_change_staleness() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        seed_draft_order_graph(&paths);
        let feed = changelog_feed(
            "DraftOrderLineItem.grams field removed in 2026-07",
            "The DraftOrderLineItem.grams field will be removed. Migrate to weight.",
            "https://shopify.dev/changelog/draft-order-line-item-grams-map",
        );
        let source = MockTextSource::new(&[(SHOPIFY_CHANGELOG_FEED_URL, &feed)]);
        poll_changelog_from_source(&paths, SHOPIFY_CHANGELOG_FEED_URL, &source)
            .await
            .unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem".to_string(),
                radius: Some(1),
                lens: Some("doc".to_string()),
                version: Some("2026-04".to_string()),
                max_nodes: Some(10),
            },
        )
        .unwrap();

        let doc_node = response
            .nodes
            .iter()
            .find(|node| node.path == "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem")
            .expect("map response should include the affected doc node");
        assert!(doc_node.staleness.references_deprecated);
        assert!(doc_node.staleness.upcoming_changes.iter().any(|change| {
            change["type_name"] == "DraftOrderLineItem.grams"
                && change["change"] == "removal"
                && change["effective_date"] == "2026-07"
                && change["migration_hint"]
                    .as_str()
                    .is_some_and(|hint| hint.contains("Migrate to weight"))
        }));
    }

    #[tokio::test]
    async fn changelog_unknown_symbol_is_unresolved_and_does_not_mark_docs() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let conn = seed_draft_order_graph(&paths);
        let feed = changelog_feed(
            "UnknownType.foo field removed in 2026-07",
            "The UnknownType.foo field will be removed.",
            "https://shopify.dev/changelog/unknown-type-field",
        );
        let source = MockTextSource::new(&[(SHOPIFY_CHANGELOG_FEED_URL, &feed)]);

        poll_changelog_from_source(&paths, SHOPIFY_CHANGELOG_FEED_URL, &source)
            .await
            .unwrap();

        let unresolved: String = conn
            .query_row(
                "SELECT unresolved_affected_refs FROM changelog_entries",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            parse_json_string_vec(Some(&unresolved))
                .iter()
                .any(|reference| reference == "UnknownType.foo")
        );
        assert_eq!(count_where(&conn, "scheduled_changes", "1=1").unwrap(), 0);
        let doc = get_doc(
            &conn,
            "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem",
        )
        .unwrap()
        .unwrap();
        assert!(!doc.references_deprecated);
    }

    #[tokio::test]
    async fn changelog_doc_link_resolves_through_docs_and_edges() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let conn = seed_draft_order_graph(&paths);
        let feed = changelog_feed(
            "Draft order docs updated for a removal in 2026-07",
            "See https://shopify.dev/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem for the removal.",
            "https://shopify.dev/changelog/draft-order-doc-removal",
        );
        let source = MockTextSource::new(&[(SHOPIFY_CHANGELOG_FEED_URL, &feed)]);

        poll_changelog_from_source(&paths, SHOPIFY_CHANGELOG_FEED_URL, &source)
            .await
            .unwrap();

        let affected: String = conn
            .query_row("SELECT affected_types FROM changelog_entries", [], |row| {
                row.get(0)
            })
            .unwrap();
        let affected = parse_json_string_vec(Some(&affected));
        assert!(affected.iter().any(|reference| {
            reference == "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem"
        }));
        assert!(
            affected
                .iter()
                .any(|reference| reference == "DraftOrderLineItem.grams")
        );
        let doc = get_doc(
            &conn,
            "/docs/api/admin-graphql/2026-04/objects/DraftOrderLineItem",
        )
        .unwrap()
        .unwrap();
        assert!(doc.references_deprecated);
    }

    #[tokio::test]
    async fn status_reports_freshness_workers_and_changelog_counts() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let _conn = seed_draft_order_graph(&paths);
        let feed = changelog_feed(
            "DraftOrderLineItem.grams field removed in 2026-07",
            "The DraftOrderLineItem.grams field will be removed.",
            "https://shopify.dev/changelog/draft-order-status",
        );
        let source = MockTextSource::new(&[(SHOPIFY_CHANGELOG_FEED_URL, &feed)]);

        poll_changelog_from_source(&paths, SHOPIFY_CHANGELOG_FEED_URL, &source)
            .await
            .unwrap();
        refresh_stale_docs_from_source(&paths, &source)
            .await
            .unwrap();
        let status = status(&paths).unwrap();

        assert_eq!(status.changelog.entry_count, 1);
        assert!(status.changelog.scheduled_change_count > 0);
        assert!(status.workers.last_changelog_at.is_some());
        assert!(status.workers.last_aging_sweep_at.is_some());
        assert_eq!(status.freshness.fresh_count, 1);
    }

    #[tokio::test]
    async fn status_reports_changelog_polling_warning() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = MockTextSource::new(&[(SHOPIFY_CHANGELOG_FEED_URL, "not an rss feed")]);

        let report = poll_changelog_from_source(&paths, SHOPIFY_CHANGELOG_FEED_URL, &source)
            .await
            .unwrap();

        assert_eq!(report.entries_inserted, 0);
        assert_eq!(report.warnings.len(), 1);
        let status = status(&paths).unwrap();
        assert!(status.changelog.last_warning.is_some());
        assert!(
            status
                .warnings
                .iter()
                .any(|warning| warning.contains("Changelog polling warning"))
        );
    }

    #[tokio::test]
    async fn version_watcher_enqueues_validated_unindexed_latest_version() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn, SCHEMA_VERSION).unwrap();
        let source = MockTextSource::new(&[
            (
                SHOPIFY_VERSIONING_URL,
                "Stable version Release date Supported until 2026-04 April 1 2026 2026-07 July 1 2026",
            ),
            (
                &admin_graphql_direct_proxy_url("2026-07"),
                minimal_introspection(),
            ),
        ]);

        let report = check_new_versions_from_source(&paths, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        assert_eq!(report.latest_candidate.as_deref(), Some("2026-07"));
        assert!(report.enqueued);
        assert!(get_meta(&conn, "last_version_check_at").unwrap().is_some());
        assert_eq!(
            count_where(
                &conn,
                "version_rebuild_queue",
                "version = '2026-07' AND api_surface = 'admin_graphql' AND status = 'pending'"
            )
            .unwrap(),
            1
        );
        let status = status(&paths).unwrap();
        assert!(
            status
                .warnings
                .iter()
                .any(|warning| warning.contains("version rebuild request"))
        );
    }

    #[tokio::test]
    async fn version_watcher_does_not_enqueue_already_indexed_latest_version() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        fs::create_dir_all(&paths.raw).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn, SCHEMA_VERSION).unwrap();
        let source_doc = SourceDoc {
            url: "https://shopify.dev/docs/api/admin-graphql/2026-07/objects/Product.md"
                .to_string(),
            title_hint: Some("Product".to_string()),
            content: "# Product\nCurrent product reference.\n".to_string(),
            source: "sitemap".to_string(),
        };
        let record = store_source_doc(&paths, &source_doc).unwrap();
        upsert_doc(&conn, &record).unwrap();
        refresh_indexed_versions(&conn, &[record]).unwrap();
        let source = MockTextSource::new(&[(
            SHOPIFY_VERSIONING_URL,
            "Stable version Release date Supported until 2026-04 April 1 2026 2026-07 July 1 2026",
        )]);

        let report = check_new_versions_from_source(&paths, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        assert_eq!(report.latest_candidate.as_deref(), Some("2026-07"));
        assert!(report.already_indexed);
        assert!(!report.enqueued);
        assert_eq!(
            count_where(&conn, "version_rebuild_queue", "1=1").unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn version_watcher_rejects_html_only_candidate_without_schema_validation() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn, SCHEMA_VERSION).unwrap();
        let source = MockTextSource::new(&[(
            SHOPIFY_VERSIONING_URL,
            "Stable version Release date Supported until 2026-10 October 1 2026",
        )]);

        let report = check_new_versions_from_source(&paths, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        assert_eq!(report.latest_candidate, None);
        assert!(!report.enqueued);
        assert!(report.warning.is_some());
        assert_eq!(
            count_where(&conn, "version_rebuild_queue", "1=1").unwrap(),
            0
        );
    }

    #[test]
    fn reads_newline_delimited_mcp_message() {
        let input = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}
"#;
        let mut reader = std::io::BufReader::new(&input[..]);
        let message = read_mcp_message(&mut reader).unwrap().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&message).unwrap();
        assert_eq!(value["method"], "tools/list");
    }

    #[test]
    fn writes_newline_delimited_mcp_message() {
        let mut output = Vec::new();
        write_mcp_message(
            &mut output,
            &serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}}),
        )
        .unwrap();
        assert!(output.ends_with(b"\n"));
        assert!(!output.starts_with(b"Content-Length:"));
    }

    #[tokio::test]
    async fn mcp_initialize_tools_status_sequence_is_fast() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let started = std::time::Instant::now();
        let initialize = handle_mcp_request(
            &paths,
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        )
        .await;
        let elapsed = started.elapsed();
        assert!(elapsed.as_millis() < 20);
        assert_eq!(
            initialize["result"]["serverInfo"]["name"],
            "shopify-rextant"
        );

        let tools = handle_mcp_request(
            &paths,
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await;
        assert_eq!(tools["result"]["tools"][0]["name"], "shopify_map");

        let status = handle_mcp_request(
            &paths,
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":3,
                "method":"tools/call",
                "params":{"name":"shopify_status","arguments":{}}
            }),
        )
        .await;
        assert_eq!(
            status["result"]["structuredContent"]["schema_version"],
            SCHEMA_VERSION
        );
    }

    #[tokio::test]
    async fn full_build_uses_llms_and_sitemap_union_from_mock_sources() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = fixture_sources();

        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let conn = open_db(&paths).unwrap();
        assert_eq!(count_docs(&conn).unwrap(), 3);
        let admin_root = get_doc(&conn, "/docs/api/admin-graphql").unwrap().unwrap();
        assert_eq!(admin_root.source, "llms");
        let access_scopes = get_doc(&conn, "/docs/apps/build/access-scopes")
            .unwrap()
            .unwrap();
        assert_eq!(access_scopes.source, "sitemap");

        let coverage = coverage_status(&conn).unwrap();
        assert_eq!(coverage.discovered_count, 2);
        assert_eq!(coverage.indexed_count, 2);
        assert_eq!(coverage.sources.llms, 1);
        assert_eq!(coverage.sources.sitemap, 1);

        let results = search_docs(&paths, "optional scopes", None, 5).unwrap();
        assert_eq!(results[0].path, "/docs/apps/build/access-scopes");
    }

    #[tokio::test]
    async fn search_docs_uses_lindera_for_japanese_queries_without_regressing_english() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = admin_graphql_graph_sources();

        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let japanese_results = search_docs(&paths, "割引 クーポン", Some("2026-04"), 5).unwrap();
        assert!(
            japanese_results
                .iter()
                .any(|doc| doc.path == "/docs/apps/build/discounts/cart-level"),
            "Japanese discount/coupon query should reach the cart-level discount doc: {:?}",
            japanese_results
                .iter()
                .map(|doc| doc.path.as_str())
                .collect::<Vec<_>>()
        );

        let english_results = search_docs(&paths, "Product", Some("2026-04"), 5).unwrap();
        assert!(
            english_results
                .iter()
                .any(|doc| doc.path == "/docs/api/admin-graphql/2026-04/objects/Product"),
            "English type-name search should continue to reach Product docs: {:?}",
            english_results
                .iter()
                .map(|doc| doc.path.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn build_recreates_legacy_tantivy_index_when_search_schema_changes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        fs::create_dir_all(&paths.tantivy).unwrap();
        let mut legacy_builder = Schema::builder();
        legacy_builder.add_text_field("path", STRING | STORED);
        legacy_builder.add_text_field("title", TEXT | STORED);
        legacy_builder.add_text_field("url", STRING | STORED);
        legacy_builder.add_text_field("version", STRING | STORED);
        legacy_builder.add_text_field("api_surface", STRING | STORED);
        legacy_builder.add_text_field("doc_type", STRING | STORED);
        legacy_builder.add_text_field("content", TEXT);
        let legacy_index = Index::create_in_dir(&paths.tantivy, legacy_builder.build()).unwrap();
        legacy_index
            .writer::<TantivyDocument>(50_000_000)
            .unwrap()
            .commit()
            .unwrap();
        assert!(
            SearchFields::from_schema(&legacy_index.schema()).is_err(),
            "legacy index fixture should not contain v0.4 content_en/content_ja fields"
        );

        let source = admin_graphql_graph_sources();
        build_index_from_sources(&paths, false, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let index = Index::open_in_dir(&paths.tantivy).unwrap();
        assert!(
            SearchFields::from_schema(&index.schema()).is_ok(),
            "build should recreate old Tantivy indexes with the current search schema"
        );
        let japanese_results = search_docs(&paths, "割引 クーポン", Some("2026-04"), 5).unwrap();
        assert!(
            japanese_results
                .iter()
                .any(|doc| doc.path == "/docs/apps/build/discounts/cart-level"),
            "recreated index should support Japanese search"
        );
    }

    #[tokio::test]
    async fn full_build_indexes_admin_graphql_concepts_edges_and_status_graph_counts() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = admin_graphql_graph_sources();

        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let schema_snapshot = paths
            .data
            .join("schemas/admin-graphql/2026-04.introspection.json");
        assert!(
            schema_snapshot.exists(),
            "v0.2.0 build must persist the Admin GraphQL introspection snapshot at {}",
            schema_snapshot.display()
        );

        let conn = open_db(&paths).unwrap();
        let concept_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM concepts", [], |row| row.get(0))
            .unwrap();
        let edge_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))
            .unwrap();
        assert!(
            concept_count >= 3,
            "expected Product, ProductVariant, and ProductInput concepts, got {concept_count}"
        );
        assert!(
            edge_count >= 3,
            "expected defined_in/has_field/returns edges, got {edge_count}"
        );

        let status_json = to_json_value(status(&paths).unwrap());
        assert!(
            status_json
                .pointer("/index/concept_count")
                .and_then(Value::as_i64)
                .is_some_and(|count| count > 0),
            "shopify_status must expose index.concept_count: {status_json:#}"
        );
        assert!(
            status_json
                .pointer("/index/edge_count")
                .and_then(Value::as_i64)
                .is_some_and(|count| count > 0),
            "shopify_status must expose index.edge_count: {status_json:#}"
        );
    }

    #[tokio::test]
    async fn full_build_records_missing_raw_markdown_as_skipped_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = MockTextSource::new(&[
            (SHOPIFY_LLMS_URL, ""),
            (
                SHOPIFY_SITEMAP_URL,
                r#"
                <urlset>
                  <url><loc>https://shopify.dev/docs/apps/build/html-only</loc></url>
                </urlset>
                "#,
            ),
        ]);

        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let response = status(&paths).unwrap();
        assert_eq!(response.doc_count, 1);
        assert_eq!(response.coverage.discovered_count, 1);
        assert_eq!(response.coverage.skipped_count, 1);
        assert_eq!(response.coverage.failed_count, 0);
        assert_eq!(response.coverage.sources.sitemap, 1);
        assert!(response.coverage.last_sitemap_at.is_some());
        assert!(
            response
                .warnings
                .iter()
                .any(|warning| warning.contains("skipped because raw markdown was unavailable"))
        );
    }

    #[tokio::test]
    async fn refresh_without_path_sweeps_only_aging_or_stale_docs() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        fs::create_dir_all(&paths.raw).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn, SCHEMA_VERSION).unwrap();
        let fresh_source = SourceDoc {
            url: "https://shopify.dev/docs/apps/build/fresh.md".to_string(),
            title_hint: Some("Fresh doc".to_string()),
            content: "# Fresh doc\nCurrent content.\n".to_string(),
            source: "sitemap".to_string(),
        };
        let stale_source = SourceDoc {
            url: "https://shopify.dev/docs/apps/build/stale.md".to_string(),
            title_hint: Some("Stale doc".to_string()),
            content: "# Stale doc\nOld content.\n".to_string(),
            source: "sitemap".to_string(),
        };
        let fresh = store_source_doc(&paths, &fresh_source).unwrap();
        let stale = store_source_doc(&paths, &stale_source).unwrap();
        let fresh_sha = fresh.content_sha.clone();
        upsert_doc(&conn, &fresh).unwrap();
        upsert_doc(&conn, &stale).unwrap();
        let old_verified = (Utc::now() - chrono::Duration::days(40)).to_rfc3339();
        conn.execute(
            "UPDATE docs SET last_verified = ?1, freshness = 'stale' WHERE path = ?2",
            params![old_verified, stale.path],
        )
        .unwrap();
        rebuild_tantivy_from_db(&paths).unwrap();
        let source = MockTextSource::new(&[(
            "https://shopify.dev/docs/apps/build/stale.md",
            "# Stale doc\nRefreshed content.\n",
        )]);

        refresh_stale_docs_from_source(&paths, &source)
            .await
            .unwrap();

        let fresh_after = get_doc(&conn, "/docs/apps/build/fresh").unwrap().unwrap();
        let stale_after = get_doc(&conn, "/docs/apps/build/stale").unwrap().unwrap();
        assert_eq!(fresh_after.content_sha, fresh_sha);
        assert_ne!(stale_after.content_sha, stale.content_sha);
        assert!(get_meta(&conn, "last_aging_sweep_at").unwrap().is_some());
    }

    #[tokio::test]
    async fn map_response_exposes_fts_contract_for_zero_results() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = fixture_sources();
        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "no-local-doc-should-match-this".to_string(),
                radius: None,
                lens: None,
                version: None,
                max_nodes: Some(5),
            },
        )
        .unwrap();

        assert!(!response.meta.graph_available);
        assert_eq!(response.meta.query_interpretation.resolved_as, "free_text");
        assert_eq!(response.meta.query_interpretation.confidence, "low");
        assert!(response.meta.query_interpretation.entry_points.is_empty());
        assert!(response.nodes.is_empty());
        assert!(response.edges.is_empty());
        assert_eq!(response.query_plan[0].action, "inspect_status");
        assert_eq!(response.query_plan[1].action, "refresh");
    }

    #[tokio::test]
    async fn map_product_returns_graph_backed_concept_map() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = admin_graphql_graph_sources();
        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "Product".to_string(),
                radius: Some(2),
                lens: Some("concept".to_string()),
                version: Some("2026-04".to_string()),
                max_nodes: Some(20),
            },
        )
        .unwrap();

        assert!(
            response.meta.graph_available,
            "v0.2.0 Product map must be graph-backed"
        );
        assert_eq!(
            response.meta.query_interpretation.resolved_as,
            "concept_name"
        );
        assert_eq!(response.center.kind, "concept");
        assert!(
            response.center.id.contains("Product"),
            "center should be the Product concept, got {:?}",
            response.center.id
        );
        assert!(
            !response.edges.is_empty(),
            "Product concept map should include graph edges"
        );
        assert!(
            response
                .suggested_reading_order
                .iter()
                .any(|path| path == "/docs/api/admin-graphql/2026-04/objects/Product"),
            "reading order must include the Product reference doc: {:?}",
            response.suggested_reading_order
        );
    }

    #[tokio::test]
    async fn map_doc_path_reaches_product_concept() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = admin_graphql_graph_sources();
        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "/docs/api/admin-graphql/2026-04/objects/Product".to_string(),
                radius: Some(2),
                lens: Some("doc".to_string()),
                version: Some("2026-04".to_string()),
                max_nodes: Some(20),
            },
        )
        .unwrap();

        assert!(
            response.meta.graph_available,
            "doc path maps should use the graph when Admin GraphQL concepts are indexed"
        );
        assert_eq!(response.meta.query_interpretation.resolved_as, "doc_path");
        assert_eq!(response.center.kind, "doc");
        let product_concept = response
            .nodes
            .iter()
            .find(|node| node.kind == "concept" && node.id.contains("Product"))
            .expect("Product reference doc should reach the Product concept");
        let product_doc_path = "/docs/api/admin-graphql/2026-04/objects/Product";
        assert!(response.edges.iter().any(|edge| {
            let from = edge.get("from").and_then(Value::as_str);
            let to = edge.get("to").and_then(Value::as_str);
            (from == Some(product_doc_path) && to == Some(product_concept.id.as_str()))
                || (from == Some(product_concept.id.as_str()) && to == Some(product_doc_path))
        }));
    }

    #[tokio::test]
    async fn map_free_text_promotes_docs_to_graph_entry_points() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = admin_graphql_graph_sources();
        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "discount function cart level".to_string(),
                radius: Some(2),
                lens: Some("auto".to_string()),
                version: Some("2026-04".to_string()),
                max_nodes: Some(20),
            },
        )
        .unwrap();

        assert!(
            response.meta.graph_available,
            "free text maps should promote FTS docs into graph entry points when possible"
        );
        assert_eq!(response.meta.query_interpretation.resolved_as, "free_text");
        assert!(
            !response.meta.query_interpretation.entry_points.is_empty(),
            "free text maps should report the FTS entry points used for graph expansion"
        );
        assert!(
            response
                .nodes
                .iter()
                .any(|node| node.path == "/docs/apps/build/discounts/cart-level"),
            "promoted discount cart-level doc should remain in the result"
        );
        assert!(
            response
                .nodes
                .iter()
                .any(|node| node.path == "/docs/apps/build/discounts/overview"),
            "unpromoted FTS docs should not be dropped from the result"
        );
        assert!(
            !response.edges.is_empty(),
            "promoted free text maps should include graph edges"
        );
    }

    #[tokio::test]
    async fn map_without_schema_falls_back_to_fts_with_graph_warning() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = fixture_sources();
        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "Admin GraphQL".to_string(),
                radius: Some(2),
                lens: Some("auto".to_string()),
                version: None,
                max_nodes: Some(5),
            },
        )
        .unwrap();

        assert!(!response.meta.graph_available);
        assert!(
            response.meta.coverage_warning.is_some(),
            "fallback maps should explain that graph coverage or schema data is missing"
        );
        assert_eq!(
            response.query_plan.first().map(|step| step.action.as_str()),
            Some("inspect_status"),
            "fallback maps should ask the agent to inspect status before trusting graph coverage"
        );
    }

    #[tokio::test]
    async fn graph_edges_only_reference_returned_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = admin_graphql_graph_sources();
        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "Product".to_string(),
                radius: Some(1),
                lens: Some("concept".to_string()),
                version: Some("2026-04".to_string()),
                max_nodes: Some(5),
            },
        )
        .unwrap();

        assert!(
            !response.edges.is_empty(),
            "edge closure can only be checked when graph edges are returned"
        );
        let returned_ids = response
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        for edge in &response.edges {
            let from = edge
                .get("from")
                .and_then(Value::as_str)
                .expect("edge.from must be a string");
            let to = edge
                .get("to")
                .and_then(Value::as_str)
                .expect("edge.to must be a string");
            assert!(
                returned_ids.contains(from),
                "edge.from must reference a returned node: {edge:#}"
            );
            assert!(
                returned_ids.contains(to),
                "edge.to must reference a returned node: {edge:#}"
            );
        }
    }

    #[tokio::test]
    async fn reading_order_contains_only_doc_paths() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = admin_graphql_graph_sources();
        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let response = shopify_map(
            &paths,
            &MapArgs {
                from: "Product".to_string(),
                radius: Some(2),
                lens: Some("concept".to_string()),
                version: Some("2026-04".to_string()),
                max_nodes: Some(20),
            },
        )
        .unwrap();

        assert!(
            response.meta.graph_available,
            "reading order should be checked on a graph-backed map"
        );
        assert!(
            !response.suggested_reading_order.is_empty(),
            "graph-backed maps should suggest source docs to read"
        );
        assert!(
            response
                .suggested_reading_order
                .iter()
                .all(|path| path.starts_with("/docs/") || path.starts_with("/changelog/")),
            "suggested_reading_order should contain only doc paths: {:?}",
            response.suggested_reading_order
        );
    }

    #[tokio::test]
    async fn build_persists_and_reuses_graph_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        let source = admin_graphql_graph_sources();
        build_index_from_sources(&paths, true, None, &IndexSourceUrls::default(), &source)
            .await
            .unwrap();

        let graph_snapshot = paths.data.join("graph.msgpack");
        assert!(
            graph_snapshot.exists(),
            "graph-backed builds must persist {}",
            graph_snapshot.display()
        );

        let started = std::time::Instant::now();
        let initialize = handle_mcp_request(
            &paths,
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        )
        .await;
        let tools = handle_mcp_request(
            &paths,
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await;
        let status = handle_mcp_request(
            &paths,
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":3,
                "method":"tools/call",
                "params":{"name":"shopify_status","arguments":{}}
            }),
        )
        .await;
        let elapsed = started.elapsed();

        assert_eq!(
            initialize["result"]["serverInfo"]["name"],
            "shopify-rextant"
        );
        assert_eq!(tools["result"]["tools"][0]["name"], "shopify_map");
        assert!(
            status["result"]["structuredContent"]
                .pointer("/index/edge_count")
                .and_then(Value::as_i64)
                .is_some_and(|count| count > 0),
            "status should expose graph counts after loading the graph snapshot: {status:#}"
        );
        assert!(
            elapsed.as_millis() < 20,
            "MCP initialize/tools/status should stay fast with graph snapshot present; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn fetch_response_applies_anchor_code_filter_and_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        fs::create_dir_all(&paths.raw).unwrap();
        let conn = open_db(&paths).unwrap();
        init_db(&conn, SCHEMA_VERSION).unwrap();
        let source = SourceDoc {
            url: "https://shopify.dev/docs/apps/build/access-scopes.md".to_string(),
            title_hint: Some("Access scopes".to_string()),
            content: "# Access scopes\nIntro\n\n## Managed access scopes\nText before code.\n```graphql\nquery { shop { name } }\n```\nMore text after code that keeps this section long enough to truncate.\n\n## Next\nDone\n".to_string(),
            source: "sitemap".to_string(),
        };
        let record = store_source_doc(&paths, &source).unwrap();
        upsert_doc(&conn, &record).unwrap();

        let response = shopify_fetch(
            &paths,
            &FetchArgs {
                path: Some("/docs/apps/build/access-scopes".to_string()),
                url: None,
                anchor: Some("managed-access-scopes".to_string()),
                include_code_blocks: Some(false),
                max_chars: Some(80),
            },
        )
        .await
        .unwrap();

        assert!(response.sections.iter().any(|section| {
            section.anchor == "managed-access-scopes" && section.title == "Managed access scopes"
        }));
        assert!(response.content.contains("## Managed access scopes"));
        assert!(response.content.contains("Text before code."));
        assert!(!response.content.contains("query { shop"));
        assert!(!response.content.contains("## Next"));
        assert!(response.truncated);
        assert!(response.content.chars().count() <= 80);
    }

    #[test]
    fn reads_content_length_framed_mcp_message() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let mut input = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        input.extend_from_slice(body);
        let mut reader = std::io::BufReader::new(&input[..]);

        let message = read_mcp_message(&mut reader).unwrap().unwrap();
        assert_eq!(message, body);
        let value: serde_json::Value = serde_json::from_slice(&message).unwrap();
        assert_eq!(value["method"], "initialize");
    }
}
