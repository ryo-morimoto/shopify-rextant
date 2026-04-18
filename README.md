# shopify-rextant

`shopify-rextant` is a local-first MCP server for Shopify developer documentation.
It builds a local index of Shopify docs, then gives coding agents source-backed maps
and raw document fetches without repeated remote `shopify.dev` lookups.

The server does not synthesize answers, call an LLM, validate user code, or mutate a
Shopify store. It returns source-backed document and concept context so the caller can
decide what to read next.

## Current Status

This repository is at the v0.5.0 implementation stage. The public distribution path is
still being prepared, so source checkout installation is the canonical path until the
release workflow and package channels are in place.

Implemented release-candidate capabilities:

- MCP stdio server with newline-delimited JSON and Content-Length framing support
- `shopify_map`, `shopify_fetch`, and `shopify_status` MCP tools
- `llms.txt` plus `sitemap.xml` discovery and coverage reporting
- Admin GraphQL concept/doc graph foundation from Shopify's public direct proxy
- Changelog freshness and scheduled-change hydration
- Japanese search tokenization through Lindera/IPADIC
- On-demand official docs recovery for `shopify.dev/docs/**` and `shopify.dev/changelog/**`

Still pending for public release:

- GitHub release workflow and checksums
- Homebrew and Nix install paths
- Final public install docs
- Security and performance release gates

## Install From Source

```bash
cargo install --path .
```

Or build a local release binary:

```bash
cargo build --release
./target/release/shopify-rextant version
```

After public packaging is prepared, the intended crates.io command is:

```bash
cargo install shopify-rextant
```

## Build The Local Index

```bash
shopify-rextant build
```

Use a separate home directory for experiments:

```bash
SHOPIFY_REXTANT_HOME=/tmp/shopify-rextant-e2e shopify-rextant build --limit 20
```

## Register With MCP Clients

Claude Code:

```bash
claude mcp add --transport stdio shopify-rextant -- shopify-rextant serve
```

Codex CLI config:

```toml
[mcp_servers.shopify-rextant]
command = "shopify-rextant"
args = ["serve"]
```

For transport debugging, bypass the shared daemon shim:

```bash
shopify-rextant serve --direct
```

## CLI Usage

```bash
shopify-rextant status
shopify-rextant search "Product" --version 2026-04 --limit 5
shopify-rextant show /docs/api/admin-graphql/2026-04/objects/Product
shopify-rextant show /docs/apps/build/access-scopes --anchor managed-access-scopes
shopify-rextant refresh
shopify-rextant coverage repair
```

## On-Demand Fetch

On-demand network recovery is disabled by default. Enable it explicitly in
`~/.shopify-rextant/config.toml`:

```toml
[index]
enable_on_demand_fetch = true
```

Then recover a known official Shopify docs URL:

```bash
shopify-rextant refresh --url https://shopify.dev/docs/apps/build/access-scopes
```

Allowed scopes are only:

- `https://shopify.dev/docs/**`
- `https://shopify.dev/changelog/**`

Other hosts, schemes, and Shopify paths are rejected before any network request.

## Privacy Boundary

External requests are limited to official Shopify documentation endpoints used for
indexing and refresh. `shopify-rextant` does not send user code, user prompts,
private project files, MCP client metadata, or telemetry.

## Validation

Before releasing or opening a PR, run:

```bash
cargo test
cargo bench --bench release_contract
```

When changing MCP transport, also run a direct newline-delimited JSON smoke test:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  | target/debug/shopify-rextant serve --direct
```

## Design

The implementation contract and roadmap live in `SPEC.md`. The key product boundary is:
local-first source retrieval, zero telemetry, no LLM synthesis, and no live Shopify store
mutation.
