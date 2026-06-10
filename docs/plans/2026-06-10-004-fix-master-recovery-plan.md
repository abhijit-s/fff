# Fix: master-mode not recovered after the fff-engine master process dies

Date: 2026-06-10
Crate: fff-mcp

## Problem

The fff-mcp server is a long-lived stdio process. With multiple roots (a
`[mcp]` section in `config.toml`) it talks to a singleton `fff-engine`
**master** that routes per-root queries to workers. If the master dies while
the MCP server keeps running, every subsequent `base_path`-targeted (multi-root)
tool call fails permanently with:

    MCP error -32602: multi-root requires master mode; start fff-engine in master mode or omit the base_path argument

Calls that omit `base_path` still work (the pool reconnects), but they target
the default root only — multi-root routing stays broken until the MCP server is
restarted.

## Root cause

`FffServer::resolve_route` (`crates/fff-mcp/src/server.rs`) gates every
non-default `base_path` call on `master_mode_available()`
(`crates/fff-mcp/src/server.rs`). That helper is a **passive liveness probe**:

```rust
fn master_mode_available() -> bool {
    let sock = fff_ipc::master_socket_path();
    if !sock.exists() { return false; }
    UnixStream::connect(&sock).is_ok()
}
```

When the master dies its socket disappears (and idle workers may already have
exited via `idle_ttl`), so the probe returns `false` and `resolve_route`
returns the error **before** the call ever reaches `dispatch` →
`ConnectionPool::search_with_retry` → `EngineClient::search_with_recovery` →
`recovery::respawn`. The recovery machinery — which already respawns the master
via `ensure_master_running` inside `EngineClient::connect` — is never invoked.

In short: the gate observes the master is dead but never tries to bring it back,
even though the rest of the system can.

## Fix (minimal)

Make the gate *active*: instead of only probing the existing socket, attempt to
ensure the master is running (spawn/reattach) and report whether that succeeded.

1. `crates/fff-mcp/src/client.rs`: expose the existing private
   `ensure_master_running()` as a public `EngineClient::ensure_master()` thin
   wrapper (no behaviour change — it already spawns the master if absent and
   waits for the socket).
2. `crates/fff-mcp/src/server.rs`: change `master_mode_available()` to call
   `client::EngineClient::ensure_master().is_ok()`. A dead master is now
   respawned on the spot; the call proceeds to `dispatch`, and the pool's
   `search_with_retry` handshakes against the fresh master.

This keeps the singleton-vs-master decision unchanged for the happy path and
reuses the proven spawn/race-safe `ensure_master_running` (O_CREAT|O_EXCL
lockfile, `wait_for_socket`).

## Blast radius

- Only the multi-root gating path changes. Default-root calls (which never hit
  the gate) are unaffected.
- `ensure_master_running` is already what `EngineClient::connect` runs on every
  connect, so spawning from the gate is the same code that already runs at MCP
  startup — no new spawn semantics.
- Top-level Rust/Lua/C/Bun APIs unchanged. The new `ensure_master` is an
  internal `fff_mcp` method, not part of any external API.

## Locking

`resolve_route` holds **no** Mutex/RwLock — `registry` is a read-only `Arc`.
`ensure_master_running` does filesystem work, a process spawn, and a
`wait_for_socket` poll, but takes no in-process lock. Therefore the fix does
**not** hold any Mutex/RwLock across a spawn, IPC round-trip, or `.await`.
(`resolve_route` is a sync method called before any pool lock is taken.)

The only added cost: a `base_path`-targeted call when the master is dead now
pays one spawn + socket-wait (bounded by `SPAWN_TIMEOUT` = 10s) inside the gate.
On the happy path (master alive) `ensure_master_running` returns via its fast
path after a single successful `UnixStream::connect`, same cost as the old probe.

## Test approach

Add `u7_7_master_respawns_after_death_via_gate` to
`crates/fff-mcp/tests/integration.rs`:

1. Spawn master, connect a pool client for an explicit root, issue a FindFiles —
   expect `SearchResults`.
2. Kill the master (and let its workers exit), remove the master socket.
3. Assert `EngineClient::ensure_master()` succeeds (respawns), then issue the
   FindFiles again through the pool and expect `SearchResults` — proving
   multi-root recovery without restarting the harness.

This mirrors the existing `pool_find_files_with_explicit_root_routes_to_worker`
and `u7_2_connect_spawns_master_if_not_running` patterns.

## Verification

- `cargo build --no-default-features -p fff-mcp -p fff-engine -p fff-ipc`
- `cargo test --no-default-features -p fff-mcp -p fff-engine`
- `cargo clippy --no-default-features -p fff-mcp -p fff-engine --tests`
- `cargo fmt -p fff-mcp -p fff-engine`
