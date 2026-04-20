# Install

`shopify-rextant` is currently distributed as source. Binary release channels are listed at the end of this page as planned but not yet available.

## From Source

Requires Rust 1.85+ (edition 2024) and a C toolchain for the bundled SQLite build.

```bash
git clone https://github.com/ryo-morimoto/shopify-rextant.git
cd shopify-rextant
cargo install --path .
```

Or build a local release binary without installing:

```bash
cargo build --release
./target/release/shopify-rextant version
```

## Verify

```bash
shopify-rextant version
shopify-rextant status
```

`status` prints `index_built: false` until you run `shopify-rextant build`. See [cli.md](cli.md).

## Data Directory

Default: `~/.shopify-rextant/`. Override with `SHOPIFY_REXTANT_HOME=/some/path` or `--home /some/path`. See [config.md](config.md).

## Planned Distribution Channels (v1.0)

The following channels are planned for the v1.0 release. They are not available yet.

- **crates.io** — `cargo install shopify-rextant`  *(TBD)*
- **Homebrew** — `brew install shopify-rextant`  *(TBD)*
- **GitHub Releases** — prebuilt binaries + checksums  *(TBD)*
- **Nix** — flake output  *(TBD)*

Until those land, build from source as above.
