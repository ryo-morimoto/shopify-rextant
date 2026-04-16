use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use regex::Regex;
use reqwest::StatusCode;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, STORED, STRING, Schema, TEXT, TantivyDocument};
use tantivy::{Document, Index, doc};
use url::Url;

const SHOPIFY_LLMS_URL: &str = "https://shopify.dev/llms.txt";
const USER_AGENT: &str = concat!("shopify-rextant/", env!("CARGO_PKG_VERSION"));
const SCHEMA_VERSION: &str = "1";

#[derive(Debug, Parser)]
#[command(name = "shopify-rextant")]
#[command(version)]
#[command(about = "Local Shopify docs map MCP server")]
struct Cli {
    #[arg(long, env = "SHOPIFY_REXTANT_HOME")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve,
    Build {
        #[arg(long)]
        force: bool,
        #[arg(long)]
        limit: Option<usize>,
    },
    Refresh {
        path: Option<String>,
    },
    Status,
    Search {
        query: String,
        #[arg(long)]
        version: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    Show {
        path: String,
    },
    Version,
}

#[derive(Debug, Clone)]
struct Paths {
    data: PathBuf,
    raw: PathBuf,
    tantivy: PathBuf,
    db: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
struct DocRecord {
    path: String,
    title: String,
    url: String,
    version: Option<String>,
    doc_type: String,
    api_surface: Option<String>,
    content_class: String,
    content_sha: String,
    last_verified: String,
    last_changed: String,
    freshness: String,
    summary_raw: String,
    reading_time_min: Option<i64>,
    raw_path: String,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    schema_version: String,
    data_dir: String,
    index_built: bool,
    doc_count: i64,
    last_full_build: Option<String>,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct FetchResponse {
    path: String,
    title: String,
    url: String,
    content: String,
    staleness: Staleness,
}

#[derive(Debug, Serialize)]
struct MapResponse {
    center: MapCenter,
    nodes: Vec<MapNode>,
    edges: Vec<Value>,
    suggested_reading_order: Vec<String>,
    query_plan: Vec<String>,
    index_status: StatusResponse,
}

#[derive(Debug, Serialize)]
struct MapCenter {
    id: String,
    kind: String,
    path: Option<String>,
    title: String,
}

#[derive(Debug, Serialize)]
struct MapNode {
    id: String,
    kind: String,
    subkind: String,
    path: String,
    title: String,
    summary_from_source: String,
    version: Option<String>,
    api_surface: Option<String>,
    doc_type: String,
    reading_time_min: Option<i64>,
    staleness: Staleness,
    distance_from_center: usize,
}

#[derive(Debug, Serialize)]
struct Staleness {
    age_days: i64,
    freshness: String,
    content_verified_at: String,
    schema_version: Option<String>,
    references_deprecated: bool,
    deprecated_refs: Vec<String>,
    upcoming_changes: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct MapArgs {
    from: String,
    radius: Option<u8>,
    lens: Option<String>,
    version: Option<String>,
    max_nodes: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct FetchArgs {
    path: String,
    max_chars: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct SearchArgs {
    query: String,
    version: Option<String>,
    limit: Option<usize>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::new(cli.home)?;

    match cli.command {
        Command::Serve => serve(paths),
        Command::Build { force, limit } => build_index(&paths, force, limit).await,
        Command::Refresh { path } => refresh(&paths, path).await,
        Command::Status => print_json(&status(&paths)?),
        Command::Search {
            query,
            version,
            limit,
        } => print_json(&search_docs(&paths, &query, version.as_deref(), limit)?),
        Command::Show { path } => {
            let text = show_doc(&paths, &path)?;
            println!("{text}");
            Ok(())
        }
        Command::Version => {
            println!("shopify-rextant {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

impl Paths {
    fn new(home: Option<PathBuf>) -> Result<Self> {
        let home = match home {
            Some(path) => path,
            None => dirs::home_dir()
                .ok_or_else(|| anyhow!("could not resolve home directory"))?
                .join(".shopify-rextant"),
        };
        let data = home.join("data");
        Ok(Self {
            raw: data.join("raw"),
            tantivy: data.join("tantivy"),
            db: data.join("index.db"),
            data,
        })
    }

    fn raw_file(&self, raw_path: &str) -> PathBuf {
        self.raw.join(raw_path)
    }
}

async fn build_index(paths: &Paths, force: bool, limit: Option<usize>) -> Result<()> {
    if force && paths.data.exists() {
        fs::remove_dir_all(&paths.data)
            .with_context(|| format!("remove {}", paths.data.display()))?;
    }
    fs::create_dir_all(&paths.raw)?;
    fs::create_dir_all(&paths.tantivy)?;

    let conn = open_db(paths)?;
    init_db(&conn)?;
    let schema = search_schema();
    let index = create_or_reset_index(paths, schema.clone(), force)?;
    let mut writer = index.writer(50_000_000)?;
    writer.delete_all_documents()?;

    let client = reqwest::Client::builder().user_agent(USER_AGENT).build()?;
    let llms = fetch_text(&client, SHOPIFY_LLMS_URL).await?;
    let mut docs = vec![SourceDoc {
        url: SHOPIFY_LLMS_URL.to_string(),
        title_hint: Some("Shopify Developer Platform".to_string()),
        content: llms.clone(),
    }];

    let links = parse_markdown_links(&llms);
    let selected_links = links
        .into_iter()
        .filter(|link| is_shopify_doc_url(&link.url))
        .take(limit.unwrap_or(usize::MAX));

    for link in selected_links {
        if let Ok(source) = fetch_source_doc(&client, &link).await {
            docs.push(source);
        }
    }

    let fields = SearchFields::from_schema(&schema)?;
    let tx = conn.unchecked_transaction()?;
    for source in docs {
        let record = store_source_doc(paths, &source)?;
        upsert_doc(&tx, &record)?;
        writer.add_document(doc!(
            fields.path => record.path.clone(),
            fields.title => record.title.clone(),
            fields.url => record.url.clone(),
            fields.version => record.version.clone().unwrap_or_else(|| "evergreen".to_string()),
            fields.api_surface => record.api_surface.clone().unwrap_or_else(|| "unknown".to_string()),
            fields.doc_type => record.doc_type.clone(),
            fields.content => source.content.chars().take(4_000).collect::<String>(),
        ))?;
    }
    tx.execute(
        "INSERT INTO schema_meta(key, value) VALUES('last_full_build', ?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![now_iso()],
    )?;
    tx.commit()?;
    writer.commit()?;
    Ok(())
}

async fn refresh(paths: &Paths, path: Option<String>) -> Result<()> {
    if let Some(path) = path {
        let conn = open_db(paths)?;
        let doc = get_doc(&conn, &path)?.ok_or_else(|| anyhow!("path not found: {path}"))?;
        let client = reqwest::Client::builder().user_agent(USER_AGENT).build()?;
        let content = fetch_text(&client, &doc.url).await?;
        let source = SourceDoc {
            url: doc.url,
            title_hint: Some(doc.title),
            content,
        };
        let record = store_source_doc(paths, &source)?;
        upsert_doc(&conn, &record)?;
        rebuild_tantivy_from_db(paths)?;
        Ok(())
    } else {
        build_index(paths, false, None).await
    }
}

fn serve(paths: Paths) -> Result<()> {
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();

    while let Some(message) = read_mcp_message(&mut reader)? {
        let request: Value = serde_json::from_slice(&message)?;
        if request.get("id").is_none() {
            continue;
        }
        let response = handle_mcp_request(&paths, request);
        write_mcp_message(&mut writer, &response)?;
    }
    Ok(())
}

fn handle_mcp_request(paths: &Paths, request: Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-11-25",
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": {
                "name": "shopify-rextant",
                "version": env!("CARGO_PKG_VERSION")
            }
        })),
        "tools/list" => Ok(json!({ "tools": tool_descriptors() })),
        "tools/call" => call_mcp_tool(paths, request.get("params").cloned().unwrap_or(Value::Null)),
        _ => Err(json_rpc_error(
            -32601,
            "Method not found",
            json!({ "method": method }),
        )),
    };

    match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
    }
}

fn call_mcp_tool(paths: &Paths, params: Value) -> std::result::Result<Value, Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| json_rpc_error(-32602, "Missing tool name", Value::Null))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let value = match name {
        "shopify_status" => status(paths).map(to_json_value),
        "shopify_fetch" => {
            let args: Result<FetchArgs> = serde_json::from_value(args)
                .map_err(|e| anyhow!("invalid shopify_fetch args: {e}"));
            args.and_then(|args| shopify_fetch(paths, &args).map(to_json_value))
        }
        "shopify_map" => {
            let args: Result<MapArgs> =
                serde_json::from_value(args).map_err(|e| anyhow!("invalid shopify_map args: {e}"));
            args.and_then(|args| shopify_map(paths, &args).map(to_json_value))
        }
        "shopify_search" => {
            let args: Result<SearchArgs> = serde_json::from_value(args)
                .map_err(|e| anyhow!("invalid shopify_search args: {e}"));
            args.and_then(|args| {
                search_docs(
                    paths,
                    &args.query,
                    args.version.as_deref(),
                    args.limit.unwrap_or(10),
                )
                .map(to_json_value)
            })
        }
        _ => Err(anyhow!("unknown tool: {name}")),
    }
    .map_err(|e| json_rpc_error(-32000, &e.to_string(), Value::Null))?;

