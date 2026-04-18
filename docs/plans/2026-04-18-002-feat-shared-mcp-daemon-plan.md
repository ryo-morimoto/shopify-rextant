---
title: "feat: share MCP runtime through local daemon shim"
type: feat
status: implemented
date: 2026-04-18
---

# feat: share MCP runtime through local daemon shim

## 0. Goal Model

Project-level MCP registration and global MCP registration can both launch `shopify-rextant serve`. Today each launch creates a separate process, so each process can pay its own Tantivy reader and Japanese tokenizer warmup cost. The target is to keep the MCP stdio contract unchanged for clients while sharing one warmed local runtime per compatible project identity.

### Goals

- Keep `shopify-rextant serve` as the normal MCP entrypoint used by Codex and other MCP clients.
- Make `serve` act as a lightweight stdio shim that connects to a local daemon when possible.
- Reuse the same daemon when multiple `serve` shims use the same project identity.
- Keep one warmed `ServerState`, Tantivy reader, tokenizer cache, and background worker set in the daemon.
- Preserve local-first behavior: no remote daemon, no telemetry, no default network listener.
- Preserve stdout discipline: only MCP JSON-RPC messages go to stdout; logs stay on stderr.
- Keep current direct stdio server mode available for tests, debugging, and compatibility.

### Non-goals

- Do not introduce a remote shared service.
- Do not expose an HTTP API by default.
- Do not share state across incompatible `--home`, config, schema, package version, or index layout.
- Do not add server-side LLM summarization or answer synthesis.
- Do not solve Windows named pipe support in the first implementation pass.

### Minimum Done

- `serve` defaults to shim mode.
- A direct mode exists, for example `serve --direct`, with the current single-process stdio behavior.
- A daemon process can be started, discovered, and reused through a Unix domain socket.
- A deterministic daemon identity prevents unsafe cross-project sharing.
- Startup uses a lock file so concurrent shims start at most one daemon.
- Stale pid, stale lock, and stale socket cases recover without manual cleanup.
- Multiple shim clients can call MCP tools concurrently through one daemon.
- Search runtime warmup happens in the daemon, not in each shim.
- Existing MCP tests pass, plus new daemon/shim tests.

## 1. Source Findings

Source-first notes from the current repository:

- `src/main.rs` contains the current CLI and `Serve` command dispatch. The current `serve(paths).await` path owns the stdio MCP loop.
- The current stdio server reads JSON-RPC messages from stdin and writes responses to stdout. This must remain available for direct mode.
- `ServerState` currently owns in-process runtime state, including the warmed search runtime and background warmup handle.
- `SPEC.md` documents MCP client registration with `args = ["serve"]`, so changing the default command name would break the documented user path.
- `SPEC.md` and `AGENTS.md` both constrain the product boundary to local-first, zero telemetry, raw source-backed docs, and no LLM synthesis.

## 2. Requirements Trace

| ID | Requirement | Implementation hook | Verification |
| --- | --- | --- | --- |
| R1 | Existing `serve` remains the MCP entrypoint | Keep `Serve` subcommand; make default shim mode | Existing config examples still work |
| R2 | Same compatible project reuses one runtime | Daemon identity hash and socket path | Two `serve` shims share one daemon pid |
| R3 | Incompatible homes/configs do not share | Include canonical home, schema, config hash, and version in identity | Different `--home` creates different socket |
| R4 | stdout remains protocol-only | Shim proxies daemon responses to stdout; logs stderr only | Smoke test with strict JSON lines |
| R5 | Running daemon avoids search cold load | Daemon owns warmed `SearchRuntime` | Second shim search avoids Tantivy/IPADIC load |
| R6 | Runtime updates are coherent | Daemon owns on-demand fetch and reload path | Client A fetch, client B search sees update |
| R7 | Stale daemon artifacts recover | Lock, pid, socket health checks | Tests for stale pid/socket |
| R8 | Test/debug direct path remains available | `serve --direct` or hidden direct command | Existing stdio tests run direct mode |

## 3. Architecture

```text
MCP client
  |
  | stdio JSON-RPC
  v
shopify-rextant serve        (shim process, cheap)
  |
  | Unix domain socket JSON-RPC proxy
  v
shopify-rextant daemon       (shared local process)
  |
  | owns ServerState, Tantivy reader, tokenizer, workers
  v
local docs cache and index
```

The shim should be intentionally thin:

- read MCP JSON-RPC frames from stdin,
- connect to the daemon socket,
- forward requests to the daemon,
- forward daemon responses to stdout,
- keep logs and startup diagnostics on stderr.

The daemon owns the existing MCP handler logic. Each socket connection represents one MCP client session. JSON-RPC ids are scoped to that connection, so the daemon does not need to globally rewrite ids.

