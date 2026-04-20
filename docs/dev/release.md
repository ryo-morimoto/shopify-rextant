# Release

This checklist mirrors the CI gate in `.github/workflows/ci.yml`. CI is the authority — if a local step and CI disagree, fix CI first and update this file.

## Pre-Release Checklist

1. `Cargo.toml` `package.version` matches the intended tag.
2. `shopify-rextant --version`, `shopify-rextant version`, and the outbound `User-Agent` all derive from `CARGO_PKG_VERSION` — no manual edits needed, just confirm.
3. `cargo fmt --check`
4. `cargo test`
5. `cargo bench --bench release_contract -- --test` (compile gate)
6. `cargo bench --bench release_contract` — run before cutting a tag when fresh timing numbers are needed.
7. `cargo package --list` — confirm only intended files are included. `Cargo.toml` `include` is `/LICENSE`, `/README.md`, `/benches`, `/src`.
8. `cargo package`
9. `cargo build --release`
10. MCP initialize smoke on the release binary:

    ```bash
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"ci-smoke","version":"0"}}}' \
      | target/release/shopify-rextant serve --direct
    ```

    Response must contain `"result"` and `"name":"shopify-rextant"`.
11. Update [`docs/user/install.md`](../user/install.md) if any distribution channel (crates.io / Homebrew / GitHub Releases / Nix) becomes available.
12. Cut the release tag only after CI is green and `cargo package` verifies.

## CI Gate (`release-gate` job)

`.github/workflows/ci.yml` runs on pull requests and all branches. Steps in order:

1. `cargo fmt --check`
2. `cargo test`
3. `cargo bench --bench release_contract -- --test`
4. `cargo package --list`
5. `cargo package`
6. `cargo build --release`
7. MCP initialize smoke (grep for `"result"` and `"name":"shopify-rextant"`).

Steps 1–7 must all pass before merging to `main`.

## Distribution Channels (v1.0 target)

Planned and not yet enabled:

- **crates.io** — requires owner setup and `cargo publish`.
- **Homebrew** — formula in a tap, referencing a GitHub Releases artifact + SHA256.
- **GitHub Releases** — prebuilt Linux/macOS binaries with checksums, signed notes.
- **Nix** — flake output exposing the binary.

Each channel ships when its verification (checksum, smoke test, reproducible build where applicable) is wired into CI.

## Commit Conventions

Conventional Commits: `feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `chore:`.

Keep `.codex` out of commits unless it becomes intentional project configuration. Do not revert unrelated user changes.
