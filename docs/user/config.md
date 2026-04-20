# Configuration

## Data Directory

Resolved in this order:

1. `--home PATH` CLI flag
2. `SHOPIFY_REXTANT_HOME` env var
3. `~/.shopify-rextant/`

Layout:

```
<home>/
├── config.toml
├── data/
│   ├── index.db        # SQLite (WAL)
│   ├── tantivy/        # FTS index
│   └── raw/            # verbatim markdown
└── logs/
```

## `config.toml`

The file is optional. Missing keys take the defaults shown.

```toml
[index]
# v0.5: recover a missing official docs page on demand
enable_on_demand_fetch = false
```

### `[index].enable_on_demand_fetch`

- **Default:** `false` (no network access beyond `build` / `refresh`)
- **When `true`:** `shopify_fetch` with `url`, or `refresh --url`, may fetch the document from `shopify.dev`
- **Allowed scope:** `https://shopify.dev/docs/**`, `https://shopify.dev/changelog/**` — all other hosts, schemes, and Shopify paths are rejected before any network request
- **MCP behavior when disabled:** JSON-RPC `-32007` with the candidate URL
- **MCP behavior when out of scope:** JSON-RPC `-32008`

The flag is server-side only. There is no per-call MCP argument that lets an agent turn network access on; the user opts in explicitly on their own machine.

## Environment Variables

| var | purpose |
|---|---|
| `SHOPIFY_REXTANT_HOME` | override data directory (see above) |
| `RUST_LOG` | `tracing` filter, e.g. `RUST_LOG=info` |

Logs go to stderr; stdout is MCP protocol only.

## User-Agent

Outbound HTTP uses `shopify-rextant/<package-version>` — same string as `shopify-rextant --version`.
