# Contributing

This project is a Rust MCP server for local Shopify developer documentation lookup.
Keep changes source-backed, deterministic, and local-first.

## Development Setup

```bash
cargo build
cargo test
```

Use an isolated data directory while testing indexing behavior:

```bash
SHOPIFY_REXTANT_HOME=/tmp/shopify-rextant-dev cargo run -- build --limit 20
SHOPIFY_REXTANT_HOME=/tmp/shopify-rextant-dev cargo run -- status
```

## Source-First Workflow

Before changing behavior, read the relevant source in this order:

1. `SPEC.md`
2. `src/main.rs`
3. Existing docs under `docs/`
4. Official upstream documentation for any external tool or package behavior

Do not rely on memory for protocol, package, or release behavior when a primary source is
available.

## Validation Expectations

Run this before submitting implementation changes:

```bash
cargo test
```

When touching MCP transport, also verify direct stdio framing:

```bash
cargo build
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  | target/debug/shopify-rextant serve --direct
```

When touching indexing or search coverage, verify a query that previously needed web
fallback, such as optional access scopes or managed access scopes.

## Benchmarking

Release benchmarks use Criterion against a deterministic local fixture. They do not
fetch live Shopify docs during measurement.

Run the stable release-contract benchmarks:

```bash
cargo bench --bench release_contract
```

The benchmark group covers:

- `status`
- `search_docs` for a Product query
- `shopify_map` for the Product concept graph
- `shopify_fetch` for a local Product doc path

## Release Checklist

For a public release candidate:

1. Confirm `Cargo.toml` package version matches the intended tag.
2. Confirm `shopify-rextant --version`, `shopify-rextant version`, and HTTP User-Agent use the same package version.
3. Run `cargo test`.
4. Run `cargo bench --bench release_contract`.
5. Run an MCP direct stdio smoke test.
6. Run `cargo package --list` and check that only intended files are included.
7. Run `cargo package`.
8. Update README install instructions if crates.io, Homebrew, GitHub Releases, or Nix are available.
9. Cut the release tag only after CI, packaging, and security checks pass.

## Git

Use Conventional Commits:

- `feat:`
- `fix:`
- `docs:`
- `test:`
- `refactor:`
- `chore:`

Do not commit local data directories, generated indexes, or the empty `.codex` metadata file.

## Product Boundary

Keep these out of scope unless `SPEC.md` is explicitly changed first:

- Server-side LLM summarization or answer synthesis
- User code upload or telemetry
- Live Shopify store mutation
- Arbitrary URL fetching
- GraphQL or Liquid code validation
