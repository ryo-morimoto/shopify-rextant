use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

use super::super::mcp_framing::{
    read_message as read_mcp_message, write_json as write_mcp_message,
};
use super::super::search::runtime::SearchRuntime;
use super::super::search::tokenizer::japanese_segmenter;
use super::super::util::json::to_json_value;
use super::super::{
    FetchArgs, MapArgs, Paths, SearchArgs, ToolError, search_docs_with_runtime, shopify_fetch,
    shopify_map_with_runtime, status,
};
use super::protocol::{json_rpc_error, tool_descriptors};
use super::workers::spawn_background_workers;

pub(crate) struct ServerState {
    paths: Paths,
    search_runtime: Mutex<Option<SearchRuntime>>,
    search_warmup: Mutex<Option<std::thread::JoinHandle<Result<()>>>>,
}

impl ServerState {
    pub(crate) fn new(paths: Paths) -> Self {
        Self {
            paths,
            search_runtime: Mutex::new(None),
            search_warmup: Mutex::new(None),
        }
    }

    pub(crate) fn spawn_search_warmup(self: &Arc<Self>) {
        if self
            .search_runtime
            .lock()
            .map(|runtime| runtime.is_some())
            .unwrap_or(false)
        {
            return;
        }
        let mut warmup = match self.search_warmup.lock() {
            Ok(warmup) => warmup,
            Err(_) => {
                eprintln!("search runtime warmup setup error: lock poisoned");
                return;
            }
        };
        if warmup.is_some() {
            return;
        }
        let state = Arc::clone(self);
        let handle = std::thread::spawn(move || state.warm_search_runtime());
        *warmup = Some(handle);
    }

    fn warm_search_runtime(&self) -> Result<()> {
        let runtime = SearchRuntime::open(&self.paths)?;
        let _ = japanese_segmenter()?;
        let mut guard = self
            .search_runtime
            .lock()
            .map_err(|_| anyhow!("search runtime lock poisoned"))?;
        if guard.is_none() {
            *guard = runtime;
        }
        Ok(())
    }

    fn finish_search_warmup(&self) -> Result<()> {
        let handle = self
            .search_warmup
            .lock()
            .map_err(|_| anyhow!("search warmup lock poisoned"))?
            .take();
        if let Some(handle) = handle {
            handle
                .join()
                .map_err(|_| anyhow!("search runtime warmup panicked"))??;
        }
        Ok(())
    }

    pub(crate) async fn handle_mcp_request(self: &Arc<Self>, request: Value) -> Value {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let result = match method {
            "initialize" => {
                self.spawn_search_warmup();
                Ok(json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "shopify-rextant",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }))
            }
            "tools/list" => Ok(json!({ "tools": tool_descriptors() })),
            "tools/call" => {
                self.call_mcp_tool(request.get("params").cloned().unwrap_or(Value::Null))
                    .await
            }
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

    async fn call_mcp_tool(&self, params: Value) -> std::result::Result<Value, Value> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| json_rpc_error(-32602, "Missing tool name", Value::Null))?;
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        let value = match name {
            "shopify_status" => status(&self.paths)
                .map(to_json_value)
                .map_err(ToolError::from),
            "shopify_fetch" => {
                let args: Result<FetchArgs> = serde_json::from_value(args)
                    .map_err(|e| anyhow!("invalid shopify_fetch args: {e}"));
                match args {
                    Ok(args) => match shopify_fetch(&self.paths, &args).await {
                        Ok(response) => self
                            .reload_search_runtime()
                            .map(|()| to_json_value(response))
                            .map_err(ToolError::from),
                        Err(error) => Err(error),
                    },
                    Err(error) => Err(ToolError::from(error)),
                }
            }
            "shopify_map" => {
                let args: Result<MapArgs> = serde_json::from_value(args)
                    .map_err(|e| anyhow!("invalid shopify_map args: {e}"));
                args.and_then(|args| {
                    self.ensure_search_runtime()?;
                    let guard = self
                        .search_runtime
                        .lock()
                        .map_err(|_| anyhow!("search runtime lock poisoned"))?;
                    shopify_map_with_runtime(&self.paths, &args, guard.as_ref()).map(to_json_value)
                })
                .map_err(ToolError::from)
            }
            "shopify_search" => {
                let args: Result<SearchArgs> = serde_json::from_value(args)
                    .map_err(|e| anyhow!("invalid shopify_search args: {e}"));
                args.and_then(|args| {
                    self.ensure_search_runtime()?;
                    let guard = self
                        .search_runtime
                        .lock()
                        .map_err(|_| anyhow!("search runtime lock poisoned"))?;
                    search_docs_with_runtime(
                        &self.paths,
                        guard.as_ref(),
                        &args.query,
                        args.version.as_deref(),
                        args.limit.unwrap_or(10),
                    )
                    .map(to_json_value)
                })
                .map_err(ToolError::from)
            }
            _ => Err(ToolError::from(anyhow!("unknown tool: {name}"))),
        }
        .map_err(|e| e.json_rpc_error())?;

        let text = serde_json::to_string_pretty(&value)
            .map_err(|e| json_rpc_error(-32000, &e.to_string(), Value::Null))?;
        Ok(json!({
            "content": [{ "type": "text", "text": text }],
            "structuredContent": value,
            "isError": false
        }))
    }

    fn ensure_search_runtime(&self) -> Result<()> {
        self.finish_search_warmup()?;
        let mut guard = self
            .search_runtime
            .lock()
            .map_err(|_| anyhow!("search runtime lock poisoned"))?;
        if guard.is_none() && self.paths.tantivy.join("meta.json").exists() {
            *guard = SearchRuntime::open(&self.paths)?;
        }
        Ok(())
    }

    fn reload_search_runtime(&self) -> Result<()> {
        let mut guard = self
            .search_runtime
            .lock()
            .map_err(|_| anyhow!("search runtime lock poisoned"))?;
        *guard = SearchRuntime::open(&self.paths)?;
        Ok(())
    }
}

pub(crate) async fn serve_direct(paths: Paths) -> Result<()> {
    spawn_background_workers(paths.clone());
    let state = Arc::new(ServerState::new(paths));
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();

    while let Some(message) = read_mcp_message(&mut reader)? {
        let request: Value = serde_json::from_slice(&message)?;
        if request.get("id").is_none() {
            continue;
        }
        let response = state.handle_mcp_request(request).await;
        write_mcp_message(&mut writer, &response)?;
    }
    Ok(())
}