    let text = serde_json::to_string_pretty(&value)
        .map_err(|e| json_rpc_error(-32000, &e.to_string(), Value::Null))?;
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": value,
        "isError": false
    }))
}

fn tool_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "shopify_map",
            "description": "Return a local source map of Shopify docs for a query, path, or concept-like term.",
            "inputSchema": {
                "type": "object",
                "required": ["from"],
                "properties": {
                    "from": { "type": "string" },
                    "radius": { "type": "integer", "enum": [1, 2, 3], "default": 2 },
                    "lens": { "type": "string", "enum": ["concept", "doc", "task", "auto"], "default": "auto" },
                    "version": { "type": "string" },
                    "max_nodes": { "type": "integer", "minimum": 1, "maximum": 100, "default": 30 }
                }
            }
        }),
        json!({
            "name": "shopify_fetch",
            "description": "Fetch raw locally cached Shopify documentation by docs path.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string" },
                    "max_chars": { "type": "integer", "minimum": 1, "default": 20000 }
                }
            }
        }),
        json!({
            "name": "shopify_search",
            "description": "Search the local Shopify documentation index.",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string" },
                    "version": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 10 }
                }
            }
        }),
        json!({
            "name": "shopify_status",
            "description": "Return local index status and warnings.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
    ]
}

