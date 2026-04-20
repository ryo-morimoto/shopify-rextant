# shopify-rextant

`shopify-rextant` is a local-first MCP server for Shopify developer documentation.
It builds a local index of Shopify docs, then serves coding agents source-backed
maps and raw markdown without repeat remote `shopify.dev` lookups.

The server does not synthesize answers, call an LLM, validate user code, or mutate
any Shopify store. It returns source-backed context; the caller decides what to
read next.

## Quickstart

```bash
cargo install --path .        # source install
shopify-rextant build         # first-time index build (2–5 minutes)
shopify-rextant status        # confirm doc_count > 0
```

Register with an MCP client:

```bash
# Claude Code
claude mcp add --transport stdio shopify-rextant -- shopify-rextant serve
```

```toml
# Codex CLI (~/.codex/config.toml)
[mcp_servers.shopify-rextant]
command = "shopify-rextant"
args = ["serve"]
```

## Documentation

- User guide: [`docs/user/install.md`](docs/user/install.md), [`cli.md`](docs/user/cli.md), [`mcp.md`](docs/user/mcp.md), [`config.md`](docs/user/config.md), [`troubleshooting.md`](docs/user/troubleshooting.md)
- Developer guide: [`docs/dev/architecture.md`](docs/dev/architecture.md), [`testing.md`](docs/dev/testing.md), [`release.md`](docs/dev/release.md)

## Capabilities

- MCP stdio server (newline-delimited JSON, also accepts `Content-Length` framing)
- `shopify_map`, `shopify_fetch`, and `shopify_status` tools
- `llms.txt` + `sitemap.xml` discovery with coverage reporting
- Admin GraphQL concept/doc graph from Shopify's public direct proxy
- Changelog freshness and scheduled-change hydration
- Japanese search tokenization via Lindera / IPADIC
- Opt-in on-demand recovery of missing `shopify.dev/docs/**` and `shopify.dev/changelog/**` pages

## Privacy Boundary

Outbound HTTP is limited to official Shopify documentation endpoints used for
indexing and refresh. The server never sends user code, prompts, project files,
MCP client metadata, or telemetry.

## Install From Source

See [`docs/user/install.md`](docs/user/install.md) for the full instructions.

```bash
cargo install --path .
# or
cargo build --release
./target/release/shopify-rextant version
```

## Distribution Channels (v1.0 target)

Planned and not yet enabled. `cargo install --path .` is the canonical path until
each channel lands.

- **crates.io** — `cargo install shopify-rextant`  *(TBD)*
- **Homebrew** — `brew install shopify-rextant`  *(TBD)*
- **GitHub Releases** — prebuilt binaries + checksums  *(TBD)*
- **Nix** — flake output  *(TBD)*

## License

MIT. See [`LICENSE`](LICENSE).
