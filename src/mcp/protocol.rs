use serde_json::{Value, json};

#[cfg(test)]
use anyhow::{Result, anyhow};
#[cfg(test)]
use super::super::util::json::to_json_value;
#[cfg(test)]
use super::super::{
    FetchArgs, MapArgs, Paths, SearchArgs, ToolError, search_docs, shopify_fetch, shopify_map,
    status,
};

pub(crate) fn json_rpc_error(code: i64, message: &str, data: Value) -> Value {
    json!({ "code": code, "message": message, "data": data })
}

pub(crate) fn tool_descriptors() -> Vec<Value> {
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
            "description": "Fetch raw Shopify documentation by local docs path. When local on-demand fetch is enabled, allowed shopify.dev docs/changelog URLs or unindexed canonical paths can be recovered and indexed.",
            "inputSchema": {
                "type": "object",
                "anyOf": [
                    { "required": ["path"] },
                    { "required": ["url"] }
                ],
                "properties": {
                    "path": { "type": "string" },
                    "url": {
                        "type": "string",
                        "description": "Allowed only for https://shopify.dev/docs/** and https://shopify.dev/changelog/**. Requires [index].enable_on_demand_fetch=true in local config."
                    },
                    "anchor": { "type": "string" },
                    "include_code_blocks": { "type": "boolean", "default": true },
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

#[cfg(test)]
pub(crate) async fn handle_mcp_request(paths: &Paths, request: Value) -> Value {
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
        "tools/call" => {
            call_mcp_tool(paths, request.get("params").cloned().unwrap_or(Value::Null)).await
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

#[cfg(test)]
pub(crate) async fn call_mcp_tool(
    paths: &Paths,
    params: Value,
) -> std::result::Result<Value, Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| json_rpc_error(-32602, "Missing tool name", Value::Null))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let value = match name {
        "shopify_status" => status(paths).map(to_json_value).map_err(ToolError::from),
        "shopify_fetch" => {
            let args: Result<FetchArgs> = serde_json::from_value(args)
                .map_err(|e| anyhow!("invalid shopify_fetch args: {e}"));
            match args {
                Ok(args) => shopify_fetch(paths, &args).await.map(to_json_value),
                Err(error) => Err(ToolError::from(error)),
            }
        }
        "shopify_map" => {
            let args: Result<MapArgs> =
                serde_json::from_value(args).map_err(|e| anyhow!("invalid shopify_map args: {e}"));
            args.and_then(|args| shopify_map(paths, &args).map(to_json_value))
                .map_err(ToolError::from)
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
