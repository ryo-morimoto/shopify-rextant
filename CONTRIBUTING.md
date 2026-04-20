# Contributing

`shopify-rextant` is a Rust MCP server for local Shopify developer documentation.
Keep changes source-backed, deterministic, and local-first.

## Setup

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

Read before changing behavior:

1. [`docs/dev/architecture.md`](docs/dev/architecture.md)
2. The module under `src/` that implements the behavior.
3. Official upstream documentation for any external tool or package.

Do not rely on memory for protocol, package, or release behavior when a primary source is available.

## Validation

Before submitting a PR:

```bash
cargo fmt --check
cargo test
```

See [`docs/dev/testing.md`](docs/dev/testing.md) for the full test matrix (MCP smoke, benchmarks, package verification).

## Release

See [`docs/dev/release.md`](docs/dev/release.md). CI mirrors the local release gate.

## Git

Use Conventional Commits: `feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `chore:`.

Do not commit local data directories, generated indexes, or the empty `.codex` metadata file.

## Product Boundary

Keep these out of scope unless [`docs/dev/architecture.md`](docs/dev/architecture.md) is changed first:

- Server-side LLM summarization or answer synthesis
- User code upload or telemetry
- Live Shopify store mutation
- Arbitrary URL fetching
- GraphQL or Liquid code validation