fn read_mcp_message<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Ok(None);
        }

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }

        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            let len = value.trim().parse::<usize>()?;
            loop {
                line.clear();
                let bytes = reader.read_line(&mut line)?;
                if bytes == 0 {
                    bail!("unexpected EOF before MCP message body");
                }
                if line.trim_end_matches(['\r', '\n']).is_empty() {
                    break;
                }
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body)?;
            return Ok(Some(body));
        }

        return Ok(Some(trimmed.as_bytes().to_vec()));
    }
}

fn write_mcp_message<W: Write>(writer: &mut W, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn json_rpc_error(code: i64, message: &str, data: Value) -> Value {
    json!({ "code": code, "message": message, "data": data })
}

fn shopify_fetch(paths: &Paths, args: &FetchArgs) -> Result<FetchResponse> {
    let conn = open_db(paths)?;
    let doc =
        get_doc(&conn, &args.path)?.ok_or_else(|| anyhow!("path not found: {}", args.path))?;
    let mut content = fs::read_to_string(paths.raw_file(&doc.raw_path))?;
    if let Some(max_chars) = args.max_chars {
        content = content.chars().take(max_chars).collect();
    }
    Ok(FetchResponse {
        path: doc.path.clone(),
        title: doc.title.clone(),
        url: doc.url.clone(),
        content,
        staleness: staleness(&doc),
    })
}

fn shopify_map(paths: &Paths, args: &MapArgs) -> Result<MapResponse> {
    let limit = args.max_nodes.unwrap_or(30).clamp(1, 100);
    let radius = args.radius.unwrap_or(2).clamp(1, 3);
    let lens = args.lens.as_deref().unwrap_or("auto");
    let mut docs = if args.from.starts_with("/docs/") || args.from == "/llms.txt" {
        let conn = open_db(paths)?;
        get_doc(&conn, &args.from)?.into_iter().collect()
    } else {
        search_docs(paths, &args.from, args.version.as_deref(), limit)?
    };
    docs.truncate(limit);

    let index_status = status(paths)?;
    let center_doc = docs
        .first()
        .ok_or_else(|| anyhow!("no local docs matched {}", args.from))?;
    let center = MapCenter {
        id: center_doc.path.clone(),
        kind: "doc".to_string(),
        path: Some(center_doc.path.clone()),
        title: center_doc.title.clone(),
    };
    let nodes = docs
        .iter()
        .enumerate()
        .map(|(i, doc)| MapNode {
            id: doc.path.clone(),
            kind: "doc".to_string(),
            subkind: doc.content_class.clone(),
            path: doc.path.clone(),
            title: doc.title.clone(),
            summary_from_source: doc.summary_raw.clone(),
            version: doc.version.clone(),
            api_surface: doc.api_surface.clone(),
            doc_type: doc.doc_type.clone(),
            reading_time_min: doc.reading_time_min,
            staleness: staleness(doc),
            distance_from_center: usize::from(i > 0),
        })
        .collect::<Vec<_>>();
    let suggested_reading_order = nodes.iter().map(|node| node.path.clone()).collect();
    Ok(MapResponse {
        center,
        nodes,
        edges: Vec::new(),
        suggested_reading_order,
        query_plan: vec![
            format!("lens={lens}; radius={radius}; v0.1 uses local FTS before concept graph"),
            "Call shopify_fetch on the suggested paths that match your implementation task."
                .to_string(),
            "Use staleness fields to decide whether to refresh before relying on a page."
                .to_string(),
        ],
        index_status,
    })
}

fn status(paths: &Paths) -> Result<StatusResponse> {
    let mut warnings = Vec::new();
    if !paths.db.exists() {
        warnings.push("Index not built. Run `shopify-rextant build` first.".to_string());
        return Ok(StatusResponse {
            schema_version: SCHEMA_VERSION.to_string(),
            data_dir: paths.data.display().to_string(),
            index_built: false,
            doc_count: 0,
            last_full_build: None,
            warnings,
        });
    }

    let conn = open_db(paths)?;
    let doc_count = conn.query_row("SELECT COUNT(*) FROM docs", [], |row| row.get(0))?;
    let last_full_build = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key='last_full_build'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let schema_version = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_else(|| "unknown".to_string());
    if !paths.tantivy.join("meta.json").exists() {
        warnings.push("Tantivy index missing; run `shopify-rextant build`.".to_string());
    }
    Ok(StatusResponse {
        schema_version,
        data_dir: paths.data.display().to_string(),
        index_built: doc_count > 0,
        doc_count,
        last_full_build,
        warnings,
    })
}

fn search_docs(
    paths: &Paths,
    query: &str,
    version: Option<&str>,
    limit: usize,
) -> Result<Vec<DocRecord>> {
    let conn = open_db(paths)?;
    if !paths.tantivy.join("meta.json").exists() {
        return sqlite_like_search(&conn, query, version, limit);
    }
    let schema = search_schema();
    let fields = SearchFields::from_schema(&schema)?;
    let index = Index::open_in_dir(&paths.tantivy)?;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let parser = QueryParser::for_index(&index, vec![fields.title, fields.content, fields.path]);
    let parsed = parser
        .parse_query(query)
        .or_else(|_| parser.parse_query(&escape_query(query)))?;
    let top_docs = searcher.search(&parsed, &TopDocs::with_limit(limit).order_by_score())?;
    let mut records = Vec::new();
    for (_score, address) in top_docs {
        let retrieved = searcher.doc::<TantivyDocument>(address)?;
        let Some(path) = doc_json_field(&retrieved.to_json(&schema), "path") else {
            continue;
        };
        if let Some(record) = get_doc(&conn, &path)? {
            if version.is_none_or(|v| record.version.as_deref() == Some(v)) {
                records.push(record);
            }
        }
    }
    Ok(records)
}

fn sqlite_like_search(
    conn: &Connection,
    query: &str,
    version: Option<&str>,
    limit: usize,
) -> Result<Vec<DocRecord>> {
    let like = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
    let mut stmt = conn.prepare(
        "SELECT path, title, url, version, doc_type, api_surface, content_class, content_sha,
                last_verified, last_changed, freshness, summary_raw, reading_time_min, raw_path
         FROM docs
         WHERE (title LIKE ?1 ESCAPE '\\' OR path LIKE ?1 ESCAPE '\\' OR summary_raw LIKE ?1 ESCAPE '\\')
           AND (?2 IS NULL OR version = ?2)
         ORDER BY hit_count DESC, title
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![like, version, limit as i64], doc_from_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn show_doc(paths: &Paths, path: &str) -> Result<String> {
    let conn = open_db(paths)?;
    let doc = get_doc(&conn, path)?.ok_or_else(|| anyhow!("path not found: {path}"))?;
    fs::read_to_string(paths.raw_file(&doc.raw_path)).map_err(Into::into)
}

fn rebuild_tantivy_from_db(paths: &Paths) -> Result<()> {
    let schema = search_schema();
    let index = create_or_reset_index(paths, schema.clone(), true)?;
    let fields = SearchFields::from_schema(&schema)?;
    let conn = open_db(paths)?;
    let mut stmt = conn.prepare(
        "SELECT path, title, url, version, doc_type, api_surface, content_class, content_sha,
                last_verified, last_changed, freshness, summary_raw, reading_time_min, raw_path
         FROM docs",
    )?;
    let docs = stmt.query_map([], doc_from_row)?;
    let mut writer = index.writer(50_000_000)?;
    for doc in docs {
        let doc = doc?;
        let content = fs::read_to_string(paths.raw_file(&doc.raw_path)).unwrap_or_default();
        writer.add_document(doc!(
            fields.path => doc.path,
            fields.title => doc.title,
            fields.url => doc.url,
            fields.version => doc.version.unwrap_or_else(|| "evergreen".to_string()),
            fields.api_surface => doc.api_surface.unwrap_or_else(|| "unknown".to_string()),
            fields.doc_type => doc.doc_type,
            fields.content => content.chars().take(4_000).collect::<String>(),
        ))?;
    }
    writer.commit()?;
    Ok(())
}

#[derive(Debug)]
struct MarkdownLink {
    title: String,
    url: String,
}

#[derive(Debug)]
struct SourceDoc {
    url: String,
    title_hint: Option<String>,
    content: String,
}

async fn fetch_source_doc(client: &reqwest::Client, link: &MarkdownLink) -> Result<SourceDoc> {
    let candidates = raw_doc_candidates(&link.url)?;
    let mut last_status = None;
    for url in candidates {
        let response = client.get(&url).send().await?;
        if response.status() == StatusCode::OK {
            return Ok(SourceDoc {
                url,
                title_hint: Some(link.title.clone()),
                content: response.text().await?,
            });
        }
        last_status = Some(response.status());
    }
    bail!(
        "no raw candidate succeeded for {} ({:?})",
        link.url,
        last_status
    )
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let response = client.get(url).send().await?;
    let status = response.status();
    if !status.is_success() {
        bail!("GET {url} failed with {status}");
    }
    response.text().await.map_err(Into::into)
}

fn parse_markdown_links(markdown: &str) -> Vec<MarkdownLink> {
    let re = Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").expect("valid regex");
    re.captures_iter(markdown)
        .filter_map(|caps| {
            let title = caps.get(1)?.as_str().trim().to_string();
            let raw_url = caps.get(2)?.as_str().trim();
            let url = if raw_url.starts_with("http") {
                raw_url.to_string()
            } else if raw_url.starts_with('/') {
                format!("https://shopify.dev{raw_url}")
            } else {
                return None;
            };
            Some(MarkdownLink { title, url })
        })
        .collect()
}

fn is_shopify_doc_url(url: &str) -> bool {
    Url::parse(url)
        .ok()
        .and_then(|url| {
            let host_ok = matches!(url.host_str(), Some("shopify.dev" | "www.shopify.dev"));
            Some(host_ok && url.path().starts_with("/docs/"))
        })
        .unwrap_or(false)
}

fn raw_doc_candidates(url: &str) -> Result<Vec<String>> {
    let parsed = Url::parse(url)?;
    let mut base = parsed;
    base.set_query(None);
    base.set_fragment(None);
    let clean = base.to_string().trim_end_matches('/').to_string();
    let mut candidates = Vec::new();
    if clean.ends_with(".md") || clean.ends_with(".txt") {
        candidates.push(clean);
    } else {
        candidates.push(format!("{clean}.md"));
        candidates.push(format!("{clean}.txt"));
        candidates.push(clean);
    }
    Ok(candidates)
}

fn store_source_doc(paths: &Paths, source: &SourceDoc) -> Result<DocRecord> {
    let path = canonical_doc_path(&source.url)?;
    let title = title_from_markdown(&source.content)
        .or_else(|| source.title_hint.clone())
        .unwrap_or_else(|| path.clone());
    let sha = hex_sha256(&source.content);
    let raw_path = raw_path_for(&path);
    let raw_file = paths.raw_file(&raw_path);
    if let Some(parent) = raw_file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&raw_file, &source.content)?;
    let now = now_iso();
    Ok(DocRecord {
        path: path.clone(),
        title,
        url: source.url.clone(),
        version: extract_version(&path),
        doc_type: classify_doc_type(&path),
        api_surface: classify_api_surface(&path),
        content_class: classify_content_class(&path),
        content_sha: sha,
        last_verified: now.clone(),
        last_changed: now,
        freshness: "fresh".to_string(),
        summary_raw: source.content.chars().take(400).collect(),
        reading_time_min: Some(reading_time_min(&source.content)),
        raw_path,
    })
}

fn canonical_doc_path(url: &str) -> Result<String> {
    let parsed = Url::parse(url)?;
    let mut path = parsed.path().trim_end_matches('/').to_string();
    if path.starts_with("/docs/") {
        if let Some(stripped) = path.strip_suffix(".md") {
            path = stripped.to_string();
        }
        if let Some(stripped) = path.strip_suffix(".txt") {
            path = stripped.to_string();
        }
    }
    if path.is_empty() {
        path = "/".to_string();
    }
    Ok(path)
}

fn raw_path_for(path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    let mut safe = String::new();
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '/' | '-' | '_' | '.') {
            safe.push(ch);
        } else {
            safe.push('_');
        }
    }
    if safe.is_empty() {
        "index.md".to_string()
    } else {
        format!("{safe}.md")
    }
}