## 4. Design Decisions

### D1. Keep `serve` as the user-facing command

`serve` should become shim mode by default because MCP clients already call it. A direct mode should be added for tests and troubleshooting.

Rejected alternative: introduce a separate `serve-shared` command and ask users to update MCP config. That keeps implementation simpler but fails the goal of project-level and global registrations automatically sharing runtime.

Trade-off: default shim mode adds process management complexity, but keeps client configuration stable and solves the cold-load issue for existing registrations.

### D2. Use Unix domain sockets first

Use a Unix domain socket under the configured home or runtime directory. This stays local-only and avoids exposing network ports.

Rejected alternative: TCP localhost. It is easier to debug and more portable, but increases accidental exposure risk and requires port allocation policy.

Trade-off: Unix sockets are excellent for Linux/macOS but defer Windows support.

### D3. Identity is conservative

Daemon identity should include:

- canonical `--home`,
- package version,
- index schema version,
- relevant config file hash,
- binary path or binary mtime in development builds if needed.

This prevents a project-level MCP from accidentally attaching to a daemon with a different index layout or config.

Rejected alternative: one daemon per user. That maximizes reuse but risks cross-project contamination.

Trade-off: conservative identity may start more daemons than strictly necessary, but avoids incorrect search results and update races.

### D4. Lock before spawning daemon

When no healthy socket exists, a shim should acquire a lock file, re-check socket health, then spawn the daemon if still absent. Other shims wait for readiness.

Rejected alternative: let all shims spawn and rely on bind failure. That is simpler but creates noisy races and fragile startup behavior.

Trade-off: lock handling introduces stale-lock recovery requirements, but gives deterministic behavior under parallel starts.

### D5. Daemon owns warmup and workers

Warmup should happen in the daemon after daemon start or after the first daemon-side `initialize`. Shims must not initialize Tantivy or Lindera.

Rejected alternative: shims prewarm before proxying. That recreates the current duplication problem.

Trade-off: first ever daemon start can still pay setup cost, but subsequent project/global shims avoid it.

### D6. Idle shutdown is required

The daemon should exit after a configurable idle period, for example 10 minutes with a shorter duration in tests.

Rejected alternative: daemon runs forever. That gives best latency but makes lifecycle and upgrades harder.

Trade-off: idle shutdown means occasional cold starts after inactivity, but avoids long-lived orphan processes.

## 5. Implementation Units

### Unit 1: Preserve direct stdio server

Files:

- `src/main.rs`

Work:

- Rename the current `serve(paths).await` implementation internally to a direct server function.
- Add a `serve --direct` path that calls the direct server.
- Keep existing newline-delimited JSON and Content-Length compatibility behavior.
- Keep all logs on stderr.

Tests:

- Existing initialize/tools/list/tool-call smoke tests pass through direct mode.
- Transport framing tests still cover newline JSON and Content-Length.

### Unit 2: Add daemon identity and paths

Files:

- `src/main.rs`

Work:

- Add `DaemonIdentity`.
- Add `DaemonPaths` for socket, lock, pid, and log/stderr policy if needed.
- Canonicalize `--home`.
- Hash identity into a bounded filename to avoid Unix socket path length problems.

Tests:

- Same canonical home produces the same identity.
- Different home produces a different identity.
- Different schema/config/version produces a different identity.
- Socket path length is bounded.

### Unit 3: Add daemon process mode

Files:

- `src/main.rs`

Work:

- Add an internal `daemon` command or hidden subcommand.
- Bind a Unix domain socket for the identity.
- For each client connection, run the existing MCP handler against shared daemon state.
- Keep per-connection JSON-RPC ordering and response routing isolated.
- Initialize shared `ServerState` once per daemon.

Tests:

- Connect to socket and run `initialize`.
- Connect to socket and run `tools/list`.
- Connect to socket and run `shopify_search`.
- Two concurrent socket clients both succeed.

### Unit 4: Implement shim connect/start/proxy

Files:

- `src/main.rs`

Work:

- Make default `serve` compute daemon identity and socket path.
- If socket is healthy, connect and proxy stdin/stdout.
- If socket is missing or unhealthy, acquire lock and spawn daemon.
- Wait for daemon readiness with bounded retry/backoff.
- Forward stdin EOF and daemon disconnects cleanly.

Tests:

- Starting `serve` with no daemon starts one daemon.
- Starting a second `serve` reuses the same daemon.
- Simultaneous `serve` starts create only one daemon.
- Dead socket is removed and replaced.

### Unit 5: Add lock, pid, and lifecycle handling

Files:

- `src/main.rs`

