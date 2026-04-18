use criterion::{Criterion, criterion_group, criterion_main};
use std::collections::HashMap;
use std::hint::black_box;
use std::sync::OnceLock;

#[allow(dead_code, private_interfaces, unused_imports)]
#[path = "../src/lib.rs"]
mod app;

struct Fixture {
    _dir: tempfile::TempDir,
    paths: app::Paths,
    runtime: tokio::runtime::Runtime,
}

struct MockTextSource {
    texts: HashMap<String, String>,
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

impl app::TextSource for MockTextSource {
    async fn fetch_text(&self, url: &str) -> Result<String, app::SourceFetchError> {
        self.texts
            .get(url)
            .cloned()
            .ok_or_else(|| app::SourceFetchError {
                status: "skipped".to_string(),
                reason: "mock_not_found".to_string(),
                http_status: Some(404),
            })
    }
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let dir = tempfile::tempdir().expect("create benchmark temp dir");
        let paths = app::Paths::new(Some(dir.path().to_path_buf())).expect("create paths");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("create benchmark runtime");
        let source = release_fixture_sources();
        runtime
            .block_on(app::build_index_from_sources(
                &paths,
                true,
                None,
                &app::IndexSourceUrls::default(),
                &source,
            ))
            .expect("build benchmark fixture index");
        Fixture {
            _dir: dir,
            paths,
            runtime,
        }
    })
}

fn release_fixture_sources() -> MockTextSource {
    MockTextSource::new(&[
        (
            "https://shopify.dev/llms.txt",
            "[Product](/docs/api/admin-graphql/2026-04/objects/Product)\n\
             [Products guide](/docs/apps/build/products)\n\
             [Cart level discount functions](/docs/apps/build/discounts/cart-level)\n\
             [Discount overview](/docs/apps/build/discounts/overview)\n",
        ),
        (
            "https://shopify.dev/sitemap.xml",
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
            "https://shopify.dev/changelog/feed.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel></channel></rss>"#,
        ),
        (
            "https://shopify.dev/docs/api/usage/versioning",
            "Stable version Release date Supported until 2026-04 April 1 2026",
        ),
        (
            "https://shopify.dev/admin-graphql-direct-proxy/2026-04",
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

fn bench_release_contract(c: &mut Criterion) {
    let fixture = fixture();
    let mut group = c.benchmark_group("release_contract");

    group.bench_function("status", |b| {
        b.iter(|| black_box(app::status(black_box(&fixture.paths)).expect("status")))
    });

    group.bench_function("search_product", |b| {
        b.iter(|| {
            black_box(
                app::search_docs(
                    black_box(&fixture.paths),
                    black_box("Product"),
                    Some("2026-04"),
                    5,
                )
                .expect("search Product"),
            )
        })
    });

    group.bench_function("map_product", |b| {
        b.iter(|| {
            black_box(
                app::shopify_map(
                    black_box(&fixture.paths),
                    &app::MapArgs {
                        from: black_box("Product").to_string(),
                        radius: Some(2),
                        lens: Some("concept".to_string()),
                        version: Some("2026-04".to_string()),
                        max_nodes: Some(20),
                    },
                )
                .expect("map Product"),
            )
        })
    });

    group.bench_function("fetch_product", |b| {
        b.iter(|| {
            black_box(
                fixture
                    .runtime
                    .block_on(app::shopify_fetch(
                        black_box(&fixture.paths),
                        &app::FetchArgs {
                            path: Some(
                                "/docs/api/admin-graphql/2026-04/objects/Product".to_string(),
                            ),
                            url: None,
                            anchor: None,
                            include_code_blocks: Some(true),
                            max_chars: Some(20_000),
                        },
                    ))
                    .expect("fetch Product"),
            )
        })
    });

    group.finish();
}

criterion_group!(benches, bench_release_contract);
criterion_main!(benches);