fn title_from_markdown(markdown: &str) -> Option<String> {
    markdown.lines().find_map(|line| {
        line.strip_prefix("# ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn extract_version(path: &str) -> Option<String> {
    let re = Regex::new(r"20\d{2}-\d{2}|latest").expect("valid regex");
    re.find(path).map(|m| m.as_str().to_string())
}

fn classify_doc_type(path: &str) -> String {
    if path.contains("/reference") || path.contains("/objects/") || path.contains("/queries/") {
        "reference"
    } else if path.contains("/tutorial") || path.contains("/build/") {
        "tutorial"
    } else if path.contains("/migrate") || path.contains("/migration") {
        "migration"
    } else if path.contains("/guide") || path.contains("/how-to") {
        "how-to"
    } else {
        "explanation"
    }
    .to_string()
}

fn classify_content_class(path: &str) -> String {
    if path.contains("/admin-graphql/") || path.contains("/storefront/") {
        "schema_ref"
    } else if path.contains("/liquid/") {
        "liquid_ref"
    } else if path.contains("/changelog") {
        "changelog"
    } else if path.contains("/api/") {
        "api_ref"
    } else if path.contains("/tutorial") {
        "tutorial"
    } else {
        "guide"
    }
    .to_string()
}

fn classify_api_surface(path: &str) -> Option<String> {
    let surface = if path.contains("/admin-graphql/") {
        "admin_graphql"
    } else if path.contains("/storefront/") {
        "storefront"
    } else if path.contains("/liquid/") {
        "liquid"
    } else if path.contains("/hydrogen/") {
        "hydrogen"
    } else if path.contains("/functions/") {
        "functions"
    } else if path.contains("/polaris") {
        "polaris"
    } else if path.contains("/flow/") {
        "flow"
    } else {
        return None;
    };
    Some(surface.to_string())
}

fn reading_time_min(content: &str) -> i64 {
    let words = content.split_whitespace().count() as i64;
    (words / 220).max(1)
}

fn hex_sha256(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn open_db(paths: &Paths) -> Result<Connection> {
    if let Some(parent) = paths.db.parent() {
        fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(&paths.db)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(conn)
}

fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS schema_meta (
          key TEXT PRIMARY KEY,
          value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS docs (
          path TEXT PRIMARY KEY,
          title TEXT NOT NULL,
          url TEXT NOT NULL,
          version TEXT,
          doc_type TEXT NOT NULL,
          api_surface TEXT,
          content_class TEXT NOT NULL,
          content_sha TEXT NOT NULL,
          last_verified TEXT NOT NULL,
          last_changed TEXT NOT NULL,
          freshness TEXT NOT NULL,
          references_deprecated INTEGER NOT NULL DEFAULT 0,
          deprecated_refs TEXT,
          summary_raw TEXT NOT NULL,
          reading_time_min INTEGER,
          raw_path TEXT NOT NULL,
          hit_count INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_docs_version ON docs(version);
        CREATE INDEX IF NOT EXISTS idx_docs_surface ON docs(api_surface);
        CREATE INDEX IF NOT EXISTS idx_docs_class ON docs(content_class, api_surface);
        CREATE INDEX IF NOT EXISTS idx_docs_freshness ON docs(freshness);
        ",
    )?;
    conn.execute(
        "INSERT INTO schema_meta(key, value) VALUES('schema_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![SCHEMA_VERSION],
    )?;
    Ok(())
}

fn upsert_doc(conn: &Connection, doc: &DocRecord) -> Result<()> {
    conn.execute(
        "
        INSERT INTO docs (
          path, title, url, version, doc_type, api_surface, content_class, content_sha,
          last_verified, last_changed, freshness, summary_raw, reading_time_min, raw_path
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
        ON CONFLICT(path) DO UPDATE SET
          title=excluded.title,
          url=excluded.url,
          version=excluded.version,
          doc_type=excluded.doc_type,
          api_surface=excluded.api_surface,
          content_class=excluded.content_class,
          content_sha=excluded.content_sha,
          last_verified=excluded.last_verified,
          last_changed=CASE
            WHEN docs.content_sha = excluded.content_sha THEN docs.last_changed
            ELSE excluded.last_changed
          END,
          freshness=excluded.freshness,
          summary_raw=excluded.summary_raw,
          reading_time_min=excluded.reading_time_min,
          raw_path=excluded.raw_path
        ",
        params![
            doc.path,
            doc.title,
            doc.url,
            doc.version,
            doc.doc_type,
            doc.api_surface,
            doc.content_class,
            doc.content_sha,
            doc.last_verified,
            doc.last_changed,
            doc.freshness,
            doc.summary_raw,
            doc.reading_time_min,
            doc.raw_path,
        ],
    )?;
    Ok(())
}

fn get_doc(conn: &Connection, path: &str) -> Result<Option<DocRecord>> {
    conn.query_row(
        "SELECT path, title, url, version, doc_type, api_surface, content_class, content_sha,
                last_verified, last_changed, freshness, summary_raw, reading_time_min, raw_path
         FROM docs WHERE path = ?1",
        params![path],
        doc_from_row,
    )
    .optional()
    .map_err(Into::into)
}

fn doc_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DocRecord> {
    Ok(DocRecord {
        path: row.get(0)?,
        title: row.get(1)?,
        url: row.get(2)?,
        version: row.get(3)?,
        doc_type: row.get(4)?,
        api_surface: row.get(5)?,
        content_class: row.get(6)?,
        content_sha: row.get(7)?,
        last_verified: row.get(8)?,
        last_changed: row.get(9)?,
        freshness: row.get(10)?,
        summary_raw: row.get(11)?,
        reading_time_min: row.get(12)?,
        raw_path: row.get(13)?,
    })
}

fn staleness(doc: &DocRecord) -> Staleness {
    let verified = DateTime::parse_from_rfc3339(&doc.last_verified)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    let age_days = (Utc::now() - verified).num_days();
    Staleness {
        age_days,
        freshness: match age_days {
            0..=7 => "fresh",
            8..=30 => "aging",
            _ => "stale",
        }
        .to_string(),
        content_verified_at: doc.last_verified.clone(),
        schema_version: doc.version.clone(),
        references_deprecated: false,
        deprecated_refs: Vec::new(),
        upcoming_changes: Vec::new(),
    }
}

#[derive(Clone, Copy)]
struct SearchFields {
    path: Field,
    title: Field,
    url: Field,
    version: Field,
    api_surface: Field,
    doc_type: Field,
    content: Field,
}

impl SearchFields {
    fn from_schema(schema: &Schema) -> Result<Self> {
        Ok(Self {
            path: schema.get_field("path")?,
            title: schema.get_field("title")?,
            url: schema.get_field("url")?,
            version: schema.get_field("version")?,
            api_surface: schema.get_field("api_surface")?,
            doc_type: schema.get_field("doc_type")?,
            content: schema.get_field("content")?,
        })
    }
}

fn search_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("path", STRING | STORED);
    builder.add_text_field("title", TEXT | STORED);
    builder.add_text_field("url", STRING | STORED);
    builder.add_text_field("version", STRING | STORED);
    builder.add_text_field("api_surface", STRING | STORED);
    builder.add_text_field("doc_type", STRING | STORED);
    builder.add_text_field("content", TEXT);
    builder.build()
}

fn create_or_reset_index(paths: &Paths, schema: Schema, reset: bool) -> Result<Index> {
    if reset && paths.tantivy.exists() {
        fs::remove_dir_all(&paths.tantivy)?;
    }
    fs::create_dir_all(&paths.tantivy)?;
    if paths.tantivy.join("meta.json").exists() {
        Index::open_in_dir(&paths.tantivy).map_err(Into::into)
    } else {
        Index::create_in_dir(&paths.tantivy, schema).map_err(Into::into)
    }
}

fn doc_json_field(doc_json: &str, field: &str) -> Option<String> {
    let value: Value = serde_json::from_str(doc_json).ok()?;
    value.get(field).and_then(|field_value| {
        field_value
            .as_array()
            .and_then(|values| values.first())
            .and_then(Value::as_str)
            .or_else(|| field_value.as_str())
            .map(ToOwned::to_owned)
    })
}

fn escape_query(query: &str) -> String {
    query
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || ch.is_whitespace() {
                ch
            } else {
                ' '
            }
        })
        .collect::<String>()
}

fn to_json_value<T: Serialize>(value: T) -> Value {
    serde_json::to_value(value).unwrap_or_else(|e| json!({ "serialization_error": e.to_string() }))
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn now_iso() -> String {
    Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
