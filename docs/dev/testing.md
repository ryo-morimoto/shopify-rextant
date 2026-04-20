# Testing

All validation is local-first and offline. Tests never hit `shopify.dev`.

## Unit & Integration

```bash
cargo test
```

Tests live under `src/tests.rs` and are organized by domain. They use fixture sources (`TextSource`) instead of real HTTP to keep results deterministic and cache-friendly.

Before committing any behavioral change:

```bash
cargo fmt --check
cargo test
```

## MCP Transport Smoke

Any change under `src/mcp/`, `src/mcp_framing.rs`, or the `serve` path must pass this on `target/debug/` and `target/release/`:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  | target/debug/shopify-rextant serve --direct
```

The response must arrive within a second, contain `"result"`, and include `"name":"shopify-rextant"`. The CI gate runs the same check on the release binary.

Also verify, when touching transport:

- `tools/list` returns `shopify_map`, `shopify_fetch`, and `shopify_status`.
- `tools/call` for `shopify_status` succeeds and returns valid JSON.

## Search & Coverage

When changing indexing, discovery, classification, or Tantivy schema, run at least one query that previously relied on web fallback (for example `optional access scopes` / `managed access scopes`) and confirm the result lands via the local index.

## Benchmarks

Release benchmarks use Criterion against local fixtures. They never fetch from `shopify.dev`.

Compile-only gate (fast, runs in CI):

```bash
cargo bench --bench release_contract -- --test
```

Full measurement (run locally when release timing numbers matter):

```bash
cargo bench --bench release_contract
```

Measured groups: `status`, `search_docs` (Product query), `shopify_map` (Product concept), `shopify_fetch` (Product doc path).

## Isolated Data Directory

Always test indexing behavior against a throwaway home so real user data stays intact:

```bash
SHOPIFY_REXTANT_HOME=/tmp/shopify-rextant-dev cargo run -- build --limit 20
SHOPIFY_REXTANT_HOME=/tmp/shopify-rextant-dev cargo run -- status
```

## Package Verification

```bash
cargo package --list
cargo package
```

Run after touching `Cargo.toml`, `include`, or any file that affects publishable contents.
