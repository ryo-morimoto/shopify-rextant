# CLI Reference

All commands accept a global `--home PATH` (or `SHOPIFY_REXTANT_HOME` env) to isolate the data directory.

```
shopify-rextant [--home PATH] <command>
```

## `build`

Build the local index from `shopify.dev`.

```bash
shopify-rextant build
shopify-rextant build --force       # drop existing index first
shopify-rextant build --limit 20    # take at most N docs (debugging)
```

First build downloads `llms.txt` + `sitemap.xml`, fetches raw markdown, and populates SQLite + Tantivy. Expect 2–5 minutes; subsequent builds use conditional GET.

## `serve`

Start the MCP stdio server. This is what MCP clients (Claude Code, Codex) run.

```bash
shopify-rextant serve            # default: shared daemon shim
shopify-rextant serve --direct   # no shim, useful for transport debugging
```

## `status`

Print index state as JSON (same payload as the `shopify_status` MCP tool).

```bash
shopify-rextant status
```

## `search`

Run the FTS layer directly. Debugging aid; agents should prefer `shopify_map` over calling this.

```bash
shopify-rextant search "Product" --version 2026-04 --limit 5
```

## `show`

Print the raw markdown of one indexed document.

```bash
shopify-rextant show /docs/api/admin-graphql/2026-04/objects/Product
shopify-rextant show /docs/apps/build/access-scopes --anchor managed-access-scopes
shopify-rextant show /docs/... --max-chars 8000
shopify-rextant show /docs/... --include-code-blocks false
```

`--anchor` returns only the section under the matching `h1`/`h2`/`h3` slug.

## `refresh`

Re-verify documents against upstream.

```bash
shopify-rextant refresh                          # sweep aging / stale docs
shopify-rextant refresh /docs/apps/build/...     # refresh one indexed path
shopify-rextant refresh --url https://shopify.dev/docs/apps/build/access-scopes
```

The `--url` form is on-demand recovery for a missing official docs page. It requires `enable_on_demand_fetch = true` and only accepts `shopify.dev/docs/**` or `shopify.dev/changelog/**` URLs. See [config.md](config.md) and [mcp.md](mcp.md).

## `coverage repair`

Retry rows in `coverage_reports` with `status = "failed"` whose URL is in allowed scope.

```bash
shopify-rextant coverage repair
```

Uses the same on-demand policy gate as `refresh --url`.

## `version`

```bash
shopify-rextant version
shopify-rextant --version
```

Both print the package version used by the binary and its `User-Agent` header.
