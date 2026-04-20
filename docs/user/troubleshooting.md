# Troubleshooting

## MCP `initialize` hangs

Symptom: the client waits 10+ seconds after `initialize`.

Cause: the client is waiting on `Content-Length` framing while the server is sending a single newline-terminated JSON object (or vice versa).

Action:

- Confirm the client supports newline-delimited JSON (Claude Code and Codex do by default).
- Run the direct smoke test — it must return within a second:

  ```bash
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
    | shopify-rextant serve --direct
  ```

If `--direct` works but the normal `serve` hangs, the shared daemon shim is at fault — use `serve --direct` as a workaround and file an issue.

## `Path not found` on `shopify_fetch`

The path is not in the local index.

- Check coverage: `shopify-rextant status` → `coverage.skipped_count` / `failed_count`.
- Rebuild discovery: `shopify-rextant build` (or `build --force` if the index is stale).
- Recover one URL on demand: enable `enable_on_demand_fetch` in `config.toml`, then `shopify-rextant refresh --url https://shopify.dev/docs/...`. See [config.md](config.md).

## JSON-RPC `-32007` — on-demand fetch disabled

The tool received a `url` (or unmatched path) but the server is configured to stay offline. Opt in:

```toml
[index]
enable_on_demand_fetch = true
```

## JSON-RPC `-32008` — URL outside allowed scope

Only `https://shopify.dev/docs/**` and `https://shopify.dev/changelog/**` are accepted. All other hosts, schemes, and Shopify paths (for example `shopify.dev/themes/...` or `admin.shopify.com/...`) are rejected before any network request.

## Search returns nothing

- `shopify-rextant status` — is `index_built: true`?
- `shopify-rextant search "<query>"` — is the FTS layer hitting anything?
- `meta.coverage_warning` in a `shopify_map` response usually names the remediation (`run shopify_refresh`, out-of-date index, etc.).

## `shopify_map.meta.graph_available = false`

The concept graph is not available for this query — the response still contains FTS candidates but `edges[]` is empty. This is expected whenever the query could not be resolved to an indexed Admin GraphQL concept; follow `query_plan[0].action` (`inspect_status`, `refresh`, …) rather than treating the empty `edges` as a bug.

## Resetting state

- Drop the index but keep config: `shopify-rextant build --force`.
- Wipe everything: delete the home directory (`~/.shopify-rextant/` by default) and rebuild.

## Reporting an issue

Include `shopify-rextant version`, `shopify-rextant status` JSON, and the failing MCP request if any. Never attach internal code or secrets — the server itself never reads them, and the issue tracker is public.