Work:

- Write pid metadata after daemon start.
- Verify pid and socket health before reusing.
- Recover stale pid files and stale socket files.
- Add idle timeout shutdown when no clients are connected.
- Make timeout configurable for tests.

Tests:

- Stale pid file does not block startup.
- Stale socket file does not block startup.
- Daemon exits after idle timeout.
- Active client prevents idle shutdown.

### Unit 6: Make runtime updates coherent

Files:

- `src/main.rs`

Work:

- Ensure warmed `SearchRuntime` lives only in daemon `ServerState`.
- Ensure background version/watch/fetch workers, if enabled, are daemon-side only.
- When on-demand fetch updates the index, reload daemon runtime once.
- Ensure all clients attached to the daemon observe the same updated index.

Tests:

- Client A performs on-demand fetch/update.
- Client B searches and can observe the updated document.
- Two clients do not trigger duplicate index reloads.

### Unit 7: Update docs and specification

Files:

- `SPEC.md`
- `docs/plans/2026-04-18-002-feat-shared-mcp-daemon-plan.md`

Work:

- Document `serve` shim behavior.
- Document `serve --direct`.
- Document daemon identity boundaries.
- Add troubleshooting notes for stale sockets and direct-mode fallback.
- Add benchmark expectations for warmed daemon path.

Verification:

- MCP registration examples still use `serve`.
- Direct mode is documented for local smoke tests.

## 6. Sequencing

1. Refactor current direct `serve` without behavior change.
2. Add identity/path helpers and unit tests.
3. Add daemon socket mode using the existing MCP handler.
4. Add shim connect/start/proxy behavior.
5. Add lock, pid, stale cleanup, and idle shutdown.
6. Move runtime warmup ownership fully into daemon path.
7. Verify on-demand fetch/update coherence across clients.
8. Update `SPEC.md`.
9. Run benchmarks against direct, cold shim, and warm daemon paths.

## 7. Benchmark Plan

Run benchmarks against a populated local docs cache:

- Direct mode baseline: `serve --direct` initialize, tools/list, status, search.
- Cold shim: no daemon running, first `serve` starts daemon.
- Warm shim: daemon already running, second `serve` runs initialize, tools/list, status, search.
- Shared registration scenario: global shim warms daemon, project-level shim attaches to the same identity.
- Japanese query scenario: daemon warmed before first Japanese search from second shim.
- Parallel startup scenario: multiple shims launched at once.

Expected target after daemon is already running:

- `initialize + tools/list + shopify_status` P50 under 20ms.
- ASCII search from second shim avoids Tantivy open cost.
- Japanese search from second shim avoids IPADIC tokenizer construction cost.
- Only one daemon process exists for the same identity.

## 8. Risks

- Lock implementation can deadlock or leave stale locks if not tested under crash-like paths.
- Unix socket path length can break when `--home` is deeply nested unless the filename is hashed.
- Over-broad identity could share incompatible indexes.
- Over-narrow identity could reduce reuse and fail the project/global sharing goal.
- Any stdout logging from shim or daemon breaks MCP clients.
- Orphan daemon processes can survive upgrades unless idle shutdown and pid checks are robust.
- Direct tests may accidentally exercise shim mode unless test helpers are updated intentionally.

## 9. Acceptance Criteria

- Two `shopify-rextant serve` processes with the same identity share one daemon process.
- Different `--home` values do not share a daemon.
- A project-level MCP launch can attach to an already warmed compatible daemon.
- Search no longer pays Tantivy/IPADIC warmup per shim process.
- On-demand fetch/update behavior is visible across clients attached to the same daemon.
- Existing direct stdio behavior is available through direct mode.
- `cargo test` passes.
- `cargo build --release` passes.
- MCP smoke tests pass for `initialize`, `tools/list`, `shopify_status`, and `shopify_search`.

## 10. Implementation Notes

Implemented in `src/main.rs`:

- `serve` now defaults to shim mode and connects to a local Unix domain socket daemon.
- `serve --direct` preserves the previous single-process stdio MCP server.
- A hidden `daemon` subcommand owns shared `ServerState`, search warmup, and background workers.
- Daemon identity includes canonical home, package version, index schema version, and `config.toml` hash.
- Socket, lock, and pid artifacts are hash-named under `/tmp/shopify-rextant-daemons/`.
- Startup uses a lock file, bounded readiness polling, stale socket cleanup, stale pid cleanup, and stale lock recovery.
- Daemon exits after an idle timeout; `SHOPIFY_REXTANT_DAEMON_IDLE_SECS` can shorten it for local tests.
- `SPEC.md` documents shim behavior, direct mode, identity boundaries, and stale artifact recovery.
